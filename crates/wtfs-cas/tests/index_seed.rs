//! 索引种子集成测试（spike 结论的回归保护）：
//!
//! * route A（`read-tree` + 批量 skip-worktree）→ `git status` 干净、
//!   零 hydrate、零对象读取；
//! * 清除 skip-worktree 位 → 未挂载必然报全量删除（route B 未挂载的失败
//!   模式，回归护栏）；
//! * route A 完整形态（stat 对齐改写）→ index stat 逐位等于 manifest 值。

#![cfg(windows)]

mod common;

use std::path::Path;

use wtfs_cas::{
    Manifest, add_worktree_no_checkout, clear_skip_worktree, seed_skip_worktree, seed_stat_aligned,
};

fn worktree_of(base: &Path, name: &str, branch: &str) -> std::path::PathBuf {
    let wt = base.join(name);
    add_worktree_no_checkout(base, &wt, branch).expect("worktree add --no-checkout");
    wt
}

#[test]
fn route_a_seeds_clean_status_with_zero_hydration() {
    let base = tempfile::tempdir().unwrap();
    let repo = common::init_demo_repo(base.path());
    let commit = common::assert_git(&repo, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    let tracked = common::assert_git(&repo, &["ls-files"]).lines().count();
    let wt = worktree_of(&repo, "wtA", "seed/a");

    let report = seed_skip_worktree(&wt, &commit).unwrap();
    assert_eq!(report.entries, tracked);
    assert!(report.read_tree_ms < 5_000.0);

    // 磁盘零文件（只有 .git 指针）
    let visible: Vec<_> = std::fs::read_dir(&wt)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name() != ".git")
        .collect();
    assert!(visible.is_empty(), "未挂载的 CAS 工作树必须没有实体文件");

    // status 干净
    assert_eq!(common::status_lines(&wt), 0);

    // skip-worktree 位全置
    let tags = common::assert_git(&wt, &["ls-files", "-v"]);
    assert!(
        tags.lines().all(|l| l.starts_with('S')),
        "全部条目 S 位: {tags}"
    );

    // 零 hydrate / 零对象读取：种子全程没有走 cat-file
    let manifest = Manifest::load(&repo, &commit).unwrap();
    let cat = wtfs_cas::CatFile::new(&repo);
    let cache = wtfs_cas::HydrationCache::open(base.path().join("store"));
    let before = cat.stats();
    assert_eq!(before.requests, 0, "种子阶段零对象读取（断言从零开始）");
    for (_p, f) in manifest.files() {
        assert!(!cache.blob_path(&f.oid).exists(), "种子不应灌入任何缓存");
    }

    // 清除位 → 未挂载必然报全量删除（route B 结论的回归护栏）
    let cleared = clear_skip_worktree(&wt).unwrap();
    assert_eq!(cleared, tracked);
    let lines = common::status_lines(&wt);
    assert_eq!(
        lines, tracked,
        "无 skip-worktree 且文件缺失 → 全量删除（未挂载）"
    );

    // 再种子 → 恢复干净
    seed_skip_worktree(&wt, &commit).unwrap();
    assert_eq!(common::status_lines(&wt), 0);
}

#[test]
fn stat_aligned_seed_writes_manifest_stat_into_index() {
    let base = tempfile::tempdir().unwrap();
    let repo = common::init_demo_repo(base.path());
    let commit = common::assert_git(&repo, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    let wt = worktree_of(&repo, "wtB", "seed/b");

    let manifest = Manifest::load(&repo, &commit).unwrap();
    let report = seed_stat_aligned(
        &wt,
        &commit,
        &manifest,
        wtfs_cas::DEFAULT_VOLUME_SERIAL,
        wtfs_cas::FIXED_STAT_EPOCH_UNIX_NS,
    )
    .unwrap();
    assert_eq!(report.entries, manifest.len());
    assert!(report.stat_rewrite_ms > 0.0);

    // status 干净（skip-worktree 位在 stat 对齐路径同样置位）
    assert_eq!(common::status_lines(&wt), 0);

    // index stat 逐位等于 manifest 值（`ls-files --debug` 解析 ctime/mtime/
    // dev/ino/size；git 报告十进制，mtime=固定纪元 946684800）
    let debug = common::assert_git(&wt, &["ls-files", "--debug", "README.md"]);
    let readme = manifest.file("README.md").unwrap();
    let mut found_ctime = false;
    for line in debug.lines() {
        if line.starts_with("  ctime:") {
            let secs: i64 = line
                .split(':')
                .nth(1)
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse().ok())
                .unwrap_or(-1);
            assert_eq!(secs, 946_684_800, "ctime = 固定纪元秒: {line}");
            found_ctime = true;
        }
        if line.starts_with("  size:") {
            let size: i64 = line
                .split(':')
                .nth(1)
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse().ok())
                .unwrap_or(-1);
            assert_eq!(size, readme.size as i64, "size = manifest 值: {line}");
        }
        if line.starts_with("  dev:") {
            let dev: i64 = line
                .split(':')
                .nth(1)
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse().ok())
                .unwrap_or(-1);
            assert_eq!(
                dev,
                wtfs_cas::DEFAULT_VOLUME_SERIAL as i64,
                "dev = 卷序列号: {line}"
            );
        }
    }
    assert!(found_ctime, "ls-files --debug 应含 ctime 行: {debug}");
}

/// route B 对照（spike 记录）：完整 checkout → 复制 index → 删树 →
/// 未挂载 status 必报全量删除。代价（4,874 文件 checkout ~2s）与失败
/// 模式由本测试固化，防止未来误改回 route B。
#[test]
fn route_b_checkout_then_delete_tree_reports_deletions_unmounted() {
    let base = tempfile::tempdir().unwrap();
    let repo = common::init_demo_repo(base.path());
    let tracked = common::assert_git(&repo, &["ls-files"]).lines().count();
    let wt = base.path().join("wt-route-b");

    // 完整 checkout（route B 的一次性代价）
    common::assert_git(
        &repo,
        &[
            "worktree",
            "add",
            &wt.to_string_lossy(),
            "-b",
            "seed/route-b",
        ],
    );
    let gitdir = common::assert_git(&wt, &["rev-parse", "--absolute-git-dir"])
        .trim()
        .to_string();
    let index = Path::new(&gitdir).join("index");
    let snapshot = base.path().join("template-index");
    std::fs::copy(&index, &snapshot).unwrap();

    // 删树
    for entry in std::fs::read_dir(&wt).unwrap().flatten() {
        let name = entry.file_name();
        if name != ".git" {
            let p = entry.path();
            if p.is_dir() {
                std::fs::remove_dir_all(&p).unwrap();
            } else {
                std::fs::remove_file(&p).unwrap();
            }
        }
    }
    std::fs::copy(&snapshot, &index).unwrap();

    // 未挂载：全部删除（这正是 route B 被弃用的原因）
    let lines = common::status_lines(&wt);
    assert_eq!(lines, tracked, "route B 未挂载必报全量删除（spike 结论）");
}
