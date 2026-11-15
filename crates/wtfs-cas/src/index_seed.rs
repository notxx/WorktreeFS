//! 索引种子（spike 定稿的 route A）：不 checkout 任何文件，让 `git status`
//! 在 CAS 视图下干净且零 hydrate。
//!
//! M17 spike 结论（demo repo 与 4,874 文件夹具实测，报告见 docs/m17-cas-hydration.md）：
//!
//! * **route A（本模块，选定）**：`git worktree add --no-checkout` →
//!   `git read-tree <commit>` → `git update-index -z --skip-worktree --stdin`
//!   批量置位 → `git status` 干净、磁盘零文件、零对象读取。代价 = 3 个 git
//!   子进程 + 一次 index 重写，与文件内容解耦。
//! * **route B（对照，弃用）**：完整 checkout 一次 → 复制 `<gitdir>/index` →
//!   删树。未挂载时 status 必报全量 ` D` 删除（index stat 对齐真实文件，
//!   文件没了 git 只能信）；叠加 skip-worktree 才干净——等于 route A 加一次
//!   全量 checkout 的无谓代价。其 index stat 还是 checkout 时刻的真实 mtime，
//!   与 CAS 视图固定纪元不对齐，挂载态仍需改写。
//!
//! `skip-worktree` 位是未挂载态的唯一干净路径：git 对该位条目跳过工作树
//! stat（sparse-checkout 的同款机制）。**挂载态切换**（M18）：FS 报告
//! manifest stat 后，[`seed_stat_aligned`] 已把 index 的 stat 字段改写为
//! manifest 值（dev/ino/size/mode/mtime=固定纪元），批量清除 skip-worktree
//! 位（[`clear_skip_worktree`]）即完成挂载态对齐——届时 status 的干净性由
//! "FS stat == index stat" 保证，与 B1 模板 index 的机制同构。
//!
//! 注意：skip-worktree 是 git 尊重的语义位（sparse-checkout 用），但全树
//! 置位属于超范围使用——`git add`/`git stash` 等对 skip-worktree 路径的
//! 行为按 sparse 规则跳过，M18 挂载后必须清除；代价与语义边界写入报告。

use std::path::Path;
use std::time::Instant;

use crate::gitx::{absolute_git_dir, require_ok, run_git, run_git_with_stdin};
use crate::manifest::Manifest;
use crate::{Error, Result};

/// 索引种子的耗时分解（毫秒；基准/报告用）。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SeedReport {
    /// read-tree 覆盖的 index entry 数（= `ls-files` 条目数）。
    pub entries: usize,
    pub read_tree_ms: f64,
    /// 批量 skip-worktree 置位耗时（route A）；stat 对齐路径内联执行为 0。
    pub skip_worktree_ms: f64,
    /// gix-index stat 改写耗时（仅 [`seed_stat_aligned`]）。
    pub stat_rewrite_ms: f64,
    pub total_ms: f64,
}

/// 为 linked worktree 建 CAS 工作树目录（`git worktree add --no-checkout`）：
/// 目录存在但工作树为空（只有 `.git` 指针文件），index 未创建。
pub fn add_worktree_no_checkout(base_repo: &Path, target: &Path, branch: &str) -> Result<()> {
    let out = run_git(
        base_repo,
        &[
            "worktree",
            "add",
            "--no-checkout",
            &target.to_string_lossy(),
            "-b",
            branch,
        ],
    )?;
    require_ok(&out, "git worktree add --no-checkout")
}

/// route A 种子：read-tree + 批量 skip-worktree。返回耗时分解。
pub fn seed_skip_worktree(worktree: &Path, commit: &str) -> Result<SeedReport> {
    let start = Instant::now();
    let out = run_git(worktree, &["read-tree", commit])?;
    require_ok(&out, "git read-tree")?;
    let read_tree_ms = start.elapsed().as_secs_f64() * 1000.0;

    let skip_start = Instant::now();
    let entries = set_skip_worktree(worktree, true)?;
    let skip_worktree_ms = skip_start.elapsed().as_secs_f64() * 1000.0;

    Ok(SeedReport {
        entries,
        read_tree_ms,
        skip_worktree_ms,
        stat_rewrite_ms: 0.0,
        total_ms: start.elapsed().as_secs_f64() * 1000.0,
    })
}

/// route A 完整形态：read-tree 后用 gix-index 把每条 entry 的 stat 字段
/// 改写为 manifest 值（dev/ino/size/mode 不变由 mode 决定、mtime=ctime=
/// 固定纪元），并置 skip-worktree 位（未挂载态干净）。挂载态（M18）只需
/// [`clear_skip_worktree`]——index stat 已与 FS 报告逐位对齐。
///
/// manifest 未覆盖的 entry（不应发生：read-tree 与 manifest 同 commit）
/// 保留 zeroed stat、只置位。
pub fn seed_stat_aligned(
    worktree: &Path,
    commit: &str,
    manifest: &Manifest,
    volume_serial: u32,
    epoch_ns: i64,
) -> Result<SeedReport> {
    use gix_index::entry::stat::Time;
    use gix_index::entry::{Flags, Stat};
    use gix_index::hash::Kind;

    let start = Instant::now();
    let out = run_git(worktree, &["read-tree", commit])?;
    require_ok(&out, "git read-tree")?;
    let read_tree_ms = start.elapsed().as_secs_f64() * 1000.0;

    let rewrite_start = Instant::now();
    let index_path = absolute_git_dir(worktree)?.join("index");
    let mut index = gix_index::File::at(&index_path, Kind::Sha1, false, Default::default())
        .map_err(|e| Error::Cas(format!("open index for rewrite failed: {e}")))?;
    let mut entries = 0usize;
    {
        for (entry, path) in index.entries_mut_with_paths() {
            let path = std::str::from_utf8(path)
                .map_err(|_| Error::Cas("index entry path is not UTF-8".into()))?;
            // SKIP_WORKTREE 是高位标志（1<<30），落盘拆进扩展标志字——必须
            // 同时置 EXTENDED（1<<14）；writer 检测到 EXTENDED 自动升 v3
            // （gix-index write.rs / entry/write.rs 的存储约定）。
            entry.flags.insert(Flags::EXTENDED);
            entry.flags.insert(Flags::SKIP_WORKTREE);
            if let Some(f) = manifest.file(path) {
                let secs = (epoch_ns / 1_000_000_000).max(0) as u32;
                let nsecs = (epoch_ns % 1_000_000_000).max(0) as u32;
                entry.stat = Stat {
                    dev: volume_serial,
                    ino: crate::file_id(path) as u32,
                    uid: 0,
                    gid: 0,
                    size: u32::try_from(f.size).unwrap_or(u32::MAX),
                    mtime: Time { secs, nsecs },
                    ctime: Time { secs, nsecs },
                };
            }
            entries += 1;
        }
    }
    // Extensions::default() = 全保留（read-tree 的 cache-tree 与条目 oid
    // 未变，仍有效）；版本维持读入值（git 2.x 默认 v2）。
    index
        .write(gix_index::write::Options::default())
        .map_err(|e| Error::Cas(format!("write rewritten index failed: {e}")))?;
    let stat_rewrite_ms = rewrite_start.elapsed().as_secs_f64() * 1000.0;

    Ok(SeedReport {
        entries,
        read_tree_ms,
        skip_worktree_ms: 0.0,
        stat_rewrite_ms,
        total_ms: start.elapsed().as_secs_f64() * 1000.0,
    })
}

/// 批量清除 skip-worktree 位（M18 挂载态切换；也用于测试里对照验证
/// "清除后未挂载必然报全量删除"）。
pub fn clear_skip_worktree(worktree: &Path) -> Result<usize> {
    set_skip_worktree(worktree, false)
}

fn set_skip_worktree(worktree: &Path, set: bool) -> Result<usize> {
    let flag = if set {
        "--skip-worktree"
    } else {
        "--no-skip-worktree"
    };
    let ls = run_git(worktree, &["ls-files", "-z"])?;
    require_ok(&ls, "git ls-files")?;
    let args = vec!["update-index", "-z", flag, "--stdin"];
    let out = run_git_with_stdin(worktree, &args, &ls.stdout)?;
    require_ok(&out, "git update-index --stdin")?;
    Ok(ls
        .stdout
        .split(|&b| b == 0u8)
        .filter(|p| !p.is_empty())
        .count())
}
