//! M20/B200100 基准 driver：在 C（CAS）挂载视图上跑 plain `git worktree add`。
//!
//! 位置说明：本 crate 刻意放 `scripts/`（bench 脚手架，非产品代码）；仓库
//! workspace members 不含它，Cargo.toml 末尾的空 `[workspace]` 表使其脱离
//! workspace，可 `cargo build --manifest-path scripts/m20-driver/Cargo.toml`
//! 独立构建（path dep 指向 crates/，复用仓库 target 的既有产物）。
//!
//! 两条腿（M20 tower 指令）：
//! * leg A（真实路径）：不带 `--no-checkout` 的 plain `git worktree add`，
//!   目标在挂载根下。M18 未接 plain-add 拦截（shim/daemon watcher 留后续），
//!   实测会发生什么——挂载根 mkdir 命中 `VfsRegistry::auto_register`（B1
//!   overlay 兜底）还是别的。如实记录，不冒充 CAS 流程。
//! * leg B（模拟路径）：`worktree add --no-checkout` 建元数据 + seed + 注册
//!   + 清位，再删 index + `git reset --hard` 强制经 CAS 视图全量物化——
//!   即 plain add 的 checkout 阶段会做的事。明确是模拟，不是真实 plain add。
//!
//! 报告经 eprintln 落 stderr，标记行 `## M20 C-PLAIN`（bench-m20-c-plain.ps1
//! 按此解析）：逐 wt 明细 + 汇总（中位）。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use wtfs_cas::{
    CatFile, HydrationCache, Manifest,
    clear_skip_worktree, seed_stat_aligned, write_stat_config,
};
use wtfs_core::FIXED_STAT_EPOCH_UNIX_NS;
use wtfs_vfs::{
    CasShared, VfsConfig, VfsRegistry, ViewBackend,
    mount_directory, register_cas_worktree,
};

fn git(repo: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("git")
        .arg("-c")
        .arg("user.name=wtfs-bench")
        .arg("-c")
        .arg("user.email=wtfs-bench@example.com")
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

/// 计时 + 软执行（leg A 取证腿：git 失败是发现的一部分，不得 panic）。
fn soft_git(repo: &Path, args: &[&str]) -> (bool, String, f64) {
    let t0 = Instant::now();
    let (ok, out) = git(repo, args);
    (ok, out, t0.elapsed().as_secs_f64() * 1000.0)
}

/// 目录字节数（upper/缓存合计口径）。
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

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// 首 3 行摘要（git 输出证据，防伪造）。
fn excerpt(text: &str) -> String {
    let mut lines: Vec<String> = text
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .take(3)
        .collect();
    if lines.is_empty() {
        lines.push("(empty output)".into());
    }
    lines.join(" | ")
}

/// `wt-<n>` → WorktreeId(n)；非数字命名派生自 FNV-1a——与
/// wtfs-vfs/src/registry.rs 的 `worktree_id_for_name` 同款（upper 目录名）。
fn upper_dir_for_name(store_root: &Path, name: &str) -> PathBuf {
    let id = if let Some(rest) = name.strip_prefix("wt-") {
        if let Ok(n) = rest.parse::<i64>() {
            if n > 0 {
                return store_root.join("worktrees").join(format!("wt-{n}"));
            }
        }
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for b in name.bytes() {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
        (hash & 0x3FFF_FFFF_FFFF_FFFF).max(1)
    } else {
        1
    };
    store_root.join("worktrees").join(format!("wt-{id}"))
}

struct LegA {
    add_ms: Vec<f64>,
    add_fail_out: Vec<String>,
    backend_desc: String,
    add_out_excerpt: String,
    initializing: bool,
    upper_after_add_kb: Vec<f64>,
    upper_after_add_files: Vec<usize>,
    status_cold_ms: Vec<f64>,
    status_hot_ms: Vec<f64>,
    status_dirty: Vec<String>,
    status_dirty_lines: usize,
    diff_ms: Vec<f64>,
    diff_dirty: Vec<String>,
    commit_ms: Vec<f64>,
    commit_fail_out: Vec<String>,
    remove_try1_ok: usize,
    remove_try1_ms: Vec<f64>,
    remove_try1_out: String,
    upper_files_after_try1: usize,
    remove_fallback_ms: Vec<f64>,
    remove_daemon_ms: f64,
    remove_daemon_ok: bool,
    remove_daemon_fail_out: String,
    upper_total_commit_kb: f64,
}

fn leg_a(
    repo: &Path,
    mount_point: &Path,
    registry: &Arc<VfsRegistry>,
    store_root: &Path,
    head: &str,
    worktrees: usize,
) -> LegA {
    let mut r = LegA {
        add_ms: vec![],
        add_fail_out: vec![],
        backend_desc: String::new(),
        add_out_excerpt: String::new(),
        initializing: false,
        upper_after_add_kb: vec![],
        upper_after_add_files: vec![],
        status_cold_ms: vec![],
        status_hot_ms: vec![],
        status_dirty: vec![],
        status_dirty_lines: 0,
        diff_ms: vec![],
        diff_dirty: vec![],
        commit_ms: vec![],
        commit_fail_out: vec![],
        remove_try1_ok: 0,
        remove_try1_ms: vec![],
        remove_try1_out: String::new(),
        upper_files_after_try1: 0,
        remove_fallback_ms: vec![],
        remove_daemon_ms: 0.0,
        remove_daemon_ok: false,
        remove_daemon_fail_out: String::new(),
        upper_total_commit_kb: 0.0,
    };
    let upper_root = store_root.join("worktrees");
    let mut upper_files_before: usize = 0;

    for i in 1..=worktrees {
        let name = format!("wt-plain-{i}");
        let view = mount_point.join(&name);
        eprintln!("leg A: plain add {name}（挂载根 mkdir → 实测路由）...");
        let t0 = Instant::now();
        let (ok, out) = git(
            repo,
            &["worktree", "add", "--detach", &view.to_string_lossy(), head],
        );
        let t_add = t0.elapsed().as_secs_f64() * 1000.0;
        eprintln!("leg A: add {name} = {t_add:.1} ms ok={ok}");
        if !ok {
            r.add_fail_out.push(format!("{name}: {}", excerpt(&out)));
            break;
        }
        r.add_ms.push(t_add);
        if r.backend_desc.is_empty() {
            r.add_out_excerpt = excerpt(&out);
            r.backend_desc = match registry.get_backend(&name) {
                Some(ViewBackend::Overlay(_)) => {
                    "B1 overlay（挂载根 mkdir 命中 auto_register；store 无模板 → full-copy 兜底）"
                        .to_string()
                }
                Some(ViewBackend::Cas(_)) => {
                    "CAS 视图（意外：plain add 命中 CAS 注册路径）".to_string()
                }
                None => "未注册".to_string(),
            };
            r.initializing = registry
                .get(&name)
                .map(|o| o.is_initializing().unwrap_or(false))
                .unwrap_or(false);
        }
        let (ub, uf) = dir_bytes(&upper_root);
        r.upper_after_add_kb.push(ub as f64 / 1024.0);
        r.upper_after_add_files.push(uf - upper_files_before);
        upper_files_before = uf;

        let (ok, out, t_cold) = soft_git(&view, &["status", "--porcelain"]);
        r.status_cold_ms.push(t_cold);
        if !ok {
            r.status_dirty.push(format!(
                "{name} status FATAL: {}",
                out.lines().take(2).collect::<Vec<_>>().join(" | ")
            ));
        } else if !out.trim().is_empty() {
            r.status_dirty_lines = out.lines().filter(|l| !l.trim().is_empty()).count();
            let sample: Vec<String> = out
                .lines()
                .filter(|l| !l.trim().is_empty())
                .take(10)
                .map(|l| l.to_string())
                .collect();
            r.status_dirty.push(format!(
                "{name}（共 {} 条）: {}",
                r.status_dirty_lines,
                sample.join(" | ")
            ));
        }
        let (ok, out, t_hot) = soft_git(&view, &["status", "--porcelain"]);
        r.status_hot_ms.push(t_hot);
        if !ok {
            r.status_dirty.push(format!(
                "{name} status热 FATAL: {}",
                out.lines().take(2).collect::<Vec<_>>().join(" | ")
            ));
        }

        let (ok, out, t_diff) = soft_git(&view, &["diff", "--stat"]);
        r.diff_ms.push(t_diff);
        if ok && !out.trim().is_empty() {
            r.diff_dirty.push(format!("{name}: {}", excerpt(&out)));
        } else if !ok {
            r.diff_dirty.push(format!(
                "{name} diff FATAL: {}",
                out.lines().take(2).collect::<Vec<_>>().join(" | ")
            ));
        }

        if std::fs::write(view.join("README.md"), format!("# bench legA {i}\n")).is_err() {
            r.commit_fail_out.push(format!("{name}: README 经视图写失败（视图不可用）"));
            continue;
        }
        let (ok, out, t_commit) = soft_git(&view, &["commit", "-am", "bench edit", "-q"]);
        if !ok {
            r.commit_fail_out.push(format!("{name}: {}", excerpt(&out)));
        } else {
            r.commit_ms.push(t_commit);
        }
        let (ok, out, _) = soft_git(&view, &["status", "--porcelain"]);
        if ok && !out.trim().is_empty() {
            r.status_dirty.push(format!("{name} post-commit: {}", excerpt(&out)));
        } else if !ok {
            r.status_dirty.push(format!(
                "{name} post-commit FATAL: {}",
                out.lines().take(2).collect::<Vec<_>>().join(" | ")
            ));
        }
    }

    let (ub, _) = dir_bytes(&upper_root);
    r.upper_total_commit_kb = ub as f64 / 1024.0;

    // remove（三条路，如实记录）：
    // * wt 1..n-1：无 daemon 首试（initializing 根 rmdir 必拒）→ 计时兜底
    //   清理（prune + 原生删 upper 目录）——no-shim 时唯一能脱手的方式。
    // * 最后一个 wt：mark_ready（模拟 daemon watcher）→ git remove --force
    //   （预期成功——这就是"有 daemon 时"的可比样本）。
    let plain_count = worktrees.saturating_sub(1);
    for i in 1..=worktrees {
        let name = format!("wt-plain-{i}");
        let view = mount_point.join(&name);
        if r.add_fail_out.iter().any(|f| f.starts_with(&name)) {
            continue;
        }
        if i <= plain_count {
            eprintln!("leg A: remove {name}（无 daemon 首试）...");
            let t0 = Instant::now();
            let (ok1, out1) =
                git(repo, &["worktree", "remove", "--force", &view.to_string_lossy()]);
            let t1 = t0.elapsed().as_secs_f64() * 1000.0;
            if ok1 {
                r.remove_try1_ok += 1;
            } else if r.remove_try1_out.is_empty() {
                r.remove_try1_out = out1.lines().take(3).collect::<Vec<_>>().join(" | ");
            }
            r.remove_try1_ms.push(t1);
            let (_, uf1) = dir_bytes(&upper_root);
            r.upper_files_after_try1 = uf1;

            eprintln!(
                "leg A: remove {name} ok={ok1} → 兜底清理（原生删该 wt 的 upper 目录，不跑 prune——prune 会把其余 initializing wt 的 admin 一并吃掉）..."
            );
            let t0 = Instant::now();
            let wt_dir = upper_dir_for_name(store_root, &name);
            let _ = std::fs::remove_dir_all(&wt_dir);
            r.remove_fallback_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
            registry.unregister(&name);
        } else {
            eprintln!("leg A: {name} mark_ready（模拟 daemon）→ remove --force...");
            if let Err(e) = registry.mark_ready(&name) {
                eprintln!("leg A: mark_ready {name} failed: {e}");
            }
            let t0 = Instant::now();
            let (ok2, out2) =
                git(repo, &["worktree", "remove", "--force", &view.to_string_lossy()]);
            r.remove_daemon_ms = t0.elapsed().as_secs_f64() * 1000.0;
            r.remove_daemon_ok = ok2;
            if !ok2 {
                r.remove_daemon_fail_out =
                    out2.lines().take(3).collect::<Vec<_>>().join(" | ");
                let wt_dir = upper_dir_for_name(store_root, &name);
                let _ = std::fs::remove_dir_all(&wt_dir);
            }
            registry.unregister(&name);
        }
    }
    // 全部 wt 处置完后统一 prune（此时 initializing admin 已无保留价值；
    // 提前 prune 会把待测 wt 的 admin 一并吃掉——r1 教训）。
    let _ = git(repo, &["worktree", "prune"]);
    r
}

struct LegB {
    add_ms: Vec<f64>,
    seed_ms: Vec<f64>,
    clear_ms: Vec<f64>,
    checkout_ms: Vec<f64>,
    checkout_fail_out: Vec<String>,
    checkout_via: String,
    upper_after_checkout_kb: Vec<f64>,
    upper_after_checkout_files: Vec<usize>,
    status_cold_ms: Vec<f64>,
    status_hot_ms: Vec<f64>,
    status_dirty: Vec<String>,
    status_new_misses: usize,
    diff_ms: Vec<f64>,
    commit_ms: Vec<f64>,
    commit_fail_out: Vec<String>,
    first_read_ms: f64,
    first_read_misses: usize,
    first_read_kb: f64,
    first_read_count: usize,
    cache_files: usize,
    cache_kb: f64,
    cache_misses_total: usize,
    remove_ms: Vec<f64>,
    remove_fail_out: Vec<String>,
    cleared_entries: usize,
}

#[allow(clippy::too_many_arguments)]
fn leg_b(
    repo: &Path,
    work: &Path,
    mount_point: &Path,
    registry: &Arc<VfsRegistry>,
    shared: &Arc<CasShared>,
    manifest: &Arc<Manifest>,
    head: &str,
    volume_serial: u32,
    worktrees: usize,
    first_read_files: usize,
) -> LegB {
    let mut r = LegB {
        add_ms: vec![],
        seed_ms: vec![],
        clear_ms: vec![],
        checkout_ms: vec![],
        checkout_fail_out: vec![],
        checkout_via: String::new(),
        upper_after_checkout_kb: vec![],
        upper_after_checkout_files: vec![],
        status_cold_ms: vec![],
        status_hot_ms: vec![],
        status_dirty: vec![],
        status_new_misses: 0,
        diff_ms: vec![],
        commit_ms: vec![],
        commit_fail_out: vec![],
        first_read_ms: 0.0,
        first_read_misses: 0,
        first_read_kb: 0.0,
        first_read_count: 0,
        cache_files: 0,
        cache_kb: 0.0,
        cache_misses_total: 0,
        remove_ms: vec![],
        remove_fail_out: vec![],
        cleared_entries: 0,
    };
    let cas_wt_root = work.join("cas-wt");
    let _ = std::fs::create_dir_all(&cas_wt_root);

    for i in 1..=worktrees {
        let name = format!("wt-cplain-{i}");
        let real_wt = cas_wt_root.join(&name);
        let view = mount_point.join(&name);
        eprintln!("leg B: add --no-checkout {name}（挂载外真实目录）...");
        let t0 = Instant::now();
        let (ok, out) = git(
            repo,
            &[
                "worktree",
                "add",
                "--no-checkout",
                "--detach",
                &real_wt.to_string_lossy(),
                head,
            ],
        );
        assert!(ok, "leg B worktree add {name}: {out}");
        r.add_ms.push(t0.elapsed().as_secs_f64() * 1000.0);

        let t0 = Instant::now();
        let report = seed_stat_aligned(
            &real_wt,
            head,
            manifest,
            volume_serial,
            FIXED_STAT_EPOCH_UNIX_NS,
        )
        .expect("seed");
        write_stat_config(&real_wt).expect("stat config");
        r.seed_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        r.cleared_entries = report.entries;

        register_cas_worktree(registry, &name, &real_wt, manifest.clone(), shared.clone())
            .expect("register cas view");
        let t0 = Instant::now();
        let cleared = clear_skip_worktree(&real_wt).expect("clear skip-worktree");
        r.clear_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(cleared, report.entries, "清位数 = index entry 数");

        // ---- 模拟 plain add 的 checkout 阶段：删 index + 强制全量物化 ----
        let index_path = repo
            .join(".git")
            .join("worktrees")
            .join(&name)
            .join("index");
        let _ = std::fs::remove_file(&index_path);
        eprintln!("leg B: 强制物化 checkout {name}（删 index + reset --hard 经 CAS 视图）...");
        let t0 = Instant::now();
        let (ok, out) = git(&view, &["reset", "--hard", "HEAD"]);
        let mut t_checkout = t0.elapsed().as_secs_f64() * 1000.0;
        if ok {
            if r.checkout_via.is_empty() {
                r.checkout_via = "git reset --hard（删 index 后）".to_string();
            }
        } else {
            // reset --hard 拒绝时退回 read-tree + checkout-index -f -a
            // （同为"把 HEAD 树逐文件写进视图"的等价路径，如实标注）。
            let (ok_rt, out_rt) = git(&view, &["read-tree", "HEAD"]);
            let (ok_ci, out_ci) = if ok_rt {
                git(&view, &["checkout-index", "-f", "-a"])
            } else {
                (false, out_rt)
            };
            t_checkout += t0.elapsed().as_secs_f64() * 1000.0;
            if ok_ci {
                if r.checkout_via.is_empty() {
                    r.checkout_via = "git read-tree + checkout-index -f -a（reset 失败退路）".to_string();
                }
            } else {
                r.checkout_fail_out.push(format!("{name}: {}", excerpt(&out_ci)));
                r.checkout_ms.push(t_checkout);
                continue;
            }
            let _ = out;
        }
        eprintln!("leg B: checkout {name} = {t_checkout:.1} ms ok={ok}");
        r.checkout_ms.push(t_checkout);

        let (ub, uf) = dir_bytes(&real_wt);
        r.upper_after_checkout_kb.push(ub as f64 / 1024.0);
        r.upper_after_checkout_files.push(uf);

        let m0 = shared.cache.stats().misses;
        let t0 = Instant::now();
        let out = assert_git(&view, &["status", "--porcelain"]);
        r.status_cold_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        if !out.trim().is_empty() {
            r.status_dirty.push(format!("{name}: {}", excerpt(&out)));
        }
        let t0 = Instant::now();
        let out = assert_git(&view, &["status", "--porcelain"]);
        r.status_hot_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        let _ = out;
        let m1 = shared.cache.stats().misses;
        r.status_new_misses += (m1 - m0) as usize;

        let t0 = Instant::now();
        let out = assert_git(&view, &["diff", "--stat"]);
        r.diff_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        if !out.trim().is_empty() {
            r.status_dirty.push(format!("{name} diff: {}", excerpt(&out)));
        }

        std::fs::write(
            view.join("README.md"),
            format!("# bench legB {i}\n"),
        )
        .expect("write README via view");
        let t0 = Instant::now();
        let (ok, out) = git(&view, &["commit", "-am", "bench edit", "-q"]);
        let t_commit = t0.elapsed().as_secs_f64() * 1000.0;
        if !ok {
            r.commit_fail_out.push(format!("{name}: {}", excerpt(&out)));
        } else {
            r.commit_ms.push(t_commit);
        }
        let cas = registry
            .get_backend(&name)
            .and_then(|b| b.as_cas().cloned())
            .expect("cas backend");
        cas.refresh_head().expect("refresh");
        let out = assert_git(&view, &["status", "--porcelain"]);
        if !out.trim().is_empty() {
            r.status_dirty.push(format!("{name} post-commit: {}", excerpt(&out)));
        }
    }

    // ---- 首读（首个 worktree，冷读 K 个 .c/.h）----
    let sample: Vec<String> = manifest
        .files()
        .keys()
        .filter(|p| p.ends_with(".c") || p.ends_with(".h"))
        .take(first_read_files)
        .cloned()
        .collect();
    if !sample.is_empty() {
        let view = mount_point.join("wt-cplain-1");
        let m0 = shared.cache.stats().misses;
        let t0 = Instant::now();
        for path in &sample {
            let data = std::fs::read(view.join(path))
                .unwrap_or_else(|e| panic!("read {path}: {e}"));
            r.first_read_kb += data.len() as f64 / 1024.0;
        }
        r.first_read_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let m1 = shared.cache.stats().misses;
        r.first_read_misses = (m1 - m0) as usize;
        r.first_read_count = sample.len();
    }

    // ---- remove：worktree remove --force（真实目录）+ 注销 ----
    for i in 1..=worktrees {
        let name = format!("wt-cplain-{i}");
        let real_wt = cas_wt_root.join(&name);
        if !real_wt.exists() {
            continue;
        }
        let t0 = Instant::now();
        let (ok, out) = git(repo, &["worktree", "remove", "--force", &real_wt.to_string_lossy()]);
        let t = t0.elapsed().as_secs_f64() * 1000.0;
        if ok {
            r.remove_ms.push(t);
        } else {
            r.remove_fail_out.push(format!("{name}: {}", excerpt(&out)));
        }
        registry.unregister(&name);
    }

    let (cb, cf) = dir_bytes(shared.cache.root());
    r.cache_files = cf;
    r.cache_kb = cb as f64 / 1024.0;
    r.cache_misses_total = shared.cache.stats().misses as usize;
    r
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut repo: Option<PathBuf> = None;
    let mut work: Option<PathBuf> = None;
    let mut worktrees = 3usize;
    let mut first_read = 200usize;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--repo" => {
                repo = Some(PathBuf::from(&args[i + 1]));
                i += 1;
            }
            "--work" => {
                work = Some(PathBuf::from(&args[i + 1]));
                i += 1;
            }
            "--worktrees" => {
                worktrees = args[i + 1].parse().expect("--worktrees number");
                i += 1;
            }
            "--first-read" => {
                first_read = args[i + 1].parse().expect("--first-read number");
                i += 1;
            }
            other => panic!("unknown arg: {other}"),
        }
        i += 1;
    }
    let repo = repo.expect("--repo required");
    let work = work.expect("--work required");

    let head = assert_git(&repo, &["rev-parse", "HEAD"]).trim().to_string();
    let _ = git(&repo, &["worktree", "prune"]);
    let file_count = {
        let out = assert_git(&repo, &["ls-files", "-z"]);
        out.split('\0').filter(|s| !s.is_empty()).count()
    };
    eprintln!(
        "fixture: {} ({file_count} files @ {})",
        repo.display(),
        &head[..8.min(head.len())]
    );

    let store_root = work.join("store");
    let mount_point = repo.join(".tower").join("worktrees");
    let _ = std::fs::remove_dir_all(&mount_point);
    std::fs::create_dir_all(&mount_point).unwrap();

    let cfg = VfsConfig::new(&repo, &store_root);
    let registry = Arc::new(VfsRegistry::open(cfg).unwrap());
    let volume = mount_directory(&mount_point, registry.clone())
        .expect("WinFSP directory mount must succeed");
    let volume_serial = registry.config().volume_serial;
    let shared = Arc::new(CasShared {
        cat: Arc::new(CatFile::new(&repo)),
        cache: Arc::new(HydrationCache::open(&store_root)),
    });
    let manifest = Arc::new(Manifest::load(&repo, &head).expect("manifest load"));
    eprintln!(
        "manifest: {} files / {:.1} MB（装载一次，跨 wt 共享）",
        manifest.len(),
        manifest.total_size() as f64 / 1024.0 / 1024.0
    );

    // ---- leg A：真实 plain add（不带 --no-checkout，目标在挂载根下）----
    let a = leg_a(&repo, &mount_point, &registry, &store_root, &head, worktrees);

    // ---- leg B：模拟路径（显式元数据流程 + 强制全量物化 checkout）----
    let b = leg_b(
        &repo,
        &work,
        &mount_point,
        &registry,
        &shared,
        &manifest,
        &head,
        volume_serial,
        worktrees,
        first_read,
    );

    volume.unmount();
    let _ = std::fs::remove_dir_all(&mount_point);

    let n = worktrees;

    let mut rep = String::new();
    rep.push_str(&format!(
        "\n## M20 C-PLAIN（release，{file_count} 文件 / {n} wt）\n\n| 指标 | 值 |\n| ---- | -- |\n"
    ));
    rep.push_str(&format!(
        "| leg A plain add（真实路径，挂载根 mkdir） | {:.1} ms（中位，ok {}/{}, 失败 {}） |\n",
        median(a.add_ms.clone()),
        a.add_ms.len(),
        n,
        a.add_fail_out.len()
    ));
    rep.push_str(&format!(
        "| leg A add 后端实测 | {}；initializing={}（无 daemon，无人 mark_ready） |\n",
        a.backend_desc, a.initializing
    ));
    rep.push_str(&format!("| leg A add git 输出摘要 | {} |\n", a.add_out_excerpt));
    if !a.add_fail_out.is_empty() {
        rep.push_str(&format!("| leg A add 失败输出 | {} |\n", a.add_fail_out.join(" / ")));
    }
    rep.push_str(&format!(
        "| leg A upper（add 后累计，每 wt 中位） | {:.1} KB / {} 文件（full-copy 兜底预期≈树大小） |\n",
        median(a.upper_after_add_kb.clone()),
        a.upper_after_add_files.last().copied().unwrap_or(0)
    ));
    rep.push_str(&format!(
        "| leg A status 冷/热（中位） | {:.1} / {:.1} ms（脏 {}） |\n",
        median(a.status_cold_ms.clone()),
        median(a.status_hot_ms.clone()),
        a.status_dirty.len()
    ));
    if !a.status_dirty.is_empty() {
        rep.push_str(&format!(
            "| leg A status 脏样本 | {} |\n",
            a.status_dirty.first().map(|s| s.clone()).unwrap_or_default()
        ));
    }
    rep.push_str(&format!(
        "| leg A diff --stat（中位） | {:.1} ms（非空 {}） |\n",
        median(a.diff_ms.clone()),
        a.diff_dirty.len()
    ));
    rep.push_str(&format!(
        "| leg A commit -am 单文件（中位） | {:.1} ms（失败 {}） |\n",
        median(a.commit_ms.clone()),
        a.commit_fail_out.len()
    ));
    rep.push_str(&format!(
        "| leg A remove 首试（无 daemon） | ok {}/{}，{:.1} ms（中位）；输出: {} |\n",
        a.remove_try1_ok,
        a.remove_try1_ms.len(),
        median(a.remove_try1_ms.clone()),
        a.remove_try1_out
    ));
    rep.push_str(&format!(
        "| leg A remove 首试后 upper 残留 | {} 文件（git 未删任何内容，且 admin 元数据已被移除） |\n",
        a.upper_files_after_try1
    ));
    rep.push_str(&format!(
        "| leg A remove 兜底清理（prune + 原生删 upper，中位） | {:.1} ms（no-shim 唯一脱手方式） |\n",
        median(a.remove_fallback_ms.clone())
    ));
    rep.push_str(&format!(
        "| leg A remove（mark_ready 后=模拟 daemon，末位 wt） | ok={}，{:.1} ms{} |\n",
        a.remove_daemon_ok,
        a.remove_daemon_ms,
        if a.remove_daemon_fail_out.is_empty() {
            String::new()
        } else {
            format!("；输出: {}", a.remove_daemon_fail_out)
        }
    ));
    rep.push_str(&format!(
        "| leg A upper 合计（commit 后，store/worktrees） | {:.1} KB |\n",
        a.upper_total_commit_kb
    ));
    rep.push_str(&format!(
        "| leg B add --no-checkout 元数据（每 wt 中位） | {:.1} ms |\n",
        median(b.add_ms.clone())
    ));
    rep.push_str(&format!(
        "| leg B seed+stat 配置（每 wt 中位） | {:.1} ms |\n",
        median(b.seed_ms.clone())
    ));
    rep.push_str(&format!(
        "| leg B 清位（每 wt 中位） | {:.1} ms（{} entries） |\n",
        median(b.clear_ms.clone()),
        b.cleared_entries
    ));
    rep.push_str(&format!(
        "| leg B 强制物化 checkout（{}，每 wt 中位） | {:.1} ms（ok {}/{}, 失败 {}） |\n",
        if b.checkout_via.is_empty() { "删 index + reset --hard" } else { &b.checkout_via },
        median(b.checkout_ms.clone()),
        b.checkout_ms.len() - b.checkout_fail_out.len(),
        n,
        b.checkout_fail_out.len()
    ));
    if !b.checkout_fail_out.is_empty() {
        rep.push_str(&format!(
            "| leg B checkout 失败输出 | {} |\n",
            b.checkout_fail_out.join(" / ")
        ));
    }
    rep.push_str(&format!(
        "| leg B upper（checkout 后，每 wt 中位） | {:.1} KB / {} 文件（含 .git 指针；manifest 一致写入应被回收） |\n",
        median(b.upper_after_checkout_kb.clone()),
        b.upper_after_checkout_files.first().copied().unwrap_or(0)
    ));
    rep.push_str(&format!(
        "| leg B status 冷/热（中位） | {:.1} / {:.1} ms（脏 {}） |\n",
        median(b.status_cold_ms.clone()),
        median(b.status_hot_ms.clone()),
        b.status_dirty.len()
    ));
    if !b.status_dirty.is_empty() {
        rep.push_str(&format!(
            "| leg B status 脏样本 | {} |\n",
            b.status_dirty.first().map(|s| s.clone()).unwrap_or_default()
        ));
    }
    rep.push_str(&format!(
        "| leg B status 期间 hydrate（新增 miss） | {} 次 |\n",
        b.status_new_misses
    ));
    rep.push_str(&format!(
        "| leg B diff --stat（中位） | {:.1} ms |\n",
        median(b.diff_ms.clone())
    ));
    rep.push_str(&format!(
        "| leg B commit -am 单文件（中位） | {:.1} ms（失败 {}） |\n",
        median(b.commit_ms.clone()),
        b.commit_fail_out.len()
    ));
    rep.push_str(&format!(
        "| leg B 首读 {} 文件（冷，合计） | {:.1} ms（{:.3} ms/文件，{:.1} KB，miss {}） |\n",
        b.first_read_count,
        b.first_read_ms,
        if b.first_read_count > 0 { b.first_read_ms / b.first_read_count as f64 } else { 0.0 },
        b.first_read_kb,
        b.first_read_misses
    ));
    rep.push_str(&format!(
        "| leg B hydration 缓存（首读后，全局共享） | {} 文件 / {:.1} KB（总 miss {}） |\n",
        b.cache_files, b.cache_kb, b.cache_misses_total
    ));
    rep.push_str(&format!(
        "| leg B remove（--force + 注销，中位） | {:.1} ms（失败 {}） |\n",
        median(b.remove_ms.clone()),
        b.remove_fail_out.len()
    ));
    rep.push_str(&format!(
        "| manifest 装载（一次，跨 wt 共享） | {} 文件 / {:.1} MB 树 |\n",
        manifest.len(),
        manifest.total_size() as f64 / 1024.0 / 1024.0
    ));
    rep.push('\n');
    // 机器可读汇总（bench-m20-c-plain.ps1 按此解析，避免啃 markdown 表格）。
    rep.push_str("### KV\n");
    let kv = [
        ("A_add_median_ms", median(a.add_ms.clone())),
        ("A_add_ok", a.add_ms.len() as f64),
        ("A_add_fail", a.add_fail_out.len() as f64),
        ("A_backend_overlay_fallback", if a.backend_desc.contains("B1 overlay") { 1.0 } else { 0.0 }),
        ("A_initializing", if a.initializing { 1.0 } else { 0.0 }),
        ("A_upper_after_add_median_kb", median(a.upper_after_add_kb.clone())),
        ("A_upper_after_add_files_last", a.upper_after_add_files.last().copied().unwrap_or(0) as f64),
        ("A_status_cold_ms", median(a.status_cold_ms.clone())),
        ("A_status_hot_ms", median(a.status_hot_ms.clone())),
        ("A_status_dirty", a.status_dirty.len() as f64),
        ("A_status_dirty_lines", a.status_dirty_lines as f64),
        ("A_diff_ms", median(a.diff_ms.clone())),
        ("A_commit_ms", median(a.commit_ms.clone())),
        ("A_remove_try1_ok", a.remove_try1_ok as f64),
        ("A_remove_try1_ms", median(a.remove_try1_ms.clone())),
        ("A_remove_try1_left_files", a.upper_files_after_try1 as f64),
        ("A_remove_fallback_ms", median(a.remove_fallback_ms.clone())),
        ("A_remove_daemon_ok", if a.remove_daemon_ok { 1.0 } else { 0.0 }),
        ("A_remove_daemon_ms", a.remove_daemon_ms),
        ("A_upper_total_commit_kb", a.upper_total_commit_kb),
        ("B_add_ms", median(b.add_ms.clone())),
        ("B_seed_ms", median(b.seed_ms.clone())),
        ("B_clear_ms", median(b.clear_ms.clone())),
        ("B_checkout_ms", median(b.checkout_ms.clone())),
        ("B_checkout_ok", (b.checkout_ms.len() - b.checkout_fail_out.len()) as f64),
        ("B_upper_after_checkout_median_kb", median(b.upper_after_checkout_kb.clone())),
        ("B_upper_after_checkout_files_first", b.upper_after_checkout_files.first().copied().unwrap_or(0) as f64),
        ("B_status_cold_ms", median(b.status_cold_ms.clone())),
        ("B_status_hot_ms", median(b.status_hot_ms.clone())),
        ("B_status_dirty", b.status_dirty.len() as f64),
        ("B_status_new_misses", b.status_new_misses as f64),
        ("B_diff_ms", median(b.diff_ms.clone())),
        ("B_commit_ms", median(b.commit_ms.clone())),
        ("B_first_read_ms", b.first_read_ms),
        ("B_first_read_count", b.first_read_count as f64),
        ("B_first_read_misses", b.first_read_misses as f64),
        ("B_first_read_kb", b.first_read_kb),
        ("B_cache_files", b.cache_files as f64),
        ("B_cache_kb", b.cache_kb),
        ("B_cache_misses_total", b.cache_misses_total as f64),
        ("B_remove_ms", median(b.remove_ms.clone())),
        ("B_remove_fail", b.remove_fail_out.len() as f64),
        ("file_count", file_count as f64),
        ("worktrees", n as f64),
    ];
    for (k, v) in kv {
        rep.push_str(&format!("{k}={v:.3}\n"));
    }
    rep.push('\n');
    eprintln!("{rep}");
}
