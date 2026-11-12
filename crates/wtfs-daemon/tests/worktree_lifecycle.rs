//! worktree 全生命周期集成测试（M5，spec §6.3/§15.2）。真实 WinFSP 挂载 +
//! 夹具仓库（`tests/fixtures/demo-repo`，复制到 temp 后 `git init`），走通
//! 全链路：mount → `git worktree add` → `git status` 干净 → dedupe upper
//! 上界 → `git diff`（含覆写截断读回）→ `git add`/`git commit` →
//! `git worktree remove` → 卸载 → gc 兜底。
//!
//! 默认 `#[ignore]`：需要本机 WinFSP 运行时与 git CLI。运行：
//! `cargo test -p wtfs-daemon --test worktree_lifecycle -- --ignored`

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

/// 把 `tests/fixtures/demo-repo` 复制到 `<base>/repo` 并 `git init` + 首次
/// 提交（autocrlf 关闭，夹具内容 LF ASCII，检出字节级稳定）。
fn init_fixture_repo(base: &Path) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("demo-repo");
    let src = src.canonicalize().expect("fixture dir must exist");
    let repo = base.join("repo");
    copy_tree(&src, &repo);

    assert_git_ok(&repo, &["init", "-b", "master"]);
    assert_git_ok(&repo, &["config", "core.autocrlf", "false"]);
    assert_git_ok(&repo, &["add", "."]);
    assert_git_ok(&repo, &["commit", "-m", "init", "-q"]);
    repo
}

fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create fixture copy root");
    for entry in std::fs::read_dir(src).expect("read fixture source") {
        let entry = entry.expect("fixture entry");
        let file_type = entry.file_type().expect("fixture file type");
        let target = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy fixture file");
        }
    }
}

/// 测试专用的 daemon 配置：store/log 全落 tempdir，管道名带 pid 防冲突，
/// 轮询加速到 100ms。
fn test_config(base: &Path, repo: &Path) -> DaemonConfig {
    let mut cfg = DaemonConfig::for_repo(repo);
    cfg.store_root = base.join("store");
    cfg.log_dir = base.join("logs");
    cfg.pipe_name = format!(r"\\.\pipe\wtfs-test-lifecycle-{}", std::process::id());
    cfg.poll_interval = Duration::from_millis(100);
    cfg
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

fn count_files(dir: &Path) -> (usize, u64) {
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

fn is_reparse(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}

#[test]
#[ignore = "requires real WinFSP runtime and git CLI"]
fn worktree_lifecycle_mount_add_status_dedupe_diff_commit_remove() {
    let base = tempfile::tempdir().unwrap();
    let base = base.path().to_path_buf();
    let repo = init_fixture_repo(&base);
    let cfg = test_config(&base, &repo);

    let store = SqliteTemplateStore::open(&cfg.store_root).unwrap();
    let template = store
        .create_or_update(&TemplateSpec::new(&cfg.repo_root, "master"))
        .expect("template creation must succeed");
    assert!(store.files(template.id).unwrap().len() >= 9, "fixture has 10 files");

    // Tower 契约：预建空挂载点目录（daemon→VFS 负责移除/重建/恢复）。
    std::fs::create_dir_all(&cfg.mount_point).unwrap();
    let daemon = Daemon::start(cfg.clone()).expect(
        "WinFSP directory mount must succeed (is the WinFSP runtime installed?)",
    );
    assert!(daemon.is_mounted());

    // ---- git worktree add 经挂载 FS（mkdir auto-register → checkout dedupe）。
    let worktree = cfg.mount_point.join("wt-1");
    assert_git_ok(&repo, &["worktree", "add", &worktree.to_string_lossy(), "-b", "feat/lifecycle"]);
    wait_ready(&daemon, "wt-1");

    // ---- git status 干净（固定 stat + config.worktree 对齐）。
    assert_eq!(status_of(&worktree), "", "worktree must be clean after checkout");

    // lower 视图：根文件与两层嵌套文件都能读，内容与夹具一致。
    assert_eq!(
        std::fs::read(worktree.join("README.md")).unwrap(),
        std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("tests")
                .join("fixtures")
                .join("demo-repo")
                .join("README.md"),
        )
        .unwrap(),
    );
    assert_eq!(
        std::fs::read(worktree.join("src").join("util").join("math.rs")).unwrap(),
        b"//! Deterministic arithmetic used by the fixture binary and tests.\n\npub fn add(a: u32, b: u32) -> u32 {\n    a + b\n}\n\npub fn mul(a: u32, b: u32) -> u32 {\n    a * b\n}\n",
    );

    // ---- dedupe 上界：checkout 内容全部与模板一致，upper 只剩 `.git` 指针。
    let upper = cfg.store_root.join("worktrees").join("wt-1").join("upper");
    let (upper_files, upper_bytes) = count_files(&upper);
    assert!(
        upper_files <= 2,
        "upper must be nearly empty after dedupe, got {upper_files} files"
    );
    assert!(
        upper_bytes < 512,
        "upper bytes must be tiny (only the .git pointer), got {upper_bytes}"
    );

    // ---- git diff：干净树空 diff；编辑后 diff 捕获改动。
    let (ok, out) = git(&worktree, &["diff", "--exit-code"]);
    assert!(ok, "clean tree must have empty diff: {out}");
    let math_rs = worktree.join("src").join("util").join("math.rs");
    std::fs::write(&math_rs, "//! Deterministic arithmetic used by the fixture binary and tests.\n\npub fn add(a: u32, b: u32) -> u32 {\n    a.wrapping_add(b)\n}\n\npub fn mul(a: u32, b: u32) -> u32 {\n    a * b\n}\n").unwrap();
    let (ok, out) = git(&worktree, &["status", "--porcelain"]);
    assert!(ok, "git status after edit failed: {out}");
    assert!(out.contains("M src/util/math.rs"), "expected ` M src/util/math.rs`, got {out:?}");
    let (ok, diff) = git(&worktree, &["diff", "--", "src/util/math.rs"]);
    assert!(ok, "git diff failed: {diff}");
    assert!(diff.contains("wrapping_add"), "diff must capture the edit: {diff:?}");

    // M6/B060100 回归：对已有 upper 文件用更短内容覆写，读回必须字节级
    // 等于新内容（EOF 截断到位，不得残留旧尾部），diff/status 随之更新。
    std::fs::write(&math_rs, "//! trimmed\n\npub fn add(a: u32, b: u32) -> u32 {\n    a + b\n}\n\npub fn mul(a: u32, b: u32) -> u32 {\n    a * b\n}\n").unwrap();
    assert_eq!(
        std::fs::read(&math_rs).unwrap(),
        b"//! trimmed\n\npub fn add(a: u32, b: u32) -> u32 {\n    a + b\n}\n\npub fn mul(a: u32, b: u32) -> u32 {\n    a * b\n}\n",
        "shorter in-place overwrite must truncate EOF"
    );

    // ---- git add / commit：编辑 + 新增文件一起提交，树回到干净。
    let notes_md = worktree.join("docs").join("notes.md");
    std::fs::write(&notes_md, "# notes\n\nadded in the lifecycle test\n").unwrap();
    assert_git_ok(&worktree, &["add", "-A"]);
    let (ok, out) = git(&worktree, &["status", "--porcelain"]);
    assert!(ok, "git status after add failed: {out}");
    assert!(out.contains("M  src/util/math.rs"), "staged modification expected, got {out:?}");
    assert!(out.contains("A  docs/notes.md"), "staged new file expected, got {out:?}");
    assert_git_ok(&worktree, &["commit", "-m", "lifecycle: edit math + add notes", "-q"]);
    assert_eq!(status_of(&worktree), "", "tree must be clean after commit");
    let (ok, subject) = git(&worktree, &["log", "-1", "--format=%s"]);
    assert!(ok, "git log failed: {subject}");
    assert_eq!(subject.trim(), "lifecycle: edit math + add notes");
    let (ok, show) = git(&worktree, &["show", "--stat", "--format="]);
    assert!(ok, "git show failed: {show}");
    assert!(show.contains("src/util/math.rs"), "commit must contain the edit: {show:?}");
    assert!(show.contains("docs/notes.md"), "commit must contain the new file: {show:?}");

    // ---- git worktree remove：M6 已放开 worktree 根 set_delete，必须直接
    // 成功；watcher 按元数据消失注销 overlay。
    assert_git_ok(&repo, &["worktree", "remove", &worktree.to_string_lossy()]);
    wait_for(
        Duration::from_secs(10),
        || daemon.registry().get("wt-1").is_none(),
        "overlay unregistered after worktree remove",
    );

    // ---- 卸载：挂载点恢复为空目录（Tower 契约）。
    daemon.shutdown();
    wait_for(Duration::from_secs(10), || !daemon.is_mounted(), "daemon unmounted");
    assert!(!is_reparse(&cfg.mount_point), "junction must be gone after unmount");
    assert!(cfg.mount_point.is_dir(), "empty mount dir restored for Tower");

    // ---- gc 兜底：注销后 upper 变孤儿，dry-run 可见，apply 清掉。
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
