//! M13/B130100 回调级 profiling harness（spec §12 续）：status 读路径归因。
//!
//! 与 `m0_benchmark.rs` 同构（进程内 daemon 驱动，真实 WinFSP + git），但
//! 重点是**归因**而非整机报告：对 4,874 文件级夹具的一个 worktree 跑
//! status 冷/热，取窗口内的回调计数/耗时（`WTFS_VFS_PROFILE=1` 由运行方
//! 预设）、NTFS 系统调用计数与进程 user/kernel CPU 差值，按
//!
//! * git status 墙钟 − Σ回调墙钟 ≈ WinFSP IPC/内核派发 + git 自身 CPU
//! * Σ回调墙钟 − ΣNTFS 墙钟 ≈ overlay 逻辑
//! * ΣNTFS 墙钟 = 元数据探测成本
//!
//! 三分。native 基线只测墙钟（无回调可归因）。
//!
//! 运行（release）：
//!
//! ```bat
//! set LIBCLANG_PATH=%LOCALAPPDATA%\wtfs\tools\llvm-23.1.5\bin
//! set WTFS_VFS_PROFILE=1
//! set WTFS_BENCH_REPO=C:\Users\notxx\AppData\Local\Temp\wtfs-bench-large-m12\repo
//! cargo test --release -p wtfs-daemon --test m13_profile -- --ignored --nocapture
//! ```
//!
//! 参数（env）：`WTFS_BENCH_REPO=<现有仓库>`（4,874 文件夹具）；缺省
//! tests/fixtures/demo-repo。`WTFS_BENCH_WTS=<worktree 数>`（缺省 2：profiling
//! 只需一个观察对象，另一个验证多 worktree 路由不串扰）。

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wtfs_core::profiling::{self, Metric};
use wtfs_core::{OverlayStore, SqliteTemplateStore, TemplateSpec, TemplateStore, WorktreeState};
use wtfs_daemon::config::DaemonConfig;
use wtfs_daemon::Daemon;

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

fn assert_git_ok(repo: &Path, args: &[&str]) -> String {
    let (ok, out) = git(repo, args);
    assert!(ok, "git {args:?} failed: {out}");
    out
}

fn timed_git(repo: &Path, args: &[&str]) -> (f64, String) {
    let start = Instant::now();
    let out = assert_git_ok(repo, args);
    (start.elapsed().as_secs_f64() * 1000.0, out)
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if values.len() % 2 == 1 {
        values[values.len() / 2]
    } else {
        (values[values.len() / 2 - 1] + values[values.len() / 2]) / 2.0
    }
}

/// 来源仓库准备：`WTFS_BENCH_REPO` 指向现有 git 仓库时 `git clone --local`
/// 快照（4,874 文件夹具的主路径）；否则（缺省 demo-repo 夹具是普通目录）
/// 复制后 init+commit——与 m0_benchmark 同口径。
fn prepare_source(base: &Path) -> (PathBuf, String, usize) {
    let source = base.join("source");
    let existing = std::env::var("WTFS_BENCH_REPO").unwrap_or_else(|_| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("tests")
            .join("fixtures")
            .join("demo-repo")
            .to_string_lossy()
            .into_owned()
    });
    assert!(Path::new(&existing).is_dir(), "WTFS_BENCH_REPO not found: {existing}");
    let mut branch = if Path::new(&existing).join(".git").exists() {
        assert_git_ok(base, &["clone", "--local", &existing, &source.to_string_lossy()]);
        assert_git_ok(&source, &["rev-parse", "--abbrev-ref", "HEAD"]).trim().to_string()
    } else {
        // 非 git 目录（demo-repo 夹具）：复制后 init。
        copy_tree(Path::new(&existing), &source);
        assert_git_ok(&source, &["init", "-b", "master"]);
        assert_git_ok(&source, &["config", "core.autocrlf", "false"]);
        assert_git_ok(&source, &["add", "."]);
        assert_git_ok(&source, &["commit", "-m", "profile source", "-q"]);
        "master".to_string()
    };
    if branch.is_empty() || branch == "HEAD" {
        branch = assert_git_ok(&source, &["rev-parse", "HEAD"]).trim().to_string();
    }
    let tracked = assert_git_ok(&source, &["ls-files"]).lines().count();
    (source, branch, tracked)
}

fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create copy root");
    for entry in std::fs::read_dir(src).expect("read source") {
        let entry = entry.expect("source entry");
        let file_type = entry.file_type().expect("file type");
        let target = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy file");
        }
    }
}

fn wait_ready(daemon: &Arc<Daemon>, name: &str, timeout: Duration) -> f64 {
    let start = Instant::now();
    let deadline = start + timeout;
    while Instant::now() < deadline {
        if daemon
            .registry()
            .get(name)
            .and_then(|overlay| overlay.store().state(overlay.worktree_id()).ok().flatten())
            == Some(WorktreeState::Ready)
        {
            return start.elapsed().as_secs_f64() * 1000.0;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timeout waiting for worktree {name} ready");
}

/// 取窗口计数（profiler 段间差值由 harness 打印分段报告天然表达——reset 后
/// 单段累计即窗口值）。
fn sum_metric(names: &[Metric]) -> (u64, f64) {
    // report() 是文本；这里直接读 COUNTERS 不可行（私有），改用 report 解析
    // 会有歧义。为让 harness 拿到数值，profiling 提供 per-metric 快照：
    let mut count = 0u64;
    let mut total_ms = 0.0;
    for m in names {
        let (c, t) = profiling::metric_snapshot(*m);
        count += c;
        total_ms += t as f64 / 1e6;
    }
    (count, total_ms)
}

fn print_section(title: &str) {
    println!();
    println!("=== profile: {title} ===");
}

#[test]
#[ignore = "requires real WinFSP runtime and git CLI; set WTFS_VFS_PROFILE=1 and run with --nocapture"]
fn m13_profile_status_read_path() {
    let wts: usize = std::env::var("WTFS_BENCH_WTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(2);
    assert!(
        profiling::enabled(),
        "run with WTFS_VFS_PROFILE=1 (profiler must be on for attribution)"
    );

    let base = tempfile::tempdir().unwrap();
    let base = base.path().to_path_buf();
    let (_source, branch, tracked) = prepare_source(&base);

    let native_repo = base.join("native-repo");
    let wtfs_repo = base.join("wtfs-repo");
    assert_git_ok(&base, &["clone", "--local", "source", &native_repo.to_string_lossy()]);
    assert_git_ok(&base, &["clone", "--local", "source", &wtfs_repo.to_string_lossy()]);

    println!("# source: {} branch {branch}, tracked files: {tracked}, worktrees: {wts}", base.join("source").display());

    // ---- native 基线（只测墙钟；native 无回调可归因） ----
    let native_worktrees = native_repo.join(".tower").join("worktrees");
    std::fs::create_dir_all(&native_worktrees).unwrap();
    let wt = native_worktrees.join("bench-wt-1");
    assert_git_ok(&native_repo, &["worktree", "add", &wt.to_string_lossy(), "-b", "bench/native-1"]);
    let (native_cold, out) = timed_git(&wt, &["status", "--porcelain"]);
    assert!(out.trim().is_empty(), "native not clean: {out:?}");
    let (native_hot, _) = timed_git(&wt, &["status", "--porcelain"]);
    print_section("native baseline (wall only)");
    println!("native status cold: {native_cold:.1} ms, hot: {native_hot:.1} ms");

    // ---- WorktreeFS：模板 + 挂载 + worktree add ×N ----
    let mut cfg = DaemonConfig::for_repo(&wtfs_repo);
    cfg.store_root = base.join("wtfs-store");
    cfg.log_dir = base.join("logs");
    cfg.poll_interval = Duration::from_millis(50);
    {
        let store = SqliteTemplateStore::open(&cfg.store_root).unwrap();
        store
            .create_or_update(&TemplateSpec::new(&cfg.repo_root, &branch))
            .expect("template creation must succeed");
    }
    let daemon = Daemon::start(cfg.clone()).expect("WinFSP mount must succeed");
    assert!(daemon.is_mounted());

    let mut names = Vec::new();
    let mut ready_ms = Vec::new();
    for i in 1..=wts {
        let name = format!("wt-prof-{i}");
        let wt_path = cfg.mount_point.join(&name);
        let (add_ms, _) = timed_git(
            &wtfs_repo,
            &["worktree", "add", &wt_path.to_string_lossy(), "-b", &format!("bench/prof-{i}")],
        );
        let ready = wait_ready(&daemon, &name, Duration::from_secs(120));
        println!("# wt-prof-{i}: add {add_ms:.0} ms, ready +{ready:.0} ms");
        ready_ms.push(add_ms + ready);
        names.push(name);
    }

    let wtfs_wt = cfg.mount_point.join(&names[0]);

    // ---- profiling：status 冷（窗口归因） ----
    profiling::reset();
    let (user0, kernel0) = profiling::process_cpu_times();
    let t0 = Instant::now();
    let (cold, out) = timed_git(&wtfs_wt, &["status", "--porcelain"]);
    assert!(out.trim().is_empty(), "wtfs status not clean: {out:?}");
    let wall_cold = t0.elapsed().as_secs_f64() * 1000.0;
    let (user1, kernel1) = profiling::process_cpu_times();
    print_section("wtfs status COLD (window attribution)");
    println!(
        "wall {:.1} ms | daemon cpu user +{} ms, kernel +{} ms",
        wall_cold,
        user1.saturating_sub(user0),
        kernel1.saturating_sub(kernel0)
    );
    println!("{}", profiling::report());
    let (cb_names, nt_names) = (
        &[
            Metric::CbGetSecurityByName, Metric::CbOpen, Metric::CbCreate, Metric::CbOverwrite,
            Metric::CbGetFileInfo, Metric::CbGetSecurity, Metric::CbCleanup, Metric::CbClose,
            Metric::CbReadDirectory, Metric::CbRead, Metric::CbWrite, Metric::CbFlush,
            Metric::CbSetBasicInfo, Metric::CbSetDelete, Metric::CbSetFileSize, Metric::CbRename,
        ],
        &[
            Metric::NtSymlinkMetadata, Metric::NtIsDirProbe, Metric::NtMetadata, Metric::NtReadDir,
            Metric::NtDirEntryMeta, Metric::NtOpenFile, Metric::NtReadFile, Metric::NtSetModified,
            Metric::NtCreateDirAll, Metric::NtCopyFile, Metric::NtRename, Metric::NtRemoveFile,
        ],
    );
    let (cb_count, cb_ms) = sum_metric(cb_names);
    let (nt_count, nt_ms) = sum_metric(nt_names);
    println!(
        "# attribution: callbacks n={} total {:.1} ms | ntfs n={} total {:.1} ms | overlay-logic ≈ {:.1} ms | ipc+git ≈ {:.1} ms",
        cb_count,
        cb_ms,
        nt_count,
        nt_ms,
        cb_ms - nt_ms,
        wall_cold - cb_ms,
    );

    // ---- profiling：status 热 ×5（中位 + 窗口归因取中位那轮） ----
    let mut hot_walls = Vec::new();
    for i in 0..5 {
        profiling::reset();
        let t = Instant::now();
        let (_, out) = timed_git(&wtfs_wt, &["status", "--porcelain"]);
        assert!(out.trim().is_empty(), "wtfs hot status not clean: {out:?}");
        let wall = t.elapsed().as_secs_f64() * 1000.0;
        hot_walls.push(wall);
        if i == 2 {
            print_section("wtfs status HOT (window attribution, median round)");
            println!("wall {wall:.1} ms");
            println!("{}", profiling::report());
            let (cb_count, cb_ms) = sum_metric(cb_names);
            let (nt_count, nt_ms) = sum_metric(nt_names);
            println!(
                "# attribution: callbacks n={} total {:.1} ms | ntfs n={} total {:.1} ms | overlay-logic ≈ {:.1} ms | ipc+git ≈ {:.1} ms",
                cb_count,
                cb_ms,
                nt_count,
                nt_ms,
                cb_ms - nt_ms,
                wall - cb_ms,
            );
        }
    }
    print_section("summary");
    println!("wtfs add+ready (mean): {:.0} ms", ready_ms.iter().sum::<f64>() / ready_ms.len() as f64);
    println!("wtfs status cold: {cold:.1} ms (git-reported) / {wall_cold:.1} ms (window)");
    println!(
        "wtfs status hot walls: {} (median {:.1} ms)",
        hot_walls.iter().map(|v| format!("{v:.1}")).collect::<Vec<_>>().join(", "),
        median(&mut hot_walls),
    );
    println!("native status cold: {native_cold:.1} ms, hot: {native_hot:.1} ms");

    daemon.shutdown();
}
