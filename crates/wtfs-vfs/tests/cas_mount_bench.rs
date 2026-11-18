//! CAS 挂载管线基准（M18/B180100，release + 真实 WinFSP）。默认 `#[ignore]`。
//!
//! 夹具：`WTFS_BENCH_REPO` 指向真实大仓库（M12 夹具
//! `C:/Users/notxx/AppData/Local/Temp/wtfs-bench-large-m12/repo`，git 源码
//! 4,874 文件；与 B1 同口径）。缺省跳过（合成仓库由 `wtfs-cas` 的
//! `cas_bench` 覆盖库层口径，本基准测挂载端到端）。
//!
//! 口径（10 worktree）：add（--no-checkout + 种子 + 配置 + 注册 + 清位）、
//! status 冷/热（挂载视图 git status）、diff、commit、remove、磁盘
//! （upper + 标记日志 + hydration 缓存）、首次读（逐文件冷读）、缓存大小。
//!
//! ```bat
//! set WTFS_BENCH_REPO=C:/Users/notxx/AppData/Local/Temp/wtfs-bench-large-m12/repo
//! cargo test --release -p wtfs-vfs --test cas_mount_bench -- --ignored --nocapture
//! ```

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use wtfs_cas::{
    CatFile, HydrationCache, Manifest,
    clear_skip_worktree, seed_stat_aligned, write_stat_config,
};
use wtfs_core::FIXED_STAT_EPOCH_UNIX_NS;
use wtfs_vfs::{CasShared, VfsConfig, VfsRegistry, mount_directory, register_cas_worktree};

const WORKTREES: usize = 10;

fn git(repo: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("git")
        .arg("-c")
        .arg("user.name=wtfs-bench")
        .arg("-c")
        .arg("user.email=wtfs-bench@example.com")
        .arg("-c")
        .arg("core.longpaths=true")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git CLI must be available");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

fn assert_git(repo: &Path, args: &[&str]) -> String {
    let (ok, out) = git(repo, args);
    assert!(ok, "git {args:?} failed: {out}");
    out
}

/// 目录字节数（upper/标记/缓存合计口径）。
fn dir_bytes(dir: &Path) -> (u64, usize) {
    let mut bytes = 0u64;
    let mut files = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                files += 1;
                bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    (bytes, files)
}

#[test]
#[ignore = "requires WinFSP runtime, git CLI and WTFS_BENCH_REPO fixture"]
fn bench_cas_mount_pipeline() {
    let Some(repo) = std::env::var("WTFS_BENCH_REPO").ok().map(PathBuf::from) else {
        eprintln!("skip: WTFS_BENCH_REPO not set (mount bench targets a real large repo)");
        return;
    };
    assert!(repo.is_dir(), "fixture repo missing: {}", repo.display());
    let (ok, out) = git(&repo, &["rev-parse", "HEAD"]);
    assert!(ok, "fixture repo is not a git repo: {out}");
    let head = out.trim().to_string();
    // 共享夹具自愈：上次失败运行残留的 prunable worktree 注册会污染
    // `worktree add`（分支冲突/元数据残留），开局清理。
    let _ = git(&repo, &["worktree", "prune"]);
    let file_count = {
        let out = assert_git(&repo, &["ls-files", "-z"]);
        out.split('\0').filter(|s| !s.is_empty()).count()
    };
    eprintln!("fixture: {} ({file_count} files @ {head})", repo.display());
    let absorb = std::env::var("WTFS_CAS_ABSORB").is_ok();
    eprintln!("mode: {}", if absorb { "absorb (plain reset --hard)" } else { "lazy (--no-checkout + seed)" });

    let base = tempfile::tempdir().expect("tempdir");
    let base = base.path().to_path_buf();
    let store_root = base.join("store");
    let cas_wt_root = base.join("cas-wt");
    let mount_point = repo.join(".tower").join("worktrees");
    let _ = std::fs::remove_dir_all(&mount_point);
    std::fs::create_dir_all(&mount_point).unwrap();

    let cfg = VfsConfig::new(&repo, &store_root);
    let registry = Arc::new(VfsRegistry::open(cfg).unwrap());
    let volume = mount_directory(&mount_point, registry.clone())
        .expect("WinFSP directory mount must succeed");
    let shared = Arc::new(CasShared {
        cat: Arc::new(CatFile::new(&repo)),
        cache: Arc::new(HydrationCache::open(&store_root)),
    });

    let manifest = Arc::new(Manifest::load(&repo, &head).expect("manifest load"));
    eprintln!(
        "manifest: {} files, {} bytes (load once, shared across worktrees)",
        manifest.len(),
        manifest.total_size()
    );

    // ---- add：--no-checkout +（lazy: seed+清位 / absorb: reset --hard）----
    let mut add_ms = Vec::new();
    let mut setup_ms = Vec::new();
    let mut checkout_ms = Vec::new();
    let mut names = Vec::new();
    for i in 1..=WORKTREES {
        let name = format!("wt-cas-{i}");
        let real_wt = cas_wt_root.join(&name);
        let t0 = Instant::now();
        let (ok, out) = git(
            &repo,
            &["worktree", "add", "--no-checkout", "--detach", &real_wt.to_string_lossy(), head.as_str()],
        );
        assert!(ok, "worktree add {name}: {out}");
        let t_add = t0.elapsed().as_secs_f64() * 1000.0;
        let t1 = Instant::now();
        if absorb {
            // 不 seed / 不清位：index 保持空，reset --hard 会真正写全部文件，
            // 由 CasBackend 的 absorbing 窗口吸收到内存并按 manifest oid 终裁。
            write_stat_config(&real_wt).expect("stat config");
        } else {
            let report = seed_stat_aligned(
                &real_wt,
                &head,
                &manifest,
                registry.config().volume_serial,
                FIXED_STAT_EPOCH_UNIX_NS,
            )
            .expect("seed");
            write_stat_config(&real_wt).expect("stat config");
            let cleared = clear_skip_worktree(&real_wt).expect("clear skip-worktree");
            assert_eq!(cleared, report.entries, "清位数 = index entry 数");
        }
        let t_setup = t1.elapsed().as_secs_f64() * 1000.0;
        let backend = register_cas_worktree(
            &registry,
            &name,
            &real_wt,
            manifest.clone(),
            shared.clone(),
        )
        .expect("register");
        if absorb {
            backend.set_absorbing(true);
            let view_dir = mount_point.join(&name);
            let t2 = Instant::now();
            let (ok, out) = git(&view_dir, &["reset", "--hard"]);
            assert!(ok, "reset --hard {name}: {out}");
            checkout_ms.push(t2.elapsed().as_secs_f64() * 1000.0);
            backend.set_absorbing(false);
        }
        add_ms.push(t_add);
        setup_ms.push(t_setup);
        names.push(name);
    }

    // ---- status 冷（挂载后首个 status）/ 热（第二个 status）----
    let mut status_cold_ms = Vec::new();
    let mut status_hot_ms = Vec::new();
    for name in names.iter() {
        let view_dir = mount_point.join(name);
        let t = Instant::now();
        let out = assert_git(&view_dir, &["status", "--porcelain"]);
        status_cold_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(out.trim(), "", "{name} cold status must be clean: {out:?}");
        let t = Instant::now();
        let out = assert_git(&view_dir, &["status", "--porcelain"]);
        status_hot_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(out.trim(), "", "{name} hot status must be clean");
    }
    let misses_after_status = shared.cache.stats().misses;
    if !absorb && std::env::var("WTFS_BENCH_SKIP_HYDRATE_ASSERT").is_err() {
        // 断言口径（实测机制）：stat 对齐下 tracked 内容零 hydrate；git 的
        // untracked 走查会读 per-directory `.gitignore` 做 exclude 分类
        // （`-uno` 不读）——hydration 缓存按 oid 全局共享，14 个 .gitignore
        // 只在首个 worktree 的首个 status 各读一次，其余 worktree 全命中。
        // 断言：misses 全部落在 .gitignore，且第二个 worktree 起零新增。
        let mut non_gitignore = Vec::new();
        if misses_after_status != 0 {
            for shard in std::fs::read_dir(shared.cache.root()).into_iter().flatten() {
                let Some(shard) = shard.ok().map(|e| e.path()) else { continue };
                for entry in std::fs::read_dir(&shard).into_iter().flatten().flatten() {
                    let oid = entry.file_name().to_string_lossy().into_owned();
                    match manifest.files().values().find(|f| f.oid == oid) {
                        Some(f) if f.path.ends_with(".gitignore") => {}
                        Some(f) => non_gitignore.push(f.path.clone()),
                        None => non_gitignore.push(format!("{oid} (non-manifest)")),
                    }
                }
            }
        }
        assert!(
            non_gitignore.is_empty(),
            "status 不得 hydrate 非 exclude 文件：{non_gitignore:?}"
        );
        assert_eq!(
            misses_after_status, 14,
            "fixture 全树 14 个 .gitignore 全局共享各读一次；tracked 内容零 hydrate"
        );
    }

    // ---- diff（干净树）----
    let mut diff_ms = Vec::new();
    for name in &names {
        let view_dir = mount_point.join(name);
        let t = Instant::now();
        let out = assert_git(&view_dir, &["diff", "--stat"]);
        diff_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(out.trim(), "");
    }

    // ---- 首次读：逐文件冷读 200 个（每文件 = cat-file + 缓存落盘 + 读）----
    let mut sample: Vec<String> = manifest
        .files()
        .iter()
        .filter(|(p, f)| {
            f.mode & 0o170000 == 0o100000 && (p.ends_with(".c") || p.ends_with(".h"))
        })
        .map(|(p, _)| p.clone())
        .take(200)
        .collect();
    if sample.is_empty() {
        // dashboard 等非 git 源码夹具：取 manifest 前 200 个普通文件
        // （跳过 gitlink/目录，std::fs::read 对目录会 access denied）。
        sample = manifest
            .files()
            .iter()
            .filter(|(_, f)| f.mode & 0o170000 == 0o100000)
            .map(|(p, _)| p.clone())
            .take(200)
            .collect();
    }
    let t0 = Instant::now();
    let mut first_read_bytes = 0u64;
    for path in &sample {
        let data = std::fs::read(mount_point.join(&names[0]).join(path))
            .unwrap_or_else(|e| panic!("read {path}: {e}"));
        first_read_bytes += data.len() as u64;
    }
    let first_read_total_ms = t0.elapsed().as_secs_f64() * 1000.0;

    // ---- commit：每 worktree 改 1 文件 + commit -am ----
    let edit_path = if manifest.files().contains_key("package.json") {
        "package.json".to_string()
    } else {
        manifest
            .files()
            .keys()
            .next()
            .cloned()
            .expect("manifest must be non-empty")
    };
    let mut commit_ms = Vec::new();
    for (i, name) in names.iter().enumerate() {
        let view_dir = mount_point.join(name);
        std::fs::write(view_dir.join(&edit_path), format!("bench {i}\n")).unwrap();
        let t = Instant::now();
        assert_git(&view_dir, &["commit", "-am", "bench edit", "-q"]);
        commit_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        let cas = registry
            .get_backend(name)
            .and_then(|b| b.as_cas().cloned())
            .expect("cas backend");
        cas.refresh_head().expect("refresh");
        let out = assert_git(&view_dir, &["status", "--porcelain"]);
        assert_eq!(out.trim(), "", "{name} post-commit status must be clean");
    }

    // ---- 磁盘（提交后、remove 前）：upper 残留 + 标记日志 + hydration 缓存 ----
    let (wt_bytes, wt_files) = dir_bytes(&cas_wt_root);
    let (cache_bytes, cache_files) = dir_bytes(shared.cache.root());
    let (meta_bytes, meta_files) = dir_bytes(&store_root.join("cas-meta"));

    // ---- remove：worktree remove + 注销 ----
    let mut remove_ms = Vec::new();
    for name in &names {
        let real_wt = cas_wt_root.join(name);
        let t = Instant::now();
        let (ok, out) = git(&repo, &["worktree", "remove", "--force", &real_wt.to_string_lossy()]);
        assert!(ok, "worktree remove {name}: {out}");
        registry.unregister(name);
        remove_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }

    let median = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    let checkout_row = if absorb {
        format!(
            "| checkout（reset --hard，absorbing，每 wt 中位） | {:.1} ms |\n",
            median(checkout_ms)
        )
    } else {
        String::new()
    };
    let hydrate_note = if absorb {
        "status 期间 hydrate（absorb 模式不强制 14 次）".to_string()
    } else {
        "status 期间 hydrate（全部 .gitignore exclude，oid 缓存全局共享一次；tracked 内容 0）"
            .to_string()
    };
    let report = format!(
        "\n## C checkout 基准（release，{} 文件 / {} worktree，{}）\n\n\
        | 指标 | 值 |\n| ---- | -- |\n\
        | add（worktree add --no-checkout，每 wt 中位） | {:.1} ms |\n\
        | setup（{}，每 wt 中位） | {:.1} ms |\n\
        {}\
        | status 冷（挂载视图，每 wt 中位） | {:.1} ms |\n\
        | status 热（每 wt 中位） | {:.1} ms |\n\
        | diff --stat 干净树（每 wt 中位） | {:.1} ms |\n\
        | commit -am 单文件（每 wt 中位） | {:.1} ms |\n\
        | remove（worktree remove + 注销，每 wt 中位） | {:.1} ms |\n\
        | 首次读 {} 文件（冷，合计） | {:.1} ms（{:.3} ms/文件，{:.1} KB） |\n\
        | {} | {} 次 |\n\
        | hydration 缓存（首读后） | {} 文件 / {:.1} KB |\n\
        | upper 磁盘（10 wt 合计，含 .git 指针 + 提交残留） | {} 文件 / {:.1} KB |\n\
        | 标记日志磁盘 | {} 文件 / {:.1} KB |\n\
        | manifest 装载（一次，跨 wt 共享） | {} 文件 / {:.1} MB 树 |\n\
        \n",
        file_count,
        WORKTREES,
        if absorb { "absorb" } else { "lazy" },
        median(add_ms),
        if absorb { "stat config，无 seed" } else { "read-tree + stat 对齐 + 配置" },
        median(setup_ms),
        checkout_row,
        median(status_cold_ms),
        median(status_hot_ms),
        median(diff_ms),
        median(commit_ms),
        median(remove_ms),
        sample.len(),
        first_read_total_ms,
        first_read_total_ms / sample.len() as f64,
        first_read_bytes as f64 / 1024.0,
        hydrate_note,
        misses_after_status,
        cache_files,
        cache_bytes as f64 / 1024.0,
        wt_files,
        wt_bytes as f64 / 1024.0,
        meta_files,
        meta_bytes as f64 / 1024.0,
        manifest.len(),
        manifest.total_size() as f64 / 1024.0 / 1024.0,
    );
    eprintln!("{report}");

    volume.unmount();
    let _ = std::fs::remove_dir_all(&mount_point);
}
