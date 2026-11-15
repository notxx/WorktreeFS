//! M14/B140100 回调级 profiling harness（spec §12 续）：remove/unlink 路径归因。
//!
//! 与 `m13_profile.rs` 同构（进程内 daemon 驱动，真实 WinFSP + git），但窗口
//! 是 `git worktree remove`（M0 基准的 remove 计量口径，含 git 的干净检查
//! status）：取窗口内回调计数/耗时、NTFS 系统调用计数与进程 user/kernel CPU
//! 差值，按 m13 同一口径三分：
//!
//! * remove 墙钟 − Σ回调墙钟 ≈ WinFSP IPC/内核派发 + git 自身 CPU
//! * Σ回调墙钟 − ΣNTFS 墙钟 ≈ overlay 逻辑
//! * ΣNTFS 墙钟 = 元数据/删除系统调用成本
//!
//! 每次 unlink 的地板 = 单位回调次数 × 单次均值（cb.set_delete / cb.cleanup /
//! cb.close / cb.open / cb.get_security_by_name 逐个给出）。
//!
//! 运行（release）：
//!
//! ```bat
//! set LIBCLANG_PATH=%LOCALAPPDATA%\wtfs\tools\llvm-23.1.5\bin
//! set WTFS_VFS_PROFILE=1
//! set WTFS_BENCH_REPO=C:\Users\notxx\AppData\Local\Temp\wtfs-bench-large-m12\repo
//! cargo test --release -p wtfs-daemon --test m14_profile -- --ignored --nocapture
//! ```
//!
//! 参数（env）：`WTFS_BENCH_REPO=<现有仓库>`（4,874 文件夹具）；缺省
//! tests/fixtures/demo-repo。`WTFS_BENCH_WTS=<worktree 数>`（缺省 2：一个
//! profiling 观察对象，一个对照）。

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

/// 来源仓库准备：与 m13_profile 同口径（WTFS_BENCH_REPO clone --local；非 git
/// 目录复制后 init）。
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

/// remove 窗口回调/NTFS 计量分组（与 m13_profile 同一组名）。
fn cb_names() -> &'static [Metric] {
    &[
        Metric::CbGetSecurityByName, Metric::CbOpen, Metric::CbCreate, Metric::CbOverwrite,
        Metric::CbGetFileInfo, Metric::CbGetSecurity, Metric::CbCleanup, Metric::CbClose,
        Metric::CbReadDirectory, Metric::CbRead, Metric::CbWrite, Metric::CbFlush,
        Metric::CbSetBasicInfo, Metric::CbSetDelete, Metric::CbSetFileSize, Metric::CbRename,
    ]
}

fn nt_names() -> &'static [Metric] {
    &[
        Metric::NtSymlinkMetadata, Metric::NtIsDirProbe, Metric::NtMetadata, Metric::NtReadDir,
        Metric::NtDirEntryMeta, Metric::NtOpenFile, Metric::NtReadFile, Metric::NtSetModified,
        Metric::NtCreateDirAll, Metric::NtCopyFile, Metric::NtRename, Metric::NtRemoveFile,
    ]
}

fn sum_metric(names: &[Metric]) -> (u64, f64) {
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

/// 打印单条回调/NTFS 计量行（次数 + 均值 µs + 总计 ms）。
fn dump_metrics(names: &[Metric]) {
    for m in names {
        let (count, ns) = profiling::metric_snapshot(*m);
        if count == 0 {
            continue;
        }
        let mean_us = ns as f64 / count as f64 / 1e3;
        println!(
            "  {:<24} n={:<7} mean {:>8.1} µs  total {:>9.1} ms",
            m.name(),
            count,
            mean_us,
            ns as f64 / 1e6,
        );
    }
}

fn attribution(wall_ms: f64) {
    let (cb_count, cb_ms) = sum_metric(cb_names());
    let (nt_count, nt_ms) = sum_metric(nt_names());
    println!(
        "# attribution: callbacks n={} total {:.1} ms | ntfs n={} total {:.1} ms | overlay-logic ≈ {:.1} ms | ipc+git ≈ {:.1} ms",
        cb_count,
        cb_ms,
        nt_count,
        nt_ms,
        cb_ms - nt_ms,
        wall_ms - cb_ms,
    );
    print_section("callbacks (remove window)");
    dump_metrics(cb_names());
    print_section("ntfs syscalls (remove window)");
    dump_metrics(nt_names());
}

#[test]
#[ignore = "requires real WinFSP runtime and git CLI; set WTFS_VFS_PROFILE=1 and run with --nocapture"]
fn m14_profile_remove_unlink_path() {
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

    // ---- native 基线（只测墙钟） ----
    let native_worktrees = native_repo.join(".tower").join("worktrees");
    std::fs::create_dir_all(&native_worktrees).unwrap();
    let wt = native_worktrees.join("bench-wt-1");
    assert_git_ok(&native_repo, &["worktree", "add", &wt.to_string_lossy(), "-b", "bench/native-1"]);
    let (_, out) = timed_git(&wt, &["status", "--porcelain"]);
    assert!(out.trim().is_empty(), "native not clean: {out:?}");
    let (native_remove, _) = timed_git(&native_repo, &["worktree", "remove", &wt.to_string_lossy()]);
    print_section("native baseline (wall only)");
    println!("native remove: {native_remove:.1} ms");

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
    for i in 1..=wts {
        let name = format!("wt-prof-{i}");
        let wt_path = cfg.mount_point.join(&name);
        let (add_ms, _) = timed_git(
            &wtfs_repo,
            &["worktree", "add", &wt_path.to_string_lossy(), "-b", &format!("bench/prof-{i}")],
        );
        let ready = wait_ready(&daemon, &name, Duration::from_secs(120));
        println!("# wt-prof-{i}: add {add_ms:.0} ms, ready +{ready:.0} ms");
        names.push(name);
    }

    // ---- profiling：remove 窗口归因（第一个 worktree） ----
    let wtfs_wt = cfg.mount_point.join(&names[0]);
    profiling::reset();
    let (user0, kernel0) = profiling::process_cpu_times();
    let t0 = Instant::now();
    let (remove_ms, _) = timed_git(&wtfs_repo, &["worktree", "remove", &wtfs_wt.to_string_lossy()]);
    let wall_remove = t0.elapsed().as_secs_f64() * 1000.0;
    let (user1, kernel1) = profiling::process_cpu_times();
    print_section("wtfs worktree remove (window attribution)");
    println!(
        "wall {:.1} ms (git-reported {:.1} ms) | daemon cpu user +{} ms, kernel +{} ms",
        wall_remove,
        remove_ms,
        user1.saturating_sub(user0),
        kernel1.saturating_sub(kernel0),
    );
    println!("{}", profiling::report());
    attribution(wall_remove);

    // ---- 第二个 worktree：add+ready 后 remove（窗口归因，对照轮） ----
    if wts >= 2 {
        let wt2 = cfg.mount_point.join(&names[1]);
        let (_, out) = timed_git(&wt2, &["status", "--porcelain"]);
        assert!(out.trim().is_empty(), "wtfs wt2 not clean: {out:?}");
        profiling::reset();
        let t = Instant::now();
        let (remove_ms2, _) = timed_git(&wtfs_repo, &["worktree", "remove", &wt2.to_string_lossy()]);
        let wall2 = t.elapsed().as_secs_f64() * 1000.0;
        print_section("wtfs worktree remove round 2 (attribution)");
        println!("wall {wall2:.1} ms (git-reported {remove_ms2:.1} ms)");
        println!("{}", profiling::report());
        attribution(wall2);
    }

    print_section("summary");
    println!("native remove: {native_remove:.1} ms");
    let n_unlink = tracked as f64;
    if n_unlink > 0.0 {
        println!("per-unlink floor: wtfs {:#.3} ms vs native {:#.3} ms", remove_ms / n_unlink, native_remove / n_unlink);
    }

    daemon.shutdown();
}
