//! 真实 WinFSP 挂载 + `git worktree add` 端到端测试（M0 验收路径，
//! spec §6.3）。默认 `#[ignore]`：需要本机安装 WinFSP 运行时与 git CLI；
//! `cargo test -p wtfs-vfs --test mount_integration -- --ignored` 显式运行。

#![cfg(windows)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use wtfs_core::{SqliteTemplateStore, TemplateSpec, TemplateStore};

use wtfs_vfs::{VfsConfig, VfsRegistry, mount_directory};

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

#[test]
#[ignore = "requires real WinFSP runtime and git CLI"]
fn real_mount_git_worktree_add_dedupes_and_reports_clean_status() {
    let base = tempfile::tempdir().expect("tempdir");
    let base = base.path().to_path_buf();
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();

    // 1. 真实 git 仓库：3 个模板文件 + 提交。
    std::fs::write(repo.join("README.md"), "# hello\n").unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn f() -> u32 { 1 }\n").unwrap();
    std::fs::write(repo.join("src/main.rs"), "fn main() { println!(\"hi\"); }\n").unwrap();
    let (ok, out) = git(&repo, &["init", "-b", "master"]);
    assert!(ok, "git init failed: {out}");
    // 关闭 autocrlf：让测试内容字节级稳定（模板 checkout 与挂载视图一致）。
    let (ok, out) = git(&repo, &["config", "core.autocrlf", "false"]);
    assert!(ok, "git config failed: {out}");
    let (ok, out) = git(&repo, &["add", "."]);
    assert!(ok, "git add failed: {out}");
    let (ok, out) = git(&repo, &["commit", "-m", "init", "-q"]);
    assert!(ok, "git commit failed: {out}");

    // 2. 模板：正常 checkout + blake3 清单（dedupe 的 lower 来源）。
    let store_root = base.join("store");
    let template_store = SqliteTemplateStore::open(&store_root).unwrap();
    let template = template_store
        .create_or_update(&TemplateSpec::new(&repo, "master"))
        .expect("template creation must succeed");
    assert!(template_store.files(template.id).unwrap().len() >= 3);

    // 3. 挂载 <repo>/.tower/worktrees（真实目录挂载）。
    let mount_point = repo.join(".tower").join("worktrees");
    std::fs::create_dir_all(&mount_point).unwrap();
    let cfg = VfsConfig::new(&repo, &store_root);
    let registry = Arc::new(VfsRegistry::open(cfg).unwrap());
    let volume = mount_directory(&mount_point, registry.clone())
        .expect("WinFSP directory mount must succeed (is the WinFSP runtime installed?)");

    // 4. git worktree add —— mkdir 走 auto-register，checkout 走暂存 dedupe。
    let worktree = mount_point.join("wt-1");
    let (ok, out) = git(&repo, &["worktree", "add", &worktree.to_string_lossy(), "-b", "feat/vfs-test"]);
    assert!(ok, "git worktree add failed: {out}");

    // 5. daemon（M4）在 index 关闭后调 mark_ready；测试手工触发钩子。
    assert!(registry.get("wt-1").is_some(), "auto-registration must exist");
    registry.mark_ready("wt-1").expect("mark_ready hook");

    // 6. 视图正确：模板文件经 lower 呈现。
    let readme = std::fs::read(worktree.join("README.md")).expect("read through mount");
    assert_eq!(readme, b"# hello\n");
    let librs = std::fs::read(worktree.join("src/lib.rs")).expect("read nested through mount");
    assert_eq!(librs, b"pub fn f() -> u32 { 1 }\n");
    let entries: Vec<String> = std::fs::read_dir(&worktree)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(entries.contains(&".git".to_string()));
    assert!(entries.contains(&"README.md".to_string()));
    assert!(entries.contains(&"src".to_string()));

    // 7. git status 干净（index stat 与 FS 固定 stat 对齐，spec §6.3 验收）。
    let (ok, out) = git(&worktree, &["status", "--porcelain"]);
    assert!(ok, "git status failed: {out}");
    assert_eq!(out.trim(), "", "git status must be clean, got: {out:?}");

    // 8. dedupe 生效：upper 只剩 .git 文件（每 worktree 唯一路径），其余回落 lower。
    let upper = store_root
        .join("worktrees")
        .join("wt-1")
        .join("upper");
    let (upper_files, upper_bytes) = count_files(&upper);
    assert!(
        upper_files <= 2,
        "upper must be nearly empty after dedupe (checkout content == template); got {upper_files} files: {out}",
        out = {
            let mut s = String::new();
            let mut stack = vec![upper.clone()];
            while let Some(d) = stack.pop() {
                if let Ok(rd) = std::fs::read_dir(&d) {
                    for e in rd.flatten() {
                        if e.path().is_dir() {
                            stack.push(e.path());
                        } else {
                            s.push_str(&format!("\n  {}", e.path().display()));
                        }
                    }
                }
            }
            s
        }
    );
    assert!(
        upper_bytes < 512,
        "upper bytes must be tiny (only .git pointer), got {upper_bytes}"
    );

    // 9. 写入路径：worker 修改一个文件 → upper；git status 报告修改。
    std::fs::write(worktree.join("README.md"), "# changed\n").unwrap();
    let (ok, out) = git(&worktree, &["status", "--porcelain"]);
    assert!(ok, "git status after edit failed: {out}");
    assert!(out.contains("M README.md"), "expected modification, got: {out:?}");

    // 10. 卸载。
    volume.unmount();
}
