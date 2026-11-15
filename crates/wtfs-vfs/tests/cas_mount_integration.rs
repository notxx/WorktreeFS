//! 真实 WinFSP 挂载 + CAS 视图端到端（M18/B180100 验收路径）。默认
//! `#[ignore]`：需要本机安装 WinFSP 运行时与 git CLI；
//! `cargo test -p wtfs-vfs --test cas_mount_integration -- --ignored` 显式运行。
//!
//! 显式流程（Tower 的 plain `git worktree add` 拦截不在本 mission）：
//! `git worktree add --no-checkout`（挂载点之外）→ `read-tree` + stat 对齐
//! 种子 → 挂载视图注册 → FS 在服务后批量清 skip-worktree → git 走视图。

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use wtfs_cas::{
    CatFile, HydrationCache, Manifest, SeedReport,
    add_worktree_no_checkout, clear_skip_worktree, seed_stat_aligned, write_stat_config,
};
use wtfs_core::FIXED_STAT_EPOCH_UNIX_NS;
use wtfs_vfs::{CasShared, VfsConfig, VfsRegistry, mount_directory, register_cas_worktree};

fn git(repo: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("git")
        .arg("-c")
        .arg("user.name=wtfs-cas-mount-test")
        .arg("-c")
        .arg("user.email=wtfs-cas-mount-test@example.com")
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

fn upper_file_count(dir: &Path, name: &str) -> usize {
    let mut count = 0;
    let root = dir.join(name);
    let mut stack = vec![root];
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

/// 显式流程建 CAS worktree + 注册视图：返回（种子报告，真实 worktree 目录）。
fn add_and_register_cas_worktree(
    registry: &Arc<VfsRegistry>,
    repo: &Path,
    store_root: &Path,
    real_wt: &Path,
    name: &str,
    branch: &str,
) -> (SeedReport, PathBuf) {
    add_worktree_no_checkout(repo, real_wt, branch).expect("worktree add --no-checkout");
    let head = assert_git(repo, &["rev-parse", "HEAD"]).trim().to_string();
    let manifest = Arc::new(Manifest::load(repo, &head).expect("manifest load"));
    let report = seed_stat_aligned(
        real_wt,
        &head,
        &manifest,
        registry.config().volume_serial,
        FIXED_STAT_EPOCH_UNIX_NS,
    )
    .expect("seed_stat_aligned");
    // stat 对齐配置：checkStat=minimal 等（清 skip-worktree 位之前）。
    write_stat_config(real_wt).expect("write stat config");
    let shared = Arc::new(CasShared {
        cat: Arc::new(CatFile::new(repo)),
        cache: Arc::new(HydrationCache::open(store_root)),
    });
    register_cas_worktree(registry, name, real_wt, manifest, shared).expect("register cas view");
    (report, real_wt.to_path_buf())
}

#[test]
#[ignore = "requires real WinFSP runtime and git CLI"]
fn cas_mount_add_status_diff_commit_remove_with_hydration_accounting() {
    let base = tempfile::tempdir().expect("tempdir");
    let base = base.path().to_path_buf();
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("README.md"), "# hello\n").unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn f() -> u32 { 1 }\n").unwrap();
    std::fs::write(repo.join("src/main.rs"), "fn main() { println!(\"hi\"); }\n").unwrap();
    assert_git(&repo, &["init", "-b", "master"]);
    assert_git(&repo, &["config", "core.autocrlf", "false"]);
    assert_git(&repo, &["add", "."]);
    assert_git(&repo, &["commit", "-m", "init", "-q"]);

    let store_root = base.join("store");
    let mount_point = repo.join(".tower").join("worktrees");
    std::fs::create_dir_all(&mount_point).unwrap();
    let cfg = VfsConfig::new(&repo, &store_root);
    let registry = Arc::new(VfsRegistry::open(cfg).unwrap());
    let volume = mount_directory(&mount_point, registry.clone())
        .expect("WinFSP directory mount must succeed (is the WinFSP runtime installed?)");

    // 1. add：--no-checkout + 种子 + 注册（真实目录在挂载点之外）。
    let real_wt = base.join("cas-wt").join("wt-cas-1");
    let (report, _real_wt) = add_and_register_cas_worktree(
        &registry,
        &repo,
        &store_root,
        &real_wt,
        "wt-cas-1",
        "cas-1",
    );
    assert_eq!(report.entries, 3, "种子覆盖全部 index entry");

    let view_dir = mount_point.join("wt-cas-1");
    let backend = registry
        .get_backend("wt-cas-1")
        .expect("cas view registered");
    let cas = backend.as_cas().expect("cas backend");

    // 2. 种子阶段零 hydrate；挂载视图可枚举（.git 在 upper，其余在 manifest）。
    assert_eq!(cas.cache_stats().misses, 0, "种子/注册零对象读取");
    let entries: Vec<String> = std::fs::read_dir(&view_dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(entries.contains(&".git".to_string()), "upper 的 .git 可见: {entries:?}");
    assert!(entries.contains(&"README.md".to_string()));
    assert!(entries.contains(&"src".to_string()));

    // 3. 挂载态切换：FS 已在服务后才清 skip-worktree（未就绪不得清位）。
    let cleared = clear_skip_worktree(&real_wt).expect("clear skip-worktree");
    assert_eq!(cleared, 3);

    // 4. status 干净 + 0 hydration（stat 不 hydrate——C 方案命门）。
    let out = assert_git(&view_dir, &["status", "--porcelain"]);
    assert_eq!(out.trim(), "", "status must be clean, got: {out:?}");
    assert_eq!(cas.cache_stats().misses, 0, "status 零 hydrate");
    assert_eq!(cas.cache_stats().hits, 0, "status 零文件读取");
    let out = assert_git(&view_dir, &["diff", "--stat"]);
    assert_eq!(out.trim(), "", "clean tree diff must be empty");
    assert_eq!(cas.cache_stats().misses, 0, "diff 零 hydrate");

    // 5. 首次读触发 hydration；二次读缓存命中。
    let content = std::fs::read(view_dir.join("src/lib.rs")).expect("read through mount");
    assert_eq!(content, b"pub fn f() -> u32 { 1 }\n");
    assert_eq!(cas.cache_stats().misses, 1, "首读恰好一次 hydrate");
    assert_eq!(cas.cache_stats().hydrated_files, 1);
    let content = std::fs::read(view_dir.join("src/lib.rs")).expect("second read");
    assert_eq!(content, b"pub fn f() -> u32 { 1 }\n");
    assert!(cas.cache_stats().hits >= 1, "二读缓存命中");
    assert_eq!(cas.cache_stats().misses, 1);

    // 6. 写路径：编辑 → status 报修改 → commit → 刷新 → 干净。
    std::fs::write(view_dir.join("README.md"), "# changed\n").unwrap();
    let out = assert_git(&view_dir, &["status", "--porcelain"]);
    assert!(out.contains("M README.md"), "expected modification, got: {out:?}");
    assert_git(&view_dir, &["commit", "-am", "edit readme", "-q"]);
    // commit 后 HEAD 前进：manifest 刷新前 status 仍应干净（stat 对齐），
    // 刷新后新 HEAD 的 manifest oid 与 upper 内容一致。
    let out = assert_git(&view_dir, &["status", "--porcelain"]);
    assert_eq!(out.trim(), "", "commit 后 status 必须干净: {out:?}");
    assert!(cas.refresh_head().unwrap(), "HEAD 前进需重载 manifest");
    let out = assert_git(&view_dir, &["status", "--porcelain"]);
    assert_eq!(out.trim(), "", "刷新后 status 仍干净");

    // 7. delete-on-close → M12 whiteout；git checkout 回写一致内容 → 回收
    // （upper 无 README.md 残留，stat 回落 manifest）。
    std::fs::remove_file(view_dir.join("README.md")).expect("delete through mount");
    let out = assert_git(&view_dir, &["status", "--porcelain"]);
    assert!(out.contains("D README.md"), "expected deletion, got: {out:?}");
    assert_git(&view_dir, &["checkout", "--", "README.md"]);
    let out = assert_git(&view_dir, &["status", "--porcelain"]);
    assert_eq!(out.trim(), "", "checkout 回写后 status 干净: {out:?}");
    assert!(
        !real_wt.join("README.md").exists(),
        "一致内容经 write_from 回收，零 upper 残留"
    );

    // 8. 新建文件（CREATE_NEW 零字节 + 写入）→ git add + commit。
    std::fs::write(view_dir.join("new.txt"), b"brand new\n").unwrap();
    let out = assert_git(&view_dir, &["status", "--porcelain"]);
    assert!(out.contains("?? new.txt"), "expected untracked, got: {out:?}");
    assert_git(&view_dir, &["add", "new.txt"]);
    assert_git(&view_dir, &["commit", "-m", "add new", "-q"]);
    assert!(cas.refresh_head().unwrap());
    let out = assert_git(&view_dir, &["status", "--porcelain"]);
    assert_eq!(out.trim(), "");

    // 9. upper 只剩必要文件（.git 指针 + 提交过的修改/新文件）。
    let upper_files = upper_file_count(&real_wt, "");
    assert!(
        upper_files <= 3,
        "upper 必须接近零残留（.git + new.txt + 已提交内容），got {upper_files}"
    );

    // 10. remove：git worktree remove + 注册表注销。C 模型边界：回收后
    // upper 是稀疏的（回收的文件物理不在真实目录），git 对真实路径的
    // 干净检查必报“已修改/已删除”→ 需要 --force（shim/daemon 拦截
    // “先注销视图再 remove”不在本 mission）。
    let (ok, out) = git(
        &repo,
        &["worktree", "remove", "--force", &_real_wt.to_string_lossy()],
    );
    assert!(ok, "worktree remove: {out}");
    assert!(registry.unregister("wt-cas-1"), "路由注销");
    assert!(registry.get_backend("wt-cas-1").is_none());
    let names: Vec<String> = std::fs::read_dir(&mount_point)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !names.contains(&"wt-cas-1".to_string()),
        "挂载根不再枚举已移除的 worktree: {names:?}"
    );

    volume.unmount();
}

#[test]
#[ignore = "requires real WinFSP runtime and git CLI"]
fn cas_mount_seed_without_mount_status_reports_all_deleted() {
    // M17 结论的挂载侧回归锚点：种子的 skip-worktree 是未挂载态干净的唯一
    // 途径；清位后未挂载（FS 未服务）status 必报全量删除——docs 写明
    // “先 mount 再清位”的依据。
    let base = tempfile::tempdir().expect("tempdir");
    let base = base.path().to_path_buf();
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("README.md"), "# hello\n").unwrap();
    assert_git(&repo, &["init", "-b", "master"]);
    assert_git(&repo, &["config", "core.autocrlf", "false"]);
    assert_git(&repo, &["add", "."]);
    assert_git(&repo, &["commit", "-m", "init", "-q"]);

    let real_wt = base.join("wt");
    add_worktree_no_checkout(&repo, &real_wt, "no-mount").unwrap();
    let head = assert_git(&repo, &["rev-parse", "HEAD"]).trim().to_string();
    let manifest = Arc::new(Manifest::load(&repo, &head).unwrap());
    seed_stat_aligned(&real_wt, &head, &manifest, 0x5754_4653, FIXED_STAT_EPOCH_UNIX_NS)
        .expect("seed");
    let out = assert_git(&real_wt, &["status", "--porcelain"]);
    assert_eq!(out.trim(), "", "种子后未挂载 status 干净（skip-worktree）");

    clear_skip_worktree(&real_wt).unwrap();
    let out = assert_git(&real_wt, &["status", "--porcelain"]);
    assert!(
        out.contains("D README.md"),
        "清位后未挂载必报全量删除（先 mount 再清位的依据）: {out:?}"
    );
}
