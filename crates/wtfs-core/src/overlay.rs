//! 单个 worktree 的 overlay 视图引擎（spec §6.5/§7：[`OverlayStore`] 之上的语义层）。
//!
//! * 解析顺序：upper > whiteout > lower；目录 = lower ∪ upper 再剔除 whiteout。
//! * 目录级重命名走 rename map（lower 侧路径改写），文件级 = whiteout + upper 搬移。
//! * 写入路径 dedupe：流式写临时文件并同步 blake3，与模板一致则丢弃副本回落
//!   lower（[`crate::DedupeState::Lower`]），不同则落 upper；模板缺失时全部
//!   upper（全量 copy 兜底），绝不丢数据。
//! * stat：lower 文件固定 dev/ino/mtime/ctime（size/mode 来自模板），upper 文件
//!   用单调递增的逻辑 mtime（`stat_cache` 持久化，跨重启不倒退），目录按
//!   `0o040755` 报告。VFS（M3）经 [`Overlay::stat`]/[`Overlay::attributes_for`]
//!   拿到 WinFSP 需要的稳定值。
//!
//! 元数据全部经 [`SqliteOverlayStore`] 落 SQLite；本引擎自身无内存可变状态，
//! `&self` 并发安全。约定：upper 根下以 `.wtfs-tmp` 开头的名字是写入临时文件，
//! 对视图不可见；写入请求若使用该前缀名会被拒绝。

use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::overlay_store::{SqliteOverlayStore, StatCacheEntry};
use crate::{
    DedupeState, Error, FileAttributes, FileHash, FileHashMap, OverlayConfig, OverlayStore,
    Result, Stat, TemplateFile, WorktreeId, validate_relative_path,
};

/// upper 内写入临时文件的名字前缀（对视图隐藏，崩溃残留由 gc 清理）。
pub const TEMP_PREFIX: &str = ".wtfs-tmp";

/// overlay 根目录的稳定 ino（`""` 不入 `file_ids` 表）。
pub const ROOT_FILE_ID: u64 = 1;

/// 解析结果（spec §6.5）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    NotFound,
    /// 被 whiteout 删除且 upper/lower 均无（VFS 语义 = 不存在）。
    Whiteout,
    /// upper 命中；`path` 为 upper 物理路径。
    Upper { path: PathBuf },
    /// lower 命中；`path` 为模板树内物理路径。
    Lower { file: TemplateFile, path: PathBuf },
    /// 目录：lower/upper 两侧存在性（合并枚举时用）。
    Directory { has_lower: bool, has_upper: bool },
}

/// 目录条目类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
}

/// [`Overlay::list`] 的条目；`rel` 是相对 overlay 根的完整路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayDirEntry {
    pub name: String,
    pub rel: String,
    pub kind: EntryKind,
}

/// lower 侧查找时经过 rename map 改写的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LowerLookup {
    /// 无 rename 命中，lower 路径即原路径。
    Same(String),
    /// 命中 rename：lower 侧应查改写后的路径。
    Mapped(String),
    /// 位于某 rename 的 from 子树下（已搬走），lower 侧不可见。
    MovedAway,
}

/// rename map 的纯改写（单层映射；链式 rename 不支持）。
pub(crate) fn map_to_lower_with(renames: &[(String, String)], rel: &str) -> LowerLookup {
    for (from, to) in renames {
        if rel == to {
            return LowerLookup::Mapped(from.clone());
        }
        if let Some(suffix) = rel.strip_prefix(&format!("{to}/")) {
            return LowerLookup::Mapped(format!("{from}/{suffix}"));
        }
    }
    for (from, _) in renames {
        if rel == from || rel.starts_with(&format!("{from}/")) {
            return LowerLookup::MovedAway;
        }
    }
    LowerLookup::Same(rel.to_string())
}

/// `mapped` 是否被 whiteout 挡住（自身或任一祖先目录命中）。
pub(crate) fn whiteout_blocks(mapped: &str, whites: &HashSet<String>) -> bool {
    if mapped.is_empty() {
        return false;
    }
    if whites.contains(mapped) {
        return true;
    }
    ancestor_whiteout_blocks(mapped, whites)
}

/// 仅祖先目录命中（不含路径自身）。路径自身的 whiteout 可以被
/// `remove_whiteout` 精确撤销，回落 lower 后仍然可见；祖先目录的 whiteout
/// 则会继续隐藏子树，回落会导致"写入成功但文件不可见"。
pub(crate) fn ancestor_whiteout_blocks(mapped: &str, whites: &HashSet<String>) -> bool {
    let mut cur = mapped;
    while let Some(idx) = cur.rfind('/') {
        cur = &cur[..idx];
        if whites.contains(cur) {
            return true;
        }
    }
    false
}

fn basename(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

fn is_temp_name(name: &str) -> bool {
    name.starts_with(TEMP_PREFIX)
}

fn join_rel(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn now_unix_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i64)
}

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// overlay 视图：config + lower 模板清单 + 元数据存储。
pub struct Overlay {
    config: OverlayConfig,
    lower: Option<Arc<FileHashMap>>,
    store: Arc<SqliteOverlayStore>,
}

impl Overlay {
    /// 新建并注册（写 `worktrees` 行，状态 initializing）；同 id 已注册则报错
    /// （恢复既有注册用 [`Overlay::reopen`]）。
    pub fn new(
        config: OverlayConfig,
        lower_files: Option<FileHashMap>,
        store: Arc<SqliteOverlayStore>,
    ) -> Result<Self> {
        store.register(&config)?;
        Ok(Self {
            config,
            lower: lower_files.map(Arc::new),
            store,
        })
    }

    /// 重新挂到已注册的 worktree（崩溃恢复/重开视图）；未注册报 [`Error::NotFound`]。
    pub fn reopen(
        config: OverlayConfig,
        lower_files: Option<FileHashMap>,
        store: Arc<SqliteOverlayStore>,
    ) -> Result<Self> {
        if store.state(config.worktree_id)?.is_none() {
            return Err(Error::NotFound(format!(
                "worktree {} not registered",
                config.worktree_id
            )));
        }
        Ok(Self {
            config,
            lower: lower_files.map(Arc::new),
            store,
        })
    }

    pub fn config(&self) -> &OverlayConfig {
        &self.config
    }

    pub fn store(&self) -> &Arc<SqliteOverlayStore> {
        &self.store
    }

    /// lower 模板清单快照；`None` = 无模板（全量 copy 模式）。
    pub fn lower(&self) -> Option<&FileHashMap> {
        self.lower.as_deref()
    }

    pub fn worktree_id(&self) -> WorktreeId {
        self.config.worktree_id
    }

    // ---- 状态机 ----

    pub fn is_initializing(&self) -> Result<bool> {
        Ok(self
            .store
            .state(self.config.worktree_id)?
            .map(|s| s.hides_entries())
            .unwrap_or(true))
    }

    pub fn mark_ready(&self) -> Result<()> {
        self.store
            .set_state(self.config.worktree_id, crate::WorktreeState::Ready)
    }

    pub fn mark_failed(&self) -> Result<()> {
        self.store
            .set_state(self.config.worktree_id, crate::WorktreeState::Failed)
    }

    // ---- 路径与映射 ----

    fn validate_rel(&self, rel: &str) -> Result<()> {
        if rel.is_empty() {
            return Err(Error::InvalidPath {
                path: String::new(),
                reason: "empty".into(),
            });
        }
        validate_relative_path(rel)
    }

    /// 解析/stat/list 接受 `""`（overlay 根）。
    fn validate_rel_or_root(&self, rel: &str) -> Result<()> {
        if rel.is_empty() {
            return Ok(());
        }
        validate_relative_path(rel)
    }

    pub(crate) fn renames(&self) -> Result<Vec<(String, String)>> {
        self.store.renames(self.config.worktree_id)
    }

    /// lower 命名空间改写；`MovedAway` 时写入场景退回原路径（upper 物理位置）。
    fn mapped_or_same(&self, rel: &str) -> Result<String> {
        match map_to_lower_with(&self.renames()?, rel) {
            LowerLookup::Same(s) | LowerLookup::Mapped(s) => Ok(s),
            LowerLookup::MovedAway => Ok(rel.to_string()),
        }
    }

    fn lookup_lower(&self, mapped: &str) -> Option<TemplateFile> {
        self.lower.as_ref().and_then(|m| m.get(mapped)).cloned()
    }

    fn has_lower_dir(&self, mapped: &str) -> bool {
        match self.lower.as_ref() {
            None => false,
            Some(m) => m.keys().any(|k| k.starts_with(&format!("{mapped}/"))),
        }
    }

    fn lower_has_file_or_dir(&self, mapped: &str) -> bool {
        self.lookup_lower(&mapped).is_some() || self.has_lower_dir(&mapped)
    }

    /// lower（模板树）物理路径。
    pub fn lower_abs(&self, mapped: &str) -> Option<PathBuf> {
        self.config
            .lower_root
            .as_ref()
            .map(|root| if mapped.is_empty() { root.clone() } else { root.join(mapped) })
    }

    /// upper 物理路径。
    pub fn upper_abs(&self, rel: &str) -> PathBuf {
        if rel.is_empty() {
            self.config.upper_root.clone()
        } else {
            self.config.upper_root.join(rel)
        }
    }

    // ---- 解析与枚举 ----

    /// 解析顺序：upper > whiteout > lower（spec §6.5）。
    pub fn resolve(&self, rel: &str) -> Result<Resolution> {
        self.validate_rel_or_root(rel)?;
        if !rel.is_empty() && is_temp_name(basename(rel)) {
            return Ok(Resolution::NotFound);
        }
        let upper_path = self.upper_abs(rel);
        if let Ok(md) = std::fs::symlink_metadata(&upper_path) {
            if !md.is_dir() {
                return Ok(Resolution::Upper { path: upper_path });
            }
        }
        if rel.is_empty() {
            return Ok(Resolution::Directory {
                has_lower: self.lower.is_some(),
                has_upper: true,
            });
        }
        let mapped = match map_to_lower_with(&self.renames()?, rel) {
            LowerLookup::Same(s) | LowerLookup::Mapped(s) => s,
            LowerLookup::MovedAway => return Ok(Resolution::NotFound),
        };
        let whites = self.store.whiteouts(self.config.worktree_id)?;
        let blocked = whiteout_blocks(&mapped, &whites);
        let lower_file = self.lookup_lower(&mapped);
        let has_lower_dir = !blocked && lower_file.is_none() && self.has_lower_dir(&mapped);
        let upper_is_dir = upper_path.is_dir();
        if blocked {
            // upper 文件已查无；upper 目录可能已重建。
            if upper_is_dir {
                return Ok(Resolution::Directory {
                    has_lower: false,
                    has_upper: true,
                });
            }
            return Ok(Resolution::Whiteout);
        }
        if let Some(file) = lower_file {
            let path = self
                .lower_abs(&mapped)
                .ok_or_else(|| Error::Overlay("lower_root missing for template file".into()))?;
            return Ok(Resolution::Lower { file, path });
        }
        if has_lower_dir || upper_is_dir {
            return Ok(Resolution::Directory {
                has_lower: has_lower_dir,
                has_upper: upper_is_dir,
            });
        }
        Ok(Resolution::NotFound)
    }

    /// 目录合并枚举；`initializing` 状态返回空（让 Git 通过"目录必须为空"检查）。
    pub fn list(&self, dir_rel: &str) -> Result<Vec<OverlayDirEntry>> {
        self.validate_rel_or_root(dir_rel)?;
        if self.is_initializing()? {
            return Ok(Vec::new());
        }
        let whites = self.store.whiteouts(self.config.worktree_id)?;
        let renames = self.renames()?;
        let mut names: BTreeMap<String, EntryKind> = BTreeMap::new();

        // lower 侧：模板清单按 dir 前缀取直接子项，逐名应用 whiteout。
        match map_to_lower_with(&renames, dir_rel) {
            LowerLookup::MovedAway => {}
            LowerLookup::Same(m) | LowerLookup::Mapped(m) => {
                if !whiteout_blocks(&m, &whites) {
                    let prefix = if m.is_empty() { String::new() } else { format!("{m}/") };
                    if let Some(map) = self.lower.as_ref() {
                        for path in map.keys() {
                            let Some(rest) = path.strip_prefix(&prefix) else {
                                continue;
                            };
                            if rest.is_empty() {
                                continue;
                            }
                            let (name, kind) = match rest.split_once('/') {
                                Some((seg, _)) => (seg, EntryKind::Directory),
                                None => (rest, EntryKind::File),
                            };
                            let mapped_name = if m.is_empty() {
                                name.to_string()
                            } else {
                                format!("{m}/{name}")
                            };
                            if whiteout_blocks(&mapped_name, &whites) {
                                continue;
                            }
                            names.insert(name.to_string(), kind);
                        }
                    }
                }
            }
        }

        // upper 侧：真实目录，覆盖/补充 lower；临时文件不可见。
        let upper_dir = self.upper_abs(dir_rel);
        match std::fs::read_dir(&upper_dir) {
            Ok(read_dir) => {
                for entry in read_dir.flatten() {
                    let os_name = entry.file_name();
                    let Some(name) = os_name.to_str() else {
                        tracing::warn!(name = ?os_name, "skip non-UTF-8 upper name");
                        continue;
                    };
                    if is_temp_name(name) {
                        continue;
                    }
                    let kind = if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        EntryKind::Directory
                    } else {
                        EntryKind::File
                    };
                    names.insert(name.to_string(), kind);
                }
            }
            Err(_) => {}
        }

        Ok(names
            .into_iter()
            .map(|(name, kind)| OverlayDirEntry {
                rel: join_rel(dir_rel, &name),
                name,
                kind,
            })
            .collect())
    }

    // ---- 读 ----

    /// 上层来源的物理路径（VFS 流式读用）；whiteout/not-found → None。
    pub fn source_path(&self, rel: &str) -> Result<Option<PathBuf>> {
        match self.resolve(rel)? {
            Resolution::Upper { path } | Resolution::Lower { path, .. } => Ok(Some(path)),
            _ => Ok(None),
        }
    }

    pub fn read(&self, rel: &str) -> Result<Option<Vec<u8>>> {
        match self.resolve(rel)? {
            Resolution::Upper { path } | Resolution::Lower { path, .. } => Ok(Some(std::fs::read(path)?)),
            Resolution::Directory { .. } => Err(Error::Overlay(format!("{rel:?} is a directory"))),
            _ => Ok(None),
        }
    }

    // ---- 写 ----

    /// 全量内容写入（M0 写路径：git checkout 逐文件写）。
    pub fn write(&self, rel: &str, data: &[u8]) -> Result<(DedupeState, u64)> {
        self.write_from(rel, &mut std::io::Cursor::new(data))
    }

    /// 流式写入：边写临时文件边哈希，与模板一致则丢弃（回落 lower），
    /// 否则原子改名落 upper。返回 (决策, 实际字节数)。
    pub fn write_from(&self, rel: &str, reader: &mut impl Read) -> Result<(DedupeState, u64)> {
        self.validate_rel(rel)?;
        if is_temp_name(basename(rel)) {
            return Err(Error::Overlay(format!(
                "path {rel:?} uses reserved temp prefix {TEMP_PREFIX:?}"
            )));
        }
        let final_path = self.upper_abs(rel);
        let parent = final_path
            .parent()
            .ok_or_else(|| Error::Overlay(format!("bad upper path for {rel:?}")))?;
        std::fs::create_dir_all(parent)?;
        let tmp = parent.join(format!(
            "{TEMP_PREFIX}-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));

        let staged = (|| -> std::io::Result<(FileHash, u64)> {
            let mut file = std::fs::File::create(&tmp)?;
            let mut hasher = blake3::Hasher::new();
            let mut buf = vec![0u8; 64 * 1024];
            let mut size = 0u64;
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                file.write_all(&buf[..n])?;
                size += n as u64;
            }
            file.sync_all()?;
            Ok((FileHash(*hasher.finalize().as_bytes()), size))
        })();
        let (hash, size) = match staged {
            Ok(v) => v,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e.into());
            }
        };

        let mapped = self.mapped_or_same(rel)?;
        let template_entry = self.lookup_lower(&mapped);
        let id = self.config.worktree_id;
        if self.config.dedupe_enabled {
            if let Some(t) = &template_entry {
                if t.hash == hash {
                    let whites = self.store.whiteouts(id)?;
                    if !ancestor_whiteout_blocks(&mapped, &whites) {
                        // 与模板一致且 lower 视图可见（路径自身的 whiteout 会被
                        // remove_whiteout 撤销）：丢弃副本回落 lower。
                        // copy-up 后改回模板内容同样回收。
                        let _ = std::fs::remove_file(&tmp);
                        if final_path.symlink_metadata().is_ok() {
                            std::fs::remove_file(&final_path)?;
                        }
                        self.store.remove_stat_entry(id, rel)?;
                        self.store.record_dedupe_state(id, rel, DedupeState::Lower)?;
                        self.store.remove_whiteout(id, &mapped)?;
                        return Ok((DedupeState::Lower, size));
                    }
                    // 祖先目录被 whiteout：回落会让刚写入的文件不可见——
                    // 保留内容继续走 Upper 路径（仅错过一次 dedupe 机会）。
                }
            }
        }
        // 落 upper（模板缺失 → 全量 copy 兜底，一样正确）。
        std::fs::rename(&tmp, &final_path)?;
        let state = if template_entry.is_some() {
            DedupeState::Upper
        } else {
            DedupeState::New
        };
        self.store.record_dedupe_state(id, rel, state)?;
        self.store.remove_whiteout(id, &mapped)?;
        let mtime = self.issue_logical_mtime()?;
        self.store.set_stat_entry(
            id,
            rel,
            StatCacheEntry { mtime_ns: mtime, size, flags: 0 },
        )?;
        Ok((state, size))
    }

    /// copy-up：lower 内容复制到 upper 以便就地修改；已在 upper 则返回其路径。
    pub fn copy_up(&self, rel: &str) -> Result<Option<PathBuf>> {
        self.validate_rel(rel)?;
        let final_path = self.upper_abs(rel);
        match self.resolve(rel)? {
            Resolution::Lower { path, .. } => {
                if let Some(parent) = final_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(&path, &final_path)?;
                let size = std::fs::metadata(&final_path)?.len();
                let mapped = self.mapped_or_same(rel)?;
                let id = self.config.worktree_id;
                self.store.remove_whiteout(id, &mapped)?;
                self.store.record_dedupe_state(id, rel, DedupeState::Upper)?;
                let mtime = self.issue_logical_mtime()?;
                self.store.set_stat_entry(
                    id,
                    rel,
                    StatCacheEntry { mtime_ns: mtime, size, flags: 0 },
                )?;
                Ok(Some(final_path))
            }
            Resolution::Upper { path } => Ok(Some(path)),
            _ => Ok(None),
        }
    }

    /// 删除：upper 删真实文件；lower 有则记 whiteout（否则被删文件会在合并视图重现）。
    pub fn remove(&self, rel: &str) -> Result<bool> {
        self.validate_rel(rel)?;
        let id = self.config.worktree_id;
        let upper_path = self.upper_abs(rel);
        match self.resolve(rel)? {
            Resolution::Upper { .. } => {
                std::fs::remove_file(&upper_path)?;
                self.store.remove_stat_entry(id, rel)?;
                self.store.remove_dedupe(id, rel)?;
                let mapped = self.mapped_or_same(rel)?;
                if self.lower_has_file_or_dir(&mapped) {
                    self.store.add_whiteout(id, &mapped)?;
                }
                Ok(true)
            }
            Resolution::Directory { has_lower, has_upper } => {
                if has_upper {
                    std::fs::remove_dir_all(&upper_path)?;
                }
                self.store.remove_meta_subtree(id, rel)?;
                let mapped = self.mapped_or_same(rel)?;
                if has_lower || self.lower_has_file_or_dir(&mapped) {
                    self.store.add_whiteout(id, &mapped)?;
                }
                Ok(true)
            }
            Resolution::Lower { .. } => {
                let mapped = self.mapped_or_same(rel)?;
                self.store.add_whiteout(id, &mapped)?;
                Ok(true)
            }
            Resolution::Whiteout | Resolution::NotFound => Ok(false),
        }
    }

    /// 重命名：文件级 = whiteout 源 + upper 搬移/copy-up；目录级 = rename map。
    /// 目标若是目录报错；目标是文件则覆盖（POSIX rename 语义）。
    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.validate_rel(from)?;
        self.validate_rel(to)?;
        if from == to {
            return Ok(());
        }
        if from.starts_with(&format!("{to}/")) || to.starts_with(&format!("{from}/")) {
            return Err(Error::Overlay(format!(
                "rename {from:?} -> {to:?}: cannot rename into own subtree"
            )));
        }
        let id = self.config.worktree_id;
        let source = self.resolve(from)?;
        if matches!(source, Resolution::NotFound | Resolution::Whiteout) {
            return Err(Error::NotFound(from.to_string()));
        }
        let target = self.resolve(to)?;
        match (&source, &target) {
            (_, Resolution::Directory { .. }) => {
                return Err(Error::Overlay(format!(
                    "rename target {to:?} is an existing directory"
                )));
            }
            (Resolution::Directory { .. }, Resolution::Upper { .. } | Resolution::Lower { .. }) => {
                return Err(Error::Overlay(format!(
                    "rename source {from:?} is a directory but target {to:?} is a file"
                )));
            }
            _ => {}
        }
        let mapped_from = self.mapped_or_same(from)?;
        let mapped_to = self.mapped_or_same(to)?;
        let is_upper_src = matches!(source, Resolution::Upper { .. });

        match source {
            Resolution::Upper { path } | Resolution::Lower { path, .. } => {
                let to_upper = self.upper_abs(to);
                if let Some(parent) = to_upper.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                if to_upper.symlink_metadata().is_ok() {
                    std::fs::remove_file(&to_upper)?;
                }
                if is_upper_src {
                    std::fs::rename(&path, &to_upper)?;
                } else {
                    std::fs::copy(&path, &to_upper)?;
                }
                if self.lower_has_file_or_dir(&mapped_from) {
                    self.store.add_whiteout(id, &mapped_from)?;
                }
                self.store.remove_whiteout(id, &mapped_to)?;
                self.store.rewrite_meta_prefix(id, from, to)?;
                self.store.record_dedupe_state(id, to, DedupeState::Upper)?;
                if !is_upper_src {
                    // lower copy-up：新 upper 路径需要逻辑 mtime。
                    let size = std::fs::metadata(&to_upper)?.len();
                    let mtime = self.issue_logical_mtime()?;
                    self.store.set_stat_entry(
                        id,
                        to,
                        StatCacheEntry { mtime_ns: mtime, size, flags: 0 },
                    )?;
                }
            }
            Resolution::Directory { has_lower, has_upper } => {
                if has_upper {
                    let to_upper = self.upper_abs(to);
                    if let Some(parent) = to_upper.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::rename(self.upper_abs(from), &to_upper)?;
                }
                if has_lower || self.lower_has_file_or_dir(&mapped_from) {
                    self.store.record_rename(id, &mapped_from, &mapped_to)?;
                }
                // upper 内容物理随迁：dedupe/stat_cache 键改写；
                // whiteout 保留在 lower 命名空间（改写后的 lower 查询仍能命中）。
                self.store.rewrite_meta_prefix(id, from, to)?;
            }
            Resolution::Whiteout | Resolution::NotFound => unreachable!("checked above"),
        }
        Ok(())
    }

    // ---- stat（暴露给 VFS 的稳定值） ----

    /// 逻辑 stat：lower 固定（dev/ino/mtime/ctime 恒定，size/mode 来自模板），
    /// upper 用逻辑 mtime；目录 `0o040755`。whiteout/not-found → None。
    pub fn stat(&self, rel: &str) -> Result<Option<Stat>> {
        self.validate_rel_or_root(rel)?;
        if !rel.is_empty() && is_temp_name(basename(rel)) {
            return Ok(None);
        }
        let vs = u64::from(self.config.volume_serial);
        let epoch = self.config.fixed_stat_epoch_ns;
        let resolution = self.resolve(rel)?;
        let out = match resolution {
            Resolution::Whiteout | Resolution::NotFound => return Ok(None),
            Resolution::Upper { path } => {
                let md = std::fs::metadata(&path)?;
                let mode = if self.lower_mode_is_exec(rel)? {
                    0o100755
                } else {
                    0o100644
                };
                let mtime = self.cached_mtime(rel, md.len(), false)?;
                Stat {
                    dev: vs,
                    ino: self.file_id(rel)?,
                    mode,
                    size: md.len(),
                    mtime_ns: mtime,
                    ctime_ns: mtime,
                }
            }
            Resolution::Lower { file, .. } => Stat {
                dev: vs,
                ino: self.file_id(rel)?,
                mode: file.mode,
                size: file.size,
                mtime_ns: epoch,
                ctime_ns: epoch,
            },
            Resolution::Directory { has_upper, .. } => {
                let ino = if rel.is_empty() { ROOT_FILE_ID } else { self.file_id(rel)? };
                let mtime = if has_upper {
                    self.cached_mtime(rel, 0, true)?
                } else {
                    epoch
                };
                Stat {
                    dev: vs,
                    ino,
                    mode: 0o040755,
                    size: 0,
                    mtime_ns: mtime,
                    ctime_ns: mtime,
                }
            }
        };
        Ok(Some(out))
    }

    /// 稳定 file id（Windows FILE_ID 与 index ino；持久映射）。
    pub fn file_id(&self, rel: &str) -> Result<u64> {
        self.store.file_id(self.config.worktree_id, rel)
    }

    fn lower_mode_is_exec(&self, rel: &str) -> Result<bool> {
        let mapped = self.mapped_or_same(rel)?;
        Ok(self
            .lower
            .as_ref()
            .and_then(|m| m.get(&mapped))
            .map(|f| f.mode & 0o111 != 0)
            .unwrap_or(false))
    }

    /// 单调递增逻辑时钟：max(系统 now, 已发放最大值 + 1)。
    fn issue_logical_mtime(&self) -> Result<i64> {
        let now = now_unix_ns();
        let last = self.store.max_stat_mtime(self.config.worktree_id)?;
        Ok(now.max(last.unwrap_or(0) + 1))
    }

    /// 读取或发放并缓存逻辑 mtime（同一 upper 路径的 stat 在两次写之间稳定）。
    fn cached_mtime(&self, rel: &str, size: u64, is_dir: bool) -> Result<i64> {
        let id = self.config.worktree_id;
        if let Some(entry) = self.store.stat_entry(id, rel)? {
            return Ok(entry.mtime_ns);
        }
        let mtime = self.issue_logical_mtime()?;
        self.store.set_stat_entry(
            id,
            rel,
            StatCacheEntry {
                mtime_ns: mtime,
                size,
                flags: if is_dir {
                    StatCacheEntry::FLAG_DIRECTORY
                } else {
                    0
                },
            },
        )?;
        Ok(mtime)
    }
}

/// [`Stat`] → Win32 文件属性位（VFS 回填 `get_file_info` 用）。
pub fn attributes_for(stat: &Stat) -> FileAttributes {
    if stat.mode & 0o170000 == 0o040000 {
        FileAttributes::DIRECTORY
    } else {
        FileAttributes::ARCHIVE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Db, OverlayStore, TemplateId, WorktreeId};
    use std::collections::HashMap;
    use tempfile::TempDir;

    const ONE: &[u8] = b"one\n";
    const TWO: &[u8] = b"two\n";
    const BEE: &[u8] = b"bee\n";
    const RUN: &[u8] = b"run\n";

    fn lower_fixture(dir: &std::path::Path) -> FileHashMap {
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::write(dir.join("a/1.txt"), ONE).unwrap();
        std::fs::write(dir.join("a/2.txt"), TWO).unwrap();
        std::fs::write(dir.join("b.txt"), BEE).unwrap();
        std::fs::write(dir.join("exec.sh"), RUN).unwrap();
        let mut map = HashMap::new();
        for (path, content, mode) in [
            ("a/1.txt", ONE, 0o100644),
            ("a/2.txt", TWO, 0o100644),
            ("b.txt", BEE, 0o100644),
            ("exec.sh", RUN, 0o100755),
        ] {
            map.insert(
                path.to_string(),
                TemplateFile {
                    path: path.to_string(),
                    hash: crate::dedupe::hash_bytes(content),
                    size: content.len() as u64,
                    mode,
                },
            );
        }
        map
    }

    struct Fixture {
        _dir: TempDir,
        lower_dir: PathBuf,
        upper_dir: PathBuf,
        store: Arc<SqliteOverlayStore>,
    }

    fn fixture(lower: bool) -> Fixture {
        let dir = TempDir::new().unwrap();
        let lower_dir = dir.path().join("lower");
        let upper_dir = dir.path().join("upper");
        std::fs::create_dir_all(&lower_dir).unwrap();
        std::fs::create_dir_all(&upper_dir).unwrap();
        let db = Db::open_in_memory().unwrap();
        let store = Arc::new(SqliteOverlayStore::new(db));
        if lower {
            lower_fixture(&lower_dir);
            seed_template(&store);
        }
        Fixture { _dir: dir, lower_dir, upper_dir, store }
    }

    /// `worktrees.template_id` 带 FK：种一行模板让注册合法。
    fn seed_template(store: &SqliteOverlayStore) {
        use crate::db::db_err;
        store
            .db()
            .with_tx(|tx| {
                tx.execute(
                    "INSERT INTO repos(id, root, common_git_dir, created_at) VALUES (1, 'r', '', 0)",
                    rusqlite::params![],
                )
                .map_err(db_err)?;
                tx.execute(
                    "INSERT INTO templates(id, repo_id, base_commit, tree_path, index_path, state, created_at)
                     VALUES (1, 1, 'c0ffee', 't', 'i', 'ready', 0)",
                    rusqlite::params![],
                )
                .map_err(db_err)?;
                Ok(())
            })
            .unwrap();
    }

    fn make(fx: &Fixture, dedupe: bool) -> Overlay {
        let mut cfg = OverlayConfig::new(WorktreeId(1), &fx.upper_dir);
        cfg.lower_root = Some(fx.lower_dir.clone());
        cfg.template_id = Some(TemplateId(1));
        cfg.dedupe_enabled = dedupe;
        Overlay::new(cfg, Some(lower_fixture(&fx.lower_dir)), fx.store.clone()).unwrap()
    }

    /// list/枚举类测试用：跳过 initializing（视图已就绪）。
    fn make_ready(fx: &Fixture) -> Overlay {
        let of = make(fx, true);
        of.mark_ready().unwrap();
        of
    }

    fn names(of: &Overlay, dir: &str) -> Vec<String> {
        of.list(dir)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect()
    }

    #[test]
    fn resolution_precedence_upper_over_whiteout_over_lower() {
        let fx = fixture(true);
        let of = make(&fx, true);
        assert_eq!(of.worktree_id(), WorktreeId(1));

        // lower 命中
        match of.resolve("b.txt").unwrap() {
            Resolution::Lower { file, path } => {
                assert_eq!(file.hash, crate::dedupe::hash_bytes(BEE));
                assert!(path.ends_with("b.txt"));
            }
            other => panic!("expected Lower, got {other:?}"),
        }
        assert_eq!(of.read("b.txt").unwrap().as_deref(), Some(BEE));

        // upper 覆盖 lower
        let (state, size) = of.write("b.txt", b"changed").unwrap();
        assert_eq!(state, DedupeState::Upper);
        assert_eq!(size, 7);
        assert!(matches!(of.resolve("b.txt").unwrap(), Resolution::Upper { .. }));
        assert_eq!(of.read("b.txt").unwrap().as_deref(), Some(b"changed".as_slice()));

        // 改回模板内容 → 回收 upper，视图回落 lower
        let (state, _) = of.write("b.txt", BEE).unwrap();
        assert_eq!(state, DedupeState::Lower);
        assert!(!fx.upper_dir.join("b.txt").exists());
        assert_eq!(of.read("b.txt").unwrap().as_deref(), Some(BEE));

        // 删除 → whiteout（lower 有）
        assert!(of.remove("b.txt").unwrap());
        assert_eq!(of.resolve("b.txt").unwrap(), Resolution::Whiteout);
        assert_eq!(of.read("b.txt").unwrap(), None);

        // 重写一致内容 → whiteout 撤销，重新可见
        let (state, _) = of.write("b.txt", BEE).unwrap();
        assert_eq!(state, DedupeState::Lower);
        assert_eq!(of.read("b.txt").unwrap().as_deref(), Some(BEE));

        // 新文件 → New
        let (state, _) = of.write("new.txt", b"fresh").unwrap();
        assert_eq!(state, DedupeState::New);
        assert!(matches!(of.resolve("new.txt").unwrap(), Resolution::Upper { .. }));

        // 目录与 not-found
        assert!(matches!(
            of.resolve("a").unwrap(),
            Resolution::Directory { has_lower: true, has_upper: false }
        ));
        assert_eq!(of.resolve("nope").unwrap(), Resolution::NotFound);
        assert!(matches!(
            of.resolve("").unwrap(),
            Resolution::Directory { has_lower: true, has_upper: true }
        ));
        assert!(of.read("a").is_err(), "read on directory errors");
    }

    #[test]
    fn directory_merge_and_dir_whiteout() {
        let fx = fixture(true);
        let of = make_ready(&fx);
        let mut root = names(&of, "");
        root.sort();
        assert_eq!(root, vec!["a", "b.txt", "exec.sh"]);
        assert_eq!(names(&of, "a"), vec!["1.txt", "2.txt"]);

        // upper 新文件并入
        of.write("a/3.txt", b"three").unwrap();
        assert_eq!(names(&of, "a"), vec!["1.txt", "2.txt", "3.txt"]);

        // 删 lower-only 目录 → whiteout 隐藏整棵子树
        assert!(of.remove("a").unwrap());
        assert!(!names(&of, "").contains(&"a".to_string()));
        assert_eq!(of.resolve("a/1.txt").unwrap(), Resolution::Whiteout);

        // upper 重建目录 → 只显示 upper 内容，lower 子树仍隐藏
        of.write("a/9.txt", b"nine").unwrap();
        assert!(matches!(
            of.resolve("a").unwrap(),
            Resolution::Directory { has_lower: false, has_upper: true }
        ));
        assert_eq!(names(&of, "a"), vec!["9.txt"]);
        assert!(names(&of, "").contains(&"a".to_string()));
    }

    #[test]
    fn write_template_identical_under_dir_whiteout_stays_visible() {
        // P1-1 回归：目录级 whiteout 下写模板一致内容，不得回落 lower
        // （祖先 whiteout 会继续隐藏子树 → "写入成功但文件不可见"）。
        let fx = fixture(true);
        let of = make(&fx, true);
        of.mark_ready().unwrap();

        assert!(of.remove("a").unwrap());
        assert_eq!(of.resolve("a").unwrap(), Resolution::Whiteout);
        // 目录 whiteout 下 copy_up 不可见即不可复制（返回 None，不产生副本）
        assert_eq!(of.copy_up("a/1.txt").unwrap(), None);

        // 与模板完全一致的重写：保留为 upper，文件可解析、可见、内容正确
        let (state, _) = of.write("a/1.txt", ONE).unwrap();
        assert_eq!(state, DedupeState::Upper);
        assert!(matches!(of.resolve("a/1.txt").unwrap(), Resolution::Upper { .. }));
        assert_eq!(of.read("a/1.txt").unwrap().as_deref(), Some(ONE));
        assert!(names(&of, "a").contains(&"1.txt".to_string()));
        assert!(matches!(
            of.resolve("a").unwrap(),
            Resolution::Directory { has_lower: false, has_upper: true }
        ));

        // 对照组：文件级 whiteout 的旧行为不变——精确路径一致重写回落 lower 且可见
        assert!(of.remove("b.txt").unwrap());
        let (state, _) = of.write("b.txt", BEE).unwrap();
        assert_eq!(state, DedupeState::Lower);
        assert_eq!(of.read("b.txt").unwrap().as_deref(), Some(BEE));
        assert!(names(&of, "").contains(&"b.txt".to_string()));
    }

    #[test]
    fn write_rejects_reserved_temp_prefix_and_hides_temp_files() {
        let fx = fixture(true);
        let of = make(&fx, true);
        assert!(of.write(".wtfs-tmp-1-1", b"x").is_err());
        std::fs::write(fx.upper_dir.join(".wtfs-tmp-9-9"), b"leftover").unwrap();
        assert!(!names(&of, "").contains(&".wtfs-tmp-9-9".to_string()));
        assert_eq!(of.resolve(".wtfs-tmp-9-9").unwrap(), Resolution::NotFound);
        assert_eq!(of.stat(".wtfs-tmp-9-9").unwrap(), None);
    }

    #[test]
    fn initializing_hides_listing_only() {
        let fx = fixture(true);
        let of = make(&fx, true);
        assert!(of.is_initializing().unwrap());
        assert_eq!(of.list("").unwrap().len(), 0, "readdir empty during checkout");
        // 非枚举操作照常（checkout 期间的写入/stat 依赖它）
        assert!(matches!(of.resolve("b.txt").unwrap(), Resolution::Lower { .. }));
        of.mark_ready().unwrap();
        assert!(!of.is_initializing().unwrap());
        assert!(!of.list("").unwrap().is_empty());
        of.mark_failed().unwrap();
        assert!(!of.is_initializing().unwrap());
    }

    #[test]
    fn rename_dir_with_rename_map() {
        let fx = fixture(true);
        let of = make_ready(&fx);
        of.write("a/3.txt", b"three").unwrap();
        of.rename("a", "z").unwrap();

        // 源路径消失；目标目录合并 lower(rename map) + upper(物理随迁)
        assert_eq!(of.resolve("a").unwrap(), Resolution::NotFound);
        assert!(matches!(
            of.resolve("z").unwrap(),
            Resolution::Directory { has_lower: true, has_upper: true }
        ));
        assert_eq!(names(&of, "z"), vec!["1.txt", "2.txt", "3.txt"]);
        assert_eq!(of.read("z/1.txt").unwrap().as_deref(), Some(ONE));
        // lower 内容经 rename map 命中，stat 仍是固定值
        match of.resolve("z/1.txt").unwrap() {
            Resolution::Lower { file, .. } => assert_eq!(file.mode, 0o100644),
            other => panic!("expected Lower, got {other:?}"),
        }
        // 再做一次文件级重命名（upper → upper）
        of.rename("z/3.txt", "top.txt").unwrap();
        assert_eq!(of.resolve("z/3.txt").unwrap(), Resolution::NotFound);
        assert!(matches!(of.resolve("top.txt").unwrap(), Resolution::Upper { .. }));
        assert_eq!(of.read("top.txt").unwrap().as_deref(), Some(b"three".as_slice()));
        assert_eq!(names(&of, "z"), vec!["1.txt", "2.txt"]);

        // 错误分支：目标为目录 / 源不存在 / 自子树
        assert!(of.rename("top.txt", "z").is_err());
        assert!(of.rename("ghost", "z2").is_err());
        assert!(of.rename("z", "z/inner").is_err());
    }

    #[test]
    fn rename_lower_file_copies_up_and_whiteouts_source() {
        let fx = fixture(true);
        let of = make(&fx, true);
        of.rename("b.txt", "c.txt").unwrap();
        // 源路径：lower 有文件 → whiteout（VFS 语义 = 不存在）
        assert_eq!(of.resolve("b.txt").unwrap(), Resolution::Whiteout);
        assert!(matches!(of.resolve("c.txt").unwrap(), Resolution::Upper { .. }));
        assert_eq!(of.read("c.txt").unwrap().as_deref(), Some(BEE));
        // c.txt 的 stat 有逻辑 mtime（upper 文件）
        let st = of.stat("c.txt").unwrap().unwrap();
        assert!(st.mtime_ns > crate::FIXED_STAT_EPOCH_UNIX_NS);
        assert_eq!(st.mode, 0o100644);
    }

    #[test]
    fn stat_fixed_lower_logical_upper_and_monotonic_mtime() {
        let fx = fixture(true);
        let of = make(&fx, true);
        let epoch = crate::FIXED_STAT_EPOCH_UNIX_NS;
        let vs = u64::from(crate::DEFAULT_VOLUME_SERIAL);

        // lower：固定 stat
        let st = of.stat("exec.sh").unwrap().unwrap();
        assert_eq!(st, Stat {
            dev: vs,
            ino: of.file_id("exec.sh").unwrap(),
            mode: 0o100755,
            size: RUN.len() as u64,
            mtime_ns: epoch,
            ctime_ns: epoch,
        });
        // 目录
        let dir_st = of.stat("a").unwrap().unwrap();
        assert_eq!(dir_st.mode, 0o040755);
        assert_eq!(dir_st.size, 0);
        assert_eq!(attributes_for(&dir_st), FileAttributes::DIRECTORY);
        assert_eq!(attributes_for(&st), FileAttributes::ARCHIVE);

        // upper：逻辑 mtime，且保留模板可执行位
        of.write("exec.sh", b"modified").unwrap();
        let st1 = of.stat("exec.sh").unwrap().unwrap();
        assert_eq!(st1.mode, 0o100755, "exec bit survives copy-up");
        assert!(st1.mtime_ns > epoch);
        assert_eq!(st1.size, 8);
        assert_eq!(st1.ino, of.file_id("exec.sh").unwrap());
        // 同一路径重复 stat 稳定
        assert_eq!(of.stat("exec.sh").unwrap().unwrap().mtime_ns, st1.mtime_ns);

        // 多次写：严格单调递增
        let (state, _) = of.write("a/1.txt", ONE).unwrap();
        assert_eq!(state, DedupeState::Lower, "模板一致内容回落 lower");
        let (state, _) = of.write("a/1.txt", b"ONE!").unwrap();
        assert_eq!(state, DedupeState::Upper);
        let t1 = of.stat("a/1.txt").unwrap().unwrap().mtime_ns;
        of.write("a/1.txt", b"ONE!!").unwrap();
        let t2 = of.stat("a/1.txt").unwrap().unwrap().mtime_ns;
        assert!(t2 > t1, "logical mtime must advance: {t1} -> {t2}");
        assert!(of.stat("gone").unwrap().is_none());
    }

    #[test]
    fn copy_up_then_read_modify_write() {
        let fx = fixture(true);
        let of = make(&fx, true);
        let p = of.copy_up("a/2.txt").unwrap().unwrap();
        assert!(p.is_file());
        // copy-up 后走 upper
        assert!(matches!(of.resolve("a/2.txt").unwrap(), Resolution::Upper { .. }));
        std::fs::write(&p, b"TWO!").unwrap();
        assert_eq!(of.read("a/2.txt").unwrap().as_deref(), Some(b"TWO!".as_slice()));
        // 不存在的路径
        assert_eq!(of.copy_up("nope").unwrap(), None);
    }

    #[test]
    fn dedupe_disabled_writes_full_copy() {
        let fx = fixture(true);
        let of = make(&fx, false);
        let (state, _) = of.write("b.txt", BEE).unwrap();
        assert_eq!(state, DedupeState::Upper, "dedupe off → 内容留在 upper");
        assert!(fx.upper_dir.join("b.txt").is_file());
        assert_eq!(of.read("b.txt").unwrap().as_deref(), Some(BEE));
    }

    #[test]
    fn no_template_falls_back_to_full_upper_copy() {
        let dir = TempDir::new().unwrap();
        let upper_dir = dir.path().join("upper");
        std::fs::create_dir_all(&upper_dir).unwrap();
        let store = Arc::new(SqliteOverlayStore::new(Db::open_in_memory().unwrap()));
        let cfg = OverlayConfig::new(WorktreeId(1), &upper_dir); // 无模板（兜底模式）
        let of = Overlay::new(cfg, None, store).unwrap();
        of.mark_ready().unwrap();
        assert!(of.lower().is_none());

        let (state, _) = of.write("x.txt", b"whatever").unwrap();
        assert_eq!(state, DedupeState::New, "无模板 → 全部 New/upper");
        assert!(matches!(of.resolve("x.txt").unwrap(), Resolution::Upper { .. }));
        assert_eq!(of.read("x.txt").unwrap().as_deref(), Some(b"whatever".as_slice()));
        assert_eq!(names(&of, ""), vec!["x.txt"]);
        // 删除无 lower → 无 whiteout，直接消失
        assert!(of.remove("x.txt").unwrap());
        assert_eq!(of.resolve("x.txt").unwrap(), Resolution::NotFound);
        assert!(of.store().whiteouts(WorktreeId(1)).unwrap().is_empty());
    }

    #[test]
    fn metadata_persists_across_reopen() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("meta.db");
        let lower_dir = dir.path().join("lower");
        let upper_dir = dir.path().join("upper");
        let db = Db::open(&db_path).unwrap();
        let store = Arc::new(SqliteOverlayStore::new(db));
        let mut cfg = OverlayConfig::new(WorktreeId(3), &upper_dir);
        cfg.lower_root = Some(lower_dir.clone());
        cfg.volume_serial = 0x00BE_EF00;
        let (of, ino, mtime) = {
            let mut cfg = cfg.clone();
            cfg.dedupe_enabled = true;
            let of = Overlay::new(cfg, Some(lower_fixture(&lower_dir)), store.clone()).unwrap();
            of.write("a/1.txt", b"ONE!").unwrap();
            of.remove("b.txt").unwrap();
            let ino = of.file_id("a/1.txt").unwrap();
            let mtime = of.stat("a/1.txt").unwrap().unwrap().mtime_ns;
            (of, ino, mtime)
        };
        drop(of);

        // 用文件库重开（模拟崩溃恢复）
        let db2 = Db::open(&db_path).unwrap();
        let store2 = Arc::new(SqliteOverlayStore::new(db2));
        let of2 = Overlay::reopen(cfg.clone(), Some(lower_fixture(&lower_dir)), store2.clone()).unwrap();
        // config 往返
        assert_eq!(of2.config().volume_serial, 0x00BE_EF00);
        assert!(of2.config().dedupe_enabled);
        // whiteout 持久
        assert_eq!(of2.resolve("b.txt").unwrap(), Resolution::Whiteout);
        // file id 与逻辑 mtime 持久（不重发放）
        assert_eq!(of2.file_id("a/1.txt").unwrap(), ino);
        assert_eq!(of2.stat("a/1.txt").unwrap().unwrap().mtime_ns, mtime);
        // 状态机持久
        of2.mark_ready().unwrap();
        assert_eq!(
            of2.store().state(WorktreeId(3)).unwrap(),
            Some(crate::WorktreeState::Ready)
        );
        // 重开到未注册 id 报 NotFound
        let mut cfg4 = cfg.clone();
        cfg4.worktree_id = WorktreeId(4);
        let err = match Overlay::reopen(cfg4, None, store2) {
            Err(e) => e,
            Ok(_) => panic!("reopen of unregistered worktree must fail"),
        };
        assert!(matches!(err, Error::NotFound(_)));
    }
}
