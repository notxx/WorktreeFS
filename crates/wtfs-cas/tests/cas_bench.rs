//! CAS core 基准（M17）：manifest 加载、逐文件 stat、按需 hydrate、索引种子
//! 两条路的耗时——C 方案核心路径的数字底稿（挂载在 M18/B180100）。
//!
//! 以 `#[ignore]` 集成测试实现（与 m0_benchmark 同一约定），markdown 报告
//! 打印在 `=== bench report ===` ... `=== end ===` 之间。
//!
//! ```bat
//! cargo test --release -p wtfs-cas --test cas_bench -- --ignored --nocapture
//! ```
//!
//! 参数（env）：`WTFS_BENCH_REPO=<现有仓库路径>`（`git clone --local` 快照，
//! 4,874 文件夹具即用它指向真实仓库）；缺省在 temp 内生成 4,874 文件合成
//! 仓库（每文件内容唯一，blob 不去重）。`WTFS_BENCH_ITERS=<n>`（缺省 3，
//! 取中位数）；`WTFS_BENCH_HYDRATE_N=<n>`（缺省 500）。

#![cfg(windows)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use wtfs_cas::{
    CasView, CasViewConfig, CatFile, DEFAULT_VOLUME_SERIAL, FIXED_STAT_EPOCH_UNIX_NS,
    HydrationCache, Manifest, add_worktree_no_checkout, clear_skip_worktree, seed_skip_worktree,
    seed_stat_aligned,
};

const REPORT_BEGIN: &str = "=== bench report ===";
const REPORT_END: &str = "=== end ===";
const SYNTHETIC_FILES: usize = 4874;

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if values.len() % 2 == 1 {
        values[values.len() / 2]
    } else {
        (values[values.len() / 2 - 1] + values[values.len() / 2]) / 2.0
    }
}

fn fmt_ms(v: f64) -> String {
    format!("{v:.1} ms")
}

fn fmt_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
}

/// 合成仓库：`dirNN/file_<i>.txt` × 4,874，内容唯一（blob 不去重）。
fn build_synthetic_repo(base: &Path) -> (PathBuf, usize) {
    let repo = base.join("synthetic");
    std::fs::create_dir_all(&repo).unwrap();
    common::assert_git(&repo, &["init", "-b", "master"]);
    common::assert_git(&repo, &["config", "core.autocrlf", "false"]);
    let pad = "0123456789".repeat(10);
    for i in 0..SYNTHETIC_FILES {
        let dir = repo.join(format!("dir{:02}", i / 200));
        if i % 200 == 0 {
            std::fs::create_dir_all(&dir).unwrap();
        }
        std::fs::write(
            dir.join(format!("file_{i}.txt")),
            format!("content of file {i} {pad}\n"),
        )
        .unwrap();
    }
    common::assert_git(&repo, &["add", "-A"]);
    common::assert_git(&repo, &["commit", "-m", "synthetic fixture", "-q"]);
    (repo, SYNTHETIC_FILES)
}

fn prepare_source(base: &Path) -> (PathBuf, usize) {
    if let Ok(existing) = std::env::var("WTFS_BENCH_REPO") {
        assert!(
            Path::new(&existing).is_dir(),
            "WTFS_BENCH_REPO not found: {existing}"
        );
        let repo = base.join("source");
        common::assert_git(
            base,
            &["clone", "--local", "-q", &existing, &repo.to_string_lossy()],
        );
        let tracked = common::assert_git(&repo, &["ls-files"]).lines().count();
        (repo, tracked)
    } else {
        let (repo, files) = build_synthetic_repo(base);
        (repo, files)
    }
}

#[test]
#[ignore = "core benchmark; prints a markdown report with --nocapture"]
fn cas_core_benchmark() {
    let iters: usize = std::env::var("WTFS_BENCH_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(3);
    let hydrate_n: usize = std::env::var("WTFS_BENCH_HYDRATE_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(500);

    let base = tempfile::tempdir().unwrap();
    let t_setup = Instant::now();
    let (repo, tracked) = prepare_source(base.path());
    let setup_s = t_setup.elapsed().as_secs_f64();
    let commit = common::assert_git(&repo, &["rev-parse", "HEAD"])
        .trim()
        .to_string();

    // ---- 1. manifest 加载 ------------------------------------------------
    let mut load_ms = Vec::new();
    let mut files = 0usize;
    let mut total_bytes = 0u64;
    for _ in 0..iters {
        let t = Instant::now();
        let m = Manifest::load(&repo, &commit).unwrap();
        load_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        files = m.len();
        total_bytes = m.total_size();
    }
    let load_med = median(&mut load_ms);

    let manifest = Arc::new(Manifest::load(&repo, &commit).unwrap());
    let cat = Arc::new(CatFile::new(&repo));
    let cache = Arc::new(HydrationCache::open(base.path().join("store")));
    let config = CasViewConfig::new(base.path().join("upper"), base.path().join("meta"));
    let view = CasView::open(config, manifest.clone(), cat.clone(), cache.clone()).unwrap();

    // ---- 2. 逐文件 stat（CAS 视图，纯内存 + upper 探测）------------------
    let paths: Vec<String> = manifest.files().keys().cloned().collect();
    let mut stat_ms = Vec::new();
    for _ in 0..iters {
        let t = Instant::now();
        for p in &paths {
            assert!(view.stat(p).unwrap().is_some(), "{p}");
        }
        stat_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let stat_med = median(&mut stat_ms);

    // ---- 3. list_stat 全树遍历（每目录一次 readdir）----------------------
    let mut dirs: Vec<String> = manifest.dirs().iter().cloned().collect();
    dirs.push(String::new());
    let mut list_ms = Vec::new();
    let mut listed = 0usize;
    for _ in 0..iters {
        listed = 0;
        let t = Instant::now();
        for d in &dirs {
            listed += view.list_stat(d).unwrap().len();
        }
        list_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let list_med = median(&mut list_ms);

    // ---- 4. 按需 hydrate（N 文件两遍：miss → hit）------------------------
    let n = hydrate_n.min(files);
    let mut hydrate_cold_ms = Vec::new();
    let mut hydrate_hot_ms = Vec::new();
    for _ in 0..iters {
        cache.clear().unwrap();
        let t = Instant::now();
        for p in paths.iter().take(n) {
            let f = manifest.file(p).unwrap();
            if f.mode == wtfs_cas::MODE_GITLINK {
                continue;
            }
            let path = cache.ensure(&cat, &f.oid, f.size).unwrap();
            assert_eq!(std::fs::metadata(&path).unwrap().len(), f.size);
        }
        hydrate_cold_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        let t = Instant::now();
        for p in paths.iter().take(n) {
            let f = manifest.file(p).unwrap();
            if f.mode == wtfs_cas::MODE_GITLINK {
                continue;
            }
            cache.ensure(&cat, &f.oid, f.size).unwrap();
        }
        hydrate_hot_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    cache.clear().unwrap();
    // iters 循环后缓存是热态；清空重灌一遍拿最终缓存体量
    for p in paths.iter().take(n) {
        let f = manifest.file(p).unwrap();
        if f.mode == wtfs_cas::MODE_GITLINK {
            continue;
        }
        cache.ensure(&cat, &f.oid, f.size).unwrap();
    }
    let hydrated_bytes = cache.cached_bytes();
    let cold_med = median(&mut hydrate_cold_ms);
    let hot_med = median(&mut hydrate_hot_ms);
    let throughput = if cold_med > 0.0 {
        hydrated_bytes as f64 / 1024.0 / 1024.0 / (cold_med / 1000.0)
    } else {
        0.0
    };
    let cat_stats = cat.stats();
    let cache_stats = cache.stats();

    // ---- 5. 索引种子 route A（未挂载干净）--------------------------------
    let misses_before_seed = cache.stats().misses;
    let wt_a = base.path().join("wt-seed-a");
    add_worktree_no_checkout(&repo, &wt_a, "bench/seed-a").unwrap();
    let t = Instant::now();
    let seed_a = seed_skip_worktree(&wt_a, &commit).unwrap();
    let seed_a_total_ms = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    let status_a_lines = common::status_lines(&wt_a);
    let status_a_ms = t.elapsed().as_secs_f64() * 1000.0;

    // route A 完整形态（stat 对齐 + skip-worktree）
    let wt_c = base.path().join("wt-seed-c");
    add_worktree_no_checkout(&repo, &wt_c, "bench/seed-c").unwrap();
    let t = Instant::now();
    let seed_c = seed_stat_aligned(
        &wt_c,
        &commit,
        &manifest,
        DEFAULT_VOLUME_SERIAL,
        FIXED_STAT_EPOCH_UNIX_NS,
    )
    .unwrap();
    let seed_c_total_ms = t.elapsed().as_secs_f64() * 1000.0;
    let status_c_lines = common::status_lines(&wt_c);

    // 挂载态切换的清除成本（对照）
    let t = Instant::now();
    let cleared = clear_skip_worktree(&wt_c).unwrap();
    let clear_ms = t.elapsed().as_secs_f64() * 1000.0;
    let status_after_clear = common::status_lines(&wt_c);

    // ---- 6. route B 对照：全量 checkout → 复制 index → 删树 ---------------
    let wt_b = base.path().join("wt-seed-b");
    let t = Instant::now();
    common::assert_git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            &wt_b.to_string_lossy(),
            "-b",
            "bench/seed-b",
        ],
    );
    let checkout_ms = t.elapsed().as_secs_f64() * 1000.0;
    let gitdir = common::assert_git(&wt_b, &["rev-parse", "--absolute-git-dir"])
        .trim()
        .to_string();
    let index = Path::new(&gitdir).join("index");
    let snapshot = base.path().join("template-index");
    std::fs::copy(&index, &snapshot).unwrap();
    let t = Instant::now();
    for entry in std::fs::read_dir(&wt_b).unwrap().flatten() {
        let name = entry.file_name();
        if name != ".git" {
            let p = entry.path();
            if p.is_dir() {
                std::fs::remove_dir_all(&p).unwrap();
            } else {
                std::fs::remove_file(&p).unwrap();
            }
        }
    }
    std::fs::copy(&snapshot, &index).unwrap();
    let delete_ms = t.elapsed().as_secs_f64() * 1000.0;
    let status_b_lines = common::status_lines(&wt_b);

    println!("{REPORT_BEGIN}");
    println!("# M17/B170100 CAS core 基准（C 方案，未挂载）");
    println!();
    println!("| 指标 | 值 |");
    println!("| ---- | -- |");
    println!(
        "| 来源仓库 | {}（{} tracked 文件，setup {:.1}s） |",
        std::env::var("WTFS_BENCH_REPO").unwrap_or_else(|_| format!("synthetic/{SYNTHETIC_FILES}")),
        tracked,
        setup_s
    );
    println!(
        "| manifest 加载（`ls-tree -r -l -z` + 解析 + 索引） | {}（{files} 文件，{}） |",
        fmt_ms(load_med),
        fmt_size(total_bytes)
    );
    println!(
        "| stat 全部文件（{files} 个，逐个 view.stat） | {}（{:.2} µs/个） |",
        fmt_ms(stat_med),
        stat_med * 1000.0 / files as f64
    );
    println!(
        "| list_stat 全树遍历（{} 目录，{} 条目） | {} |",
        dirs.len(),
        listed,
        fmt_ms(list_med)
    );
    println!(
        "| hydrate {n} 文件冷态（cat-file + temp+fsync+rename） | {}（{:.1} MB/s，{}） |",
        fmt_ms(cold_med),
        throughput,
        fmt_size(hydrated_bytes)
    );
    println!(
        "| hydrate {n} 文件热态（全命中，零对象读） | {} |",
        fmt_ms(hot_med)
    );
    println!(
        "| cat-file 累计 | {} 请求 / {} blob / {} |",
        cat_stats.requests,
        cat_stats.blobs,
        fmt_size(cat_stats.bytes)
    );
    println!(
        "| 缓存统计 | {} miss / {} hit / {} 灌入 |",
        cache_stats.misses,
        cache_stats.hits,
        fmt_size(cache_stats.hydrated_bytes)
    );
    println!(
        "| 种子 route A（read-tree + 批量 skip-worktree） | {}（read-tree {}，置位 {}）→ status {}，{} 行（干净） |",
        fmt_ms(seed_a_total_ms),
        fmt_ms(seed_a.read_tree_ms),
        fmt_ms(seed_a.skip_worktree_ms),
        fmt_ms(status_a_ms),
        status_a_lines
    );
    println!(
        "| 种子 route A 完整形态（+gix-index stat 对齐） | {}（read-tree {}，改写 {}）→ status {} 行 |",
        fmt_ms(seed_c_total_ms),
        fmt_ms(seed_c.read_tree_ms),
        fmt_ms(seed_c.stat_rewrite_ms),
        status_c_lines
    );
    println!(
        "| 挂载态切换（批量清 skip-worktree 位） | {}（{} 条目）→ 未挂载 status {} 行（预期全量删除） |",
        fmt_ms(clear_ms),
        cleared,
        status_after_clear
    );
    println!(
        "| 对照 route B（全量 checkout → 复制 index → 删树） | checkout {} + 删树 {} → 未挂载 status {} 行（必脏，弃用） |",
        fmt_ms(checkout_ms),
        fmt_ms(delete_ms),
        status_b_lines
    );
    println!(
        "| 种子期间 hydrate | {} 次（零 hydrate 证明：misses 前后差值） |",
        cache.stats().misses - misses_before_seed
    );
    println!("{REPORT_END}");

    // 报告自检：关键断言（干净/零 hydrate）失败立即让基准红
    assert_eq!(status_a_lines, 0, "route A 种子后 status 必须干净");
    assert_eq!(
        cache.stats().misses,
        misses_before_seed,
        "种子全程不得触发 hydration"
    );
    assert_eq!(status_c_lines, 0, "stat 对齐种子后 status 必须干净");
}
