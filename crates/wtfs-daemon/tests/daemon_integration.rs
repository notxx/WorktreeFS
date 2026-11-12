//! daemon 集成测试（M4/M0 验收，spec §6.3）。
//!
//! * [`daemon_watches_index_and_switches_ready_without_mount`]：无需 WinFSP，
//!   真实 git 仓库 + `git worktree add`（daemon 未挂载时退化为真实目录），
//!   验证 watcher 全链路：index 关闭 → config.worktree → extensions 开关 →
//!   `mark_ready` → `git status` 干净 → 修改报 ` M` → remove 后注销。
//! * [`real_mount_git_worktree_add_through_daemon`]（`#[ignore]`，需本机
//!   WinFSP 运行时 + git CLI）：完整 M0 验收——daemon 挂载
//!   `<repo>/.tower/worktrees`，`git worktree add` 经 FS dedupe，ready 后
//!   `git status` 干净，编辑报 ` M`，remove/unmount 干净退出。
//!   运行：`cargo test -p wtfs-daemon --test daemon_integration -- --ignored`

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wtfs_core::{OverlayStore, SqliteTemplateStore, TemplateSpec, TemplateStore, WorktreeState};
use wtfs_daemon::config::DaemonConfig;
use wtfs_daemon::Daemon;

fn git(repo: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("git")
        .arg("-c")
        .arg("user.name=wtfs-test")
        .arg("-c")
        .arg("user.email=wtfs-test@example.com")
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

fn assert_git_ok(repo: &Path, args: &[&str]) {
    let (ok, out) = git(repo, args);
    assert!(ok, "git {args:?} failed: {out}");
}

/// 最小仓库：3 个文件 + 一次提交（autocrlf 关闭，内容字节级稳定）。
fn init_repo(base: &Path) -> PathBuf {
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    assert_git_ok(&repo, &["init", "-b", "master"]);
    assert_git_ok(&repo, &["config", "core.autocrlf", "false"]);
    std::fs::write(repo.join("README.md"), "# hello\n").unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn f() -> u32 { 1 }\n").unwrap();
    std::fs::write(repo.join("src/main.rs"), "fn main() {}\n").unwrap();
    assert_git_ok(&repo, &["add", "."]);
    assert_git_ok(&repo, &["commit", "-m", "init", "-q"]);
    repo
}

/// 测试专用的 daemon 配置：store/log 全落 tempdir，管道名带 pid 防冲突，
/// 轮询加速到 100ms。
fn test_config(base: &Path, repo: &Path) -> DaemonConfig {
    let mut cfg = DaemonConfig::for_repo(repo);
    cfg.store_root = base.join("store");
    cfg.log_dir = base.join("logs");
    cfg.pipe_name = format!(r"\\.\pipe\wtfs-test-e2e-{}", std::process::id());
    cfg.poll_interval = Duration::from_millis(100);
    cfg
}

fn make_template(cfg: &DaemonConfig, base_ref: &str) {
    let store = SqliteTemplateStore::open(&cfg.store_root).unwrap();
    let template = store
        .create_or_update(&TemplateSpec::new(&cfg.repo_root, base_ref))
        .expect("template creation must succeed");
    assert!(store.files(template.id).unwrap().len() >= 3);
}

fn wait_for(timeout: Duration, mut cond: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timeout waiting for {what}");
}

fn wait_ready(daemon: &Arc<Daemon>, name: &str) {
    wait_for(
        Duration::from_secs(30),
        || {
            daemon
                .registry()
                .get(name)
                .and_then(|overlay| overlay.store().state(overlay.worktree_id()).ok().flatten())
                == Some(WorktreeState::Ready)
        },
        &format!("worktree {name} ready"),
    );
}

fn status_of(repo: &Path) -> String {
    let (ok, out) = git(repo, &["status", "--porcelain"]);
    assert!(ok, "git status failed: {out}");
    out.trim().to_string()
}

#[test]
fn daemon_watches_index_and_switches_ready_without_mount() {
    let base = tempfile::tempdir().unwrap();
    let base = base.path().to_path_buf();
    let repo = init_repo(&base);
    let cfg = test_config(&base, &repo);
    make_template(&cfg, "master");

    // daemon 不挂载（无需 WinFSP）：watcher + 注册表照常工作。
    let daemon = Daemon::start_without_mount(cfg.clone()).unwrap();

    // 模拟 Tower：普通 `git worktree add`（此时 FS 不在，写真实目录）。
    let worktrees = repo.join(".tower").join("worktrees");
    std::fs::create_dir_all(&worktrees).unwrap();
    let worktree = worktrees.join("wt-1");
    assert_git_ok(&repo, &["worktree", "add", &worktree.to_string_lossy(), "-b", "feat/e2e"]);

    wait_ready(&daemon, "wt-1");

    // config.worktree 三项 + extensions.worktreeConfig=true（spec §6.3.5）。
    let config_worktree =
        std::fs::read_to_string(repo.join(".git").join("worktrees").join("wt-1").join("config.worktree"))
            .expect("daemon must write config.worktree");
    for (key, value) in wtfs_daemon::REQUIRED_KEYS {
        let needle = format!("{key} = {value}").to_ascii_lowercase();
        assert!(
            config_worktree.to_ascii_lowercase().contains(&needle),
            "missing {key} in config.worktree; content: {config_worktree:?}"
        );
    }
    let common_config = std::fs::read_to_string(repo.join(".git").join("config")).unwrap();
    assert!(common_config.to_ascii_lowercase().contains("worktreeconfig = true"));

    // 验收：git status 干净。
    assert_eq!(status_of(&worktree), "", "worktree must be clean");

    // 编辑 → ` M`。
    std::fs::write(worktree.join("README.md"), "# changed\n").unwrap();
    let status = status_of(&worktree);
    assert!(status.contains("M README.md"), "expected ` M README.md`, got {status:?}");

    // 还原后 remove → watcher 注销。
    std::fs::write(worktree.join("README.md"), "# hello\n").unwrap();
    assert_eq!(status_of(&worktree), "");
    assert_git_ok(&repo, &["worktree", "remove", &worktree.to_string_lossy()]);
    wait_for(
        Duration::from_secs(10),
        || daemon.registry().get("wt-1").is_none(),
        "overlay unregistered after worktree remove",
    );

    daemon.shutdown();
}

#[test]
#[ignore = "requires real WinFSP runtime and git CLI"]
fn real_mount_git_worktree_add_through_daemon() {
    let base = tempfile::tempdir().unwrap();
    let base = base.path().to_path_buf();
    let repo = init_repo(&base);
    let cfg = test_config(&base, &repo);
    make_template(&cfg, "master");

    // Tower 契约：预建空挂载点目录（daemon→VFS 负责移除/重建/恢复）。
    std::fs::create_dir_all(&cfg.mount_point).unwrap();

    let daemon = Daemon::start(cfg.clone()).expect(
        "WinFSP directory mount must succeed (is the WinFSP runtime installed?)",
    );
    assert!(daemon.is_mounted());
    assert!(cfg.mount_point.exists(), "junction must exist while mounted");

    // `git worktree add` 经挂载 FS：mkdir 自动注册 → checkout dedupe。
    let worktree = cfg.mount_point.join("wt-1");
    assert_git_ok(&repo, &["worktree", "add", &worktree.to_string_lossy(), "-b", "feat/e2e-mount"]);

    // daemon 在 index 关闭后完成 ready 切换（无需手工 mark_ready）。
    wait_ready(&daemon, "wt-1");

    // 验收：git status 干净（固定 stat + config.worktree 对齐）。
    assert_eq!(status_of(&worktree), "", "worktree must be clean");

    // dedupe 生效：upper 只留 .git 指针（内容全部回落模板）。
    let upper = cfg.store_root.join("worktrees").join("wt-1").join("upper");
    let upper_files = count_files(&upper);
    assert!(
        upper_files <= 2,
        "upper must be nearly empty after dedupe, got {upper_files} files"
    );

    // 编辑 → ` M`。
    std::fs::write(worktree.join("README.md"), "# changed\n").unwrap();
    let status = status_of(&worktree);
    assert!(status.contains("M README.md"), "expected ` M README.md`, got {status:?}");

    // 还原：直接覆写已有 upper 文件（M6/B060100 已修复覆写截断——不再
    // 需要“先删再写”绕行），读回必须字节级一致（EOF 截断到位）。
    std::fs::write(worktree.join("README.md"), "# hello\n").unwrap();
    assert_eq!(
        std::fs::read(worktree.join("README.md")).unwrap(),
        b"# hello\n",
        "in-place overwrite must truncate to new content"
    );
    assert_eq!(status_of(&worktree), "");

    // remove 必须直接成功（M6/B060100 已放开 worktree 根 set_delete）；
    // watcher 按元数据消失注销 overlay，upper 变孤儿由 gc 回收。
    assert_git_ok(&repo, &["worktree", "remove", "--force", &worktree.to_string_lossy()]);
    wait_for(
        Duration::from_secs(10),
        || daemon.registry().get("wt-1").is_none(),
        "overlay unregistered after worktree remove",
    );

    daemon.shutdown();
    wait_for(
        Duration::from_secs(10),
        || !daemon.is_mounted(),
        "daemon unmounted",
    );
    assert!(
        !is_reparse(&cfg.mount_point),
        "junction must be gone after unmount"
    );
    assert!(cfg.mount_point.is_dir(), "empty mount dir restored for Tower");

    // gc 兜底：注销后 upper 变孤儿，dry-run 能看到，apply 清掉。
    let gc_ctx = wtfs_daemon::GcContext {
        mount_root: &cfg.mount_point,
        store_root: &cfg.store_root,
        registry: None,
        mounted: false,
        max_age: Duration::from_secs(7 * 24 * 3600),
    };
    let gc_plan = wtfs_daemon::gc::plan(&gc_ctx).unwrap();
    assert!(
        gc_plan.orphan_uppers.iter().any(|u| u.worktree_id == 1),
        "orphan upper must be planned, got {}",
        gc_plan.summary()
    );
    let report = wtfs_daemon::gc::apply(&gc_ctx, &gc_plan).unwrap();
    assert_eq!(report.removed_uppers, 1, "report: {}", report.summary());
}

fn count_files(dir: &Path) -> usize {
    let mut count = 0;
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
                count += 1;
            }
        }
    }
    count
}

fn is_reparse(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}
