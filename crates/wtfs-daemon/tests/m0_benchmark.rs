//! M0 基准 harness（spec §12）：原生 worktree vs WorktreeFS。
//!
//! 以 `#[ignore]` 集成测试实现（真实 WinFSP + git），daemon 经库 API 进程内
//! 驱动（`Daemon::start`，无 detached 进程、无命名管道），对同一来源仓库的
//! 两个独立 clone 分别循环 `git worktree add` → 创建耗时 → 磁盘（可见树/
//! upper）→ `git status` 冷/热 → `git worktree remove`，最后打印 markdown
//! 报告（`=== bench report ===` ... `=== end ===`，供 scripts/bench-m0.ps1
//! 提取落盘）。
//!
//! 运行（release，避免 debug 版 daemon 低估 FS 性能）：
//!
//! ```bat
//! set LIBCLANG_PATH=%LOCALAPPDATA%\wtfs\tools\llvm-23.1.5\bin
//! cargo test --release -p wtfs-daemon --test m0_benchmark -- --ignored --nocapture
//! ```
//!
//! 参数（env）：`WTFS_BENCH_REPO=<现有仓库路径>`（缺省用
//! `tests/fixtures/demo-repo`，按 `git clone --local` 快照，不碰原仓库）；
//! `WTFS_BENCH_ITERS=<迭代数>`（缺省 3，取中位数）。

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wtfs_core::{OverlayStore, SqliteTemplateStore, TemplateSpec, TemplateStore, WorktreeState};
use wtfs_daemon::config::DaemonConfig;
use wtfs_daemon::Daemon;

const REPORT_BEGIN: &str = "=== bench report ===";
const REPORT_END: &str = "=== end ===";

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

/// 递归统计目录下的文件数与字节数（经挂载点统计即“用户可见树”）。
fn count_tree(dir: &Path) -> (usize, u64) {
    let mut files = 0;
    let mut bytes = 0u64;
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
    (files, bytes)
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if values.len() % 2 == 1 {
        values[values.len() / 2]
    } else {
        (values[values.len() / 2 - 1] + values[values.len() / 2]) / 2.0
    }
}

fn fmt_ms(v: f64) -> String {
    format!("{v:.0} ms")
}

fn fmt_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
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

/// 来源仓库：`WTFS_BENCH_REPO` 指向现有仓库则 `git clone --local` 快照；
/// 否则从 tests/fixtures/demo-repo 复制后 init+commit。返回（路径、分支、
/// head、tracked 文件数）。
fn prepare_source(base: &Path) -> (PathBuf, String, String, usize) {
    let source = base.join("source");
    let mut branch;
    if let Ok(existing) = std::env::var("WTFS_BENCH_REPO") {
        assert!(
            Path::new(&existing).is_dir(),
            "WTFS_BENCH_REPO not found: {existing}"
        );
        assert_git_ok(base, &["clone", "--local", &existing, &source.to_string_lossy()]);
        // M0 模板创建拒绝 submodule gitlink（spec §11.2：submodule 回退普通
        // worktree add）。基准在本地 clone 内移除 gitlink（不碰原仓库），
        // 其余 tracked 文件照常测量。quotePath=false 让非 ASCII 路径按原样
        // 输出（git 默认八进制转义并加引号，无法直接作为 pathspec）。
        let ls_out = Command::new("git")
            .arg("-c")
            .arg("core.quotePath=false")
            .arg("-C")
            .arg(&source)
            .args(["ls-files", "--stage"])
            .output()
            .expect("git CLI must be available");
        assert!(ls_out.status.success(), "git ls-files --stage failed");
        let stage = String::from_utf8_lossy(&ls_out.stdout).into_owned();
        let gitlinks: Vec<&str> = stage
            .lines()
            .filter(|line| line.starts_with("160000 "))
            .filter_map(|line| line.split('\t').nth(1))
            .filter(|path| !path.is_empty())
            .collect();
        if !gitlinks.is_empty() {
            eprintln!(
                "bench: dropping {} submodule gitlink(s) from clone (M0 template rejects submodule entries)",
                gitlinks.len()
            );
            let mut args = vec!["rm", "--cached", "-q", "--"];
            args.extend(gitlinks.iter().copied());
            assert_git_ok(&source, &args);
            assert_git_ok(&source, &["commit", "-m", "bench: drop submodule gitlinks", "-q"]);
        }
        branch = assert_git_ok(&source, &["rev-parse", "--abbrev-ref", "HEAD"]).trim().to_string();
    } else {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("tests")
            .join("fixtures")
            .join("demo-repo")
            .canonicalize()
            .expect("fixture dir must exist");
        copy_tree(&fixture, &source);
        assert_git_ok(&source, &["init", "-b", "master"]);
        assert_git_ok(&source, &["config", "core.autocrlf", "false"]);
        assert_git_ok(&source, &["add", "."]);
        assert_git_ok(&source, &["commit", "-m", "bench source", "-q"]);
        branch = "master".to_string();
    }
    if branch == "HEAD" {
        branch = assert_git_ok(&source, &["rev-parse", "HEAD"]).trim().to_string();
    }
    let head = assert_git_ok(&source, &["rev-parse", "HEAD"]).trim().to_string();
    let tracked = assert_git_ok(&source, &["ls-files"]).lines().count();
    (source, branch, head, tracked)
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

fn wait_unregistered(daemon: &Arc<Daemon>, name: &str, timeout: Duration) -> f64 {
    let start = Instant::now();
    let deadline = start + timeout;
    while Instant::now() < deadline {
        if daemon.registry().get(name).is_none() {
            return start.elapsed().as_secs_f64() * 1000.0;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timeout waiting for overlay {name} unregistered");
}

fn clean_status(repo: &Path, what: &str) -> (f64, f64) {
    let (cold, out) = timed_git(repo, &["status", "--porcelain"]);
    assert!(
        out.trim().is_empty(),
        "{what} not clean after checkout: {out:?}"
    );
    let (hot, _) = timed_git(repo, &["status", "--porcelain"]);
    (cold, hot)
}

fn is_reparse(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}

#[test]
#[ignore = "requires real WinFSP runtime and git CLI; prints a markdown report with --nocapture"]
fn m0_benchmark_native_vs_worktreefs() {
    let iters: usize = std::env::var("WTFS_BENCH_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(3);
    let source_desc = std::env::var("WTFS_BENCH_REPO").unwrap_or_else(|_| "tests/fixtures/demo-repo".to_string());

    let base = tempfile::tempdir().unwrap();
    let base = base.path().to_path_buf();
    let (_source, branch, head, tracked) = prepare_source(&base);

    // 两组内容一致的 clone：native 基线与 WorktreeFS 候选互不共享状态。
    let native_repo = base.join("native-repo");
    let wtfs_repo = base.join("wtfs-repo");
    assert_git_ok(&base, &["clone", "--local", "source", &native_repo.to_string_lossy()]);
    assert_git_ok(&base, &["clone", "--local", "source", &wtfs_repo.to_string_lossy()]);

    // ---- native 基线 ----------------------------------------------------
    let mut native_add = Vec::new();
    let mut native_cold = Vec::new();
    let mut native_hot = Vec::new();
    let mut native_remove = Vec::new();
    let mut native_tree_bytes = Vec::new();
    let mut native_tree_files = Vec::new();
    let native_worktrees = native_repo.join(".tower").join("worktrees");
    std::fs::create_dir_all(&native_worktrees).unwrap();
    for i in 1..=iters {
        let wt = native_worktrees.join(format!("bench-wt-{i}"));
        let (ms, _) = timed_git(&native_repo, &["worktree", "add", &wt.to_string_lossy(), "-b", &format!("bench/native-{i}")]);
        native_add.push(ms);
        let (files, bytes) = count_tree(&wt);
        native_tree_files.push(files);
        native_tree_bytes.push(bytes);
        let (cold, hot) = clean_status(&wt, &format!("native bench-wt-{i}"));
        native_cold.push(cold);
        native_hot.push(hot);
        let (ms, _) = timed_git(&native_repo, &["worktree", "remove", &wt.to_string_lossy()]);
        native_remove.push(ms);
        assert_git_ok(&native_repo, &["branch", "-D", &format!("bench/native-{i}")]);
    }

    // ---- WorktreeFS（daemon 进程内驱动） --------------------------------
    let mut cfg = DaemonConfig::for_repo(&wtfs_repo);
    cfg.store_root = base.join("wtfs-store");
    cfg.log_dir = base.join("logs");
    cfg.poll_interval = Duration::from_millis(50);

    let (template_ms, _template) = {
        let start = Instant::now();
        let store = SqliteTemplateStore::open(&cfg.store_root).unwrap();
        let t = store
            .create_or_update(&TemplateSpec::new(&cfg.repo_root, &branch))
            .expect("template creation must succeed");
        (start.elapsed().as_secs_f64() * 1000.0, t)
    };
    let (mount_ms, daemon) = {
        let start = Instant::now();
        let daemon = Daemon::start(cfg.clone()).expect(
            "WinFSP directory mount must succeed (is the WinFSP runtime installed?)",
        );
        (start.elapsed().as_secs_f64() * 1000.0, daemon)
    };
    assert!(daemon.is_mounted());

    let mut wtfs_add = Vec::new();
    let mut wtfs_ready = Vec::new();
    let mut wtfs_cold = Vec::new();
    let mut wtfs_hot = Vec::new();
    let mut wtfs_remove = Vec::new();
    let mut wtfs_unregister = Vec::new();
    let mut wtfs_upper_bytes = Vec::new();
    let mut wtfs_tree_bytes = Vec::new();
    let mut wtfs_tree_files = Vec::new();
    for i in 1..=iters {
        let name = format!("wt-bench-{i}");
        let wt = cfg.mount_point.join(&name);
        let (ms, _) = timed_git(&wtfs_repo, &["worktree", "add", &wt.to_string_lossy(), "-b", &format!("bench/wtfs-{i}")]);
        wtfs_add.push(ms);
        let ready_ms = wait_ready(&daemon, &name, Duration::from_secs(30));
        wtfs_ready.push(ready_ms);

        let overlay = daemon.registry().get(&name).expect("overlay must be registered");
        let upper = cfg
            .store_root
            .join("worktrees")
            .join(format!("wt-{}", overlay.worktree_id().0))
            .join("upper");
        let (_, upper_bytes) = count_tree(&upper);
        wtfs_upper_bytes.push(upper_bytes);
        let (files, bytes) = count_tree(&wt);
        wtfs_tree_files.push(files);
        wtfs_tree_bytes.push(bytes);

        let (cold, hot) = clean_status(&wt, &format!("wtfs {name}"));
        wtfs_cold.push(cold);
        wtfs_hot.push(hot);

        let (ms, _) = timed_git(&wtfs_repo, &["worktree", "remove", &wt.to_string_lossy()]);
        wtfs_remove.push(ms);
        wtfs_unregister.push(wait_unregistered(&daemon, &name, Duration::from_secs(10)));
        assert_git_ok(&wtfs_repo, &["branch", "-D", &format!("bench/wtfs-{i}")]);
    }

    // 卸载收尾：挂载点恢复空目录（Tower 契约）。junction 删除在 FS 侧异步
    // 完成，补建可能竞态失败（见 finding：mount.rs restore 竞态）——此处
    // 等待并兜底补建，保证契约成立。
    daemon.shutdown();
    assert!(
        !is_reparse(&cfg.mount_point),
        "junction must be gone after unmount"
    );
    let restore_deadline = Instant::now() + Duration::from_secs(5);
    while !cfg.mount_point.is_dir() && Instant::now() < restore_deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if !cfg.mount_point.is_dir() {
        std::fs::create_dir_all(&cfg.mount_point).expect("restore empty mount dir");
    }
    assert!(cfg.mount_point.is_dir(), "empty mount dir restored for Tower");

    // ---- 报告 ------------------------------------------------------------
    let native_tree_bytes_total: u64 = native_tree_bytes.iter().sum();
    let wtfs_tree_bytes_total: u64 = wtfs_tree_bytes.iter().sum();
    let wtfs_upper_bytes_total: u64 = wtfs_upper_bytes.iter().sum();
    let report = vec![
        REPORT_BEGIN.to_string(),
        String::new(),
        format!("- source: {source_desc}, branch {branch} @ {}", &head[..12.min(head.len())]),
        format!("- tracked files: {tracked}; iterations: {iters} (median reported)"),
        format!("- wtfs store: upper mean {} / tree-through-mount mean {}", fmt_size(wtfs_upper_bytes_total / iters as u64), fmt_size(wtfs_tree_bytes_total / iters as u64)),
        String::new(),
        "| metric | native worktree | WorktreeFS |".to_string(),
        "|---|---|---|".to_string(),
        format!("| worktree add (median) | {} | {} |", fmt_ms(median(&mut native_add)), fmt_ms(median(&mut wtfs_add))),
        format!("| add + ready switch (median) | - | {} |", fmt_ms(median(&mut wtfs_add) + median(&mut wtfs_ready))),
        format!("| disk: worktree tree (mean) | {} ({} files) | {} ({} files) |", fmt_size(native_tree_bytes_total / iters as u64), native_tree_files[0], fmt_size(wtfs_tree_bytes_total / iters as u64), wtfs_tree_files[0]),
        format!("| disk: private upper store (mean) | - | {} |", fmt_size(wtfs_upper_bytes_total / iters as u64)),
        format!("| git status cold (median) | {} | {} |", fmt_ms(median(&mut native_cold)), fmt_ms(median(&mut wtfs_cold))),
        format!("| git status hot (median) | {} | {} |", fmt_ms(median(&mut native_hot)), fmt_ms(median(&mut wtfs_hot))),
        format!("| worktree remove (median) | {} | {} |", fmt_ms(median(&mut native_remove)), fmt_ms(median(&mut wtfs_remove))),
        format!("| one-off: template + mount | - | {} + {} |", fmt_ms(template_ms), fmt_ms(mount_ms)),
        String::new(),
        "## Raw iterations".to_string(),
        String::new(),
        format!("- native add ms: {}", join_f(&native_add)),
        format!("- native status cold/hot ms: {} / {}", join_f(&native_cold), join_f(&native_hot)),
        format!("- native remove ms: {}", join_f(&native_remove)),
        format!("- native tree bytes: {}", native_tree_bytes.iter().map(|b| b.to_string()).collect::<Vec<_>>().join(", ")),
        format!("- wtfs add ms: {}", join_f(&wtfs_add)),
        format!("- wtfs ready-wait ms: {}", join_f(&wtfs_ready)),
        format!("- wtfs status cold/hot ms: {} / {}", join_f(&wtfs_cold), join_f(&wtfs_hot)),
        format!("- wtfs remove ms: {}", join_f(&wtfs_remove)),
        format!("- wtfs unregister-wait ms: {}", join_f(&wtfs_unregister)),
        format!("- wtfs upper bytes: {}", wtfs_upper_bytes.iter().map(|b| b.to_string()).collect::<Vec<_>>().join(", ")),
        format!("- wtfs tree-through-mount bytes: {}", wtfs_tree_bytes.iter().map(|b| b.to_string()).collect::<Vec<_>>().join(", ")),
        REPORT_END.to_string(),
    ];
    for line in report {
        println!("{line}");
    }
}

fn join_f(values: &[f64]) -> String {
    values.iter().map(|v| format!("{v:.1}")).collect::<Vec<_>>().join(", ")
}
