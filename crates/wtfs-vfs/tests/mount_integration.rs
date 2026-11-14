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

    // 9b. B060100 回归：CREATE_ALWAYS（std::fs::write）用更短内容覆写已有
    // upper 文件，读回内容必须精确等于写入内容（不得残留旧尾部）。
    std::fs::write(worktree.join("README.md"), "# hi\n").unwrap();
    let readme = std::fs::read(worktree.join("README.md")).expect("read back through mount");
    assert_eq!(
        readme, b"# hi\n",
        "shorter in-place overwrite must truncate EOF, got {readme:?}"
    );
    let (ok, out) = git(&worktree, &["status", "--porcelain"]);
    assert!(ok, "git status after shorter overwrite failed: {out}");
    assert!(out.contains("M README.md"), "still modified, got: {out:?}");

    // 10. 卸载。
    volume.unmount();
}

fn is_junction(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}

#[test]
#[ignore = "requires real WinFSP runtime and git CLI"]
fn real_mount_clean_checkout_has_zero_upper_writes_and_single_fsync_for_git_pointer() {
    // M9/B090100 验收（真实 WinFSP + git）：干净 checkout 的模板文件经
    // finish_staged 零 upper 物化、零 fsync；仅 worktree 内的 `.git` 指针
    // （模板外新路径）物化 upper 并恰好一次 fsync。
    let base = tempfile::tempdir().expect("tempdir");
    let base = base.path().to_path_buf();
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("README.md"), "# hello\n").unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn f() -> u32 { 1 }\n").unwrap();
    std::fs::write(repo.join("src/main.rs"), "fn main() { println!(\"hi\"); }\n").unwrap();
    let (ok, out) = git(&repo, &["init", "-b", "master"]);
    assert!(ok, "git init failed: {out}");
    let (ok, out) = git(&repo, &["config", "core.autocrlf", "false"]);
    assert!(ok, "git config failed: {out}");
    let (ok, out) = git(&repo, &["add", "."]);
    assert!(ok, "git add failed: {out}");
    let (ok, out) = git(&repo, &["commit", "-m", "init", "-q"]);
    assert!(ok, "git commit failed: {out}");

    let store_root = base.join("store");
    let template_store = SqliteTemplateStore::open(&store_root).unwrap();
    let template = template_store
        .create_or_update(&TemplateSpec::new(&repo, "master"))
        .expect("template creation must succeed");
    assert!(template_store.files(template.id).unwrap().len() >= 3);

    let mount_point = repo.join(".tower").join("worktrees");
    std::fs::create_dir_all(&mount_point).unwrap();
    let cfg = VfsConfig::new(&repo, &store_root);
    let registry = Arc::new(VfsRegistry::open(cfg).unwrap());
    let volume = mount_directory(&mount_point, registry.clone())
        .expect("WinFSP directory mount must succeed (is the WinFSP runtime installed?)");

    let worktree = mount_point.join("wt-1");
    let (ok, out) = git(&repo, &["worktree", "add", &worktree.to_string_lossy(), "-b", "feat/m9-io-stats"]);
    assert!(ok, "git worktree add failed: {out}");
    registry.mark_ready("wt-1").expect("mark_ready hook");

    // I/O 插桩：模板内容零物化零 fsync；`.git` 指针 1 物化 + 1 fsync。
    // （B100100 起 .git 的 create 当场物化空 upper 占位 + staged 终裁物化
    // 内容 = 2 次记录，fsync 仍恰好 1 次。）
    let overlay = registry.get("wt-1").expect("overlay registered");
    let stats = overlay.io_stats();
    assert_eq!(
        stats.upper_writes(),
        2,
        "只有 .git 指针落 upper（create 占位 + 内容终裁）；fsyncs={} upper={}",
        stats.fsyncs(),
        stats.upper_writes()
    );
    assert_eq!(stats.fsyncs(), 1, "物化恰好一次 fsync（.git 指针）");

    // 上层目录只剩 `.git` 指针；git status 干净（dedupe 语义不变）。
    let upper = store_root.join("worktrees").join("wt-1").join("upper");
    let (upper_files, _) = count_files(&upper);
    assert_eq!(upper_files, 1, "upper 只剩 .git 指针，模板内容零残留");
    let (ok, out) = git(&worktree, &["status", "--porcelain"]);
    assert!(ok, "git status failed: {out}");
    assert_eq!(out.trim(), "", "git status must be clean, got: {out:?}");

    volume.unmount();
}

#[test]
#[ignore = "requires real WinFSP runtime and git CLI"]
fn real_mount_git_worktree_add_with_empty_files_mixed_checkout() {
    // B100100 回归（真实 WinFSP + git）：模板含根级空文件、嵌套空文件与
    // 非空文件的混合检出。修复前 git worktree add 报
    // "unable to stat just-written file empty.txt: No such file or directory"
    // 与 "fatal: Could not reset index file to revision 'HEAD'"（force 检出的
    // unlink 留下 whiteout，零字节 blob 写路径无终裁）；修复后必须成功、
    // git status 干净、空文件可读可 stat，模板一致内容仍零 upper 物化、
    // 零 fsync（仅 worktree 内 `.git` 指针落 upper，一次 fsync）。
    let base = tempfile::tempdir().expect("tempdir");
    let base = base.path().to_path_buf();
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("README.md"), "# hello\n").unwrap();
    std::fs::write(repo.join("empty.txt"), b"").unwrap();
    std::fs::create_dir_all(repo.join("nested")).unwrap();
    std::fs::write(repo.join("nested/empty.txt"), b"").unwrap();
    let (ok, out) = git(&repo, &["init", "-b", "master"]);
    assert!(ok, "git init failed: {out}");
    let (ok, out) = git(&repo, &["config", "core.autocrlf", "false"]);
    assert!(ok, "git config failed: {out}");
    let (ok, out) = git(&repo, &["add", "."]);
    assert!(ok, "git add failed: {out}");
    let (ok, out) = git(&repo, &["commit", "-m", "init", "-q"]);
    assert!(ok, "git commit failed: {out}");

    let store_root = base.join("store");
    let template_store = SqliteTemplateStore::open(&store_root).unwrap();
    let template = template_store
        .create_or_update(&TemplateSpec::new(&repo, "master"))
        .expect("template creation must succeed");
    // 模板必须包含空文件条目（size 0；hash = 空 blob 的 blake3）。
    let empty_entries: Vec<_> = template_store
        .files(template.id)
        .unwrap()
        .into_iter()
        .filter(|f| f.size == 0)
        .collect();
    assert_eq!(empty_entries.len(), 2, "模板必须含两个空文件条目");

    let mount_point = repo.join(".tower").join("worktrees");
    std::fs::create_dir_all(&mount_point).unwrap();
    let cfg = VfsConfig::new(&repo, &store_root);
    let registry = Arc::new(VfsRegistry::open(cfg).unwrap());
    let volume = mount_directory(&mount_point, registry.clone())
        .expect("WinFSP directory mount must succeed (is the WinFSP runtime installed?)");

    // worktree add --detach：与基准（大仓库发现 B100100）相同的调用形态。
    let worktree = mount_point.join("wt-1");
    let (ok, out) = git(&repo, &["worktree", "add", "--detach", &worktree.to_string_lossy(), "HEAD"]);
    assert!(
        ok,
        "git worktree add with empty files must succeed (B100100); output: {out}"
    );
    assert!(
        !out.contains("unable to stat"),
        "no just-written stat errors allowed: {out}"
    );

    registry.mark_ready("wt-1").expect("mark_ready hook");

    // 空文件可读、可 stat（EOF 0）；非空文件内容正确。
    assert_eq!(
        std::fs::read(worktree.join("empty.txt")).expect("read empty.txt"),
        b"",
        "empty.txt must be readable through the mount"
    );
    assert_eq!(
        std::fs::read(worktree.join("nested/empty.txt")).expect("read nested/empty.txt"),
        b""
    );
    assert_eq!(
        std::fs::metadata(worktree.join("empty.txt"))
            .expect("stat empty.txt")
            .len(),
        0
    );
    assert_eq!(std::fs::read(worktree.join("README.md")).unwrap(), b"# hello\n");

    // git status 干净（index 与 FS 固定 stat 对齐，spec §6.3 验收）。
    let (ok, out) = git(&worktree, &["status", "--porcelain"]);
    assert!(ok, "git status failed: {out}");
    assert_eq!(out.trim(), "", "git status must be clean, got: {out:?}");

    // dedupe 不回归：模板一致内容零 upper 物化、零 fsync；upper 只剩 .git。
    // （B100100 起 .git 的 create 当场物化空 upper 占位 + staged 终裁物化
    // 内容 = 2 次记录，fsync 仍恰好 1 次。）
    let overlay = registry.get("wt-1").expect("overlay registered");
    let stats = overlay.io_stats();
    assert_eq!(
        stats.upper_writes(),
        2,
        "只有 .git 指针落 upper（create 占位 + 内容终裁；含空文件在内模板内容零物化）；fsyncs={} upper={}",
        stats.fsyncs(),
        stats.upper_writes()
    );
    assert_eq!(stats.fsyncs(), 1, "物化恰好一次 fsync（.git 指针）");
    let upper = store_root.join("worktrees").join("wt-1").join("upper");
    let (upper_files, _) = count_files(&upper);
    assert_eq!(upper_files, 1, "upper 只剩 .git 指针，模板内容（含空文件）零残留");

    // 编辑空文件 → copy-up 可见（worker 流不受影响）。
    std::fs::write(worktree.join("empty.txt"), b"now nonempty\n").unwrap();
    let (ok, out) = git(&worktree, &["status", "--porcelain"]);
    assert!(ok, "git status after edit failed: {out}");
    assert!(out.contains("M empty.txt"), "expected ` M empty.txt`, got: {out:?}");
    assert_eq!(
        std::fs::read(worktree.join("empty.txt")).unwrap(),
        b"now nonempty\n"
    );

    volume.unmount();
}

#[test]
#[ignore = "requires real WinFSP runtime"]
fn real_mount_unmount_restores_empty_mount_dir_for_tower() {
    // B070100 回归：卸载后 Tower 契约目录 `<repo>/.tower/worktrees` 必须
    // 恢复为真实空目录——WinFSP 的 junction 删除（FILE_DELETE_ON_CLOSE）
    // 异步生效，补建必须先等删除竞态窗口过去（restore 内部等待 + 重试），
    // 失败升格为 error 日志，不得静默落空。
    let base = tempfile::tempdir().expect("tempdir");
    let repo = base.path().join("repo");
    let mount_point = repo.join(".tower").join("worktrees");
    std::fs::create_dir_all(&mount_point).unwrap();

    let cfg = VfsConfig::new(&repo, base.path().join("store"));
    let registry = Arc::new(VfsRegistry::open(cfg).unwrap());
    let volume = mount_directory(&mount_point, registry)
        .expect("WinFSP directory mount must succeed (is the WinFSP runtime installed?)");
    assert!(is_junction(&mount_point), "mounted point must be a junction");

    volume.unmount();

    // unmount 返回即恢复完成：junction 消失 + 真实空目录回到 Tower 契约位。
    assert!(
        !is_junction(&mount_point),
        "junction must be gone after unmount"
    );
    assert!(
        mount_point.is_dir(),
        "empty mount dir must be restored for Tower"
    );
    assert!(
        std::fs::read_dir(&mount_point).unwrap().next().is_none(),
        "restored mount dir must be empty"
    );
}
