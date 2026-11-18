//! CAS 视图模型：manifest（lower）+ upper 真实文件 + whiteout 的 overlay
//! 解析（C 方案的 [`Overlay`] 对应物，不依赖 wtfs-core）。
//!
//! 解析顺序与 B1 一致（upper > whiteout > lower）；lower 命中按需 hydrate：
//!
//! * **stat**（[`CasView::stat`]）：upper 文件 = 真实 mtime/size（mode 取
//!   manifest 的可执行位，upper-only 文件默认 `0o100644`）；manifest 文件 =
//!   固定 stat（dev=[`DEFAULT_VOLUME_SERIAL`]、ino=[`file_id`]、size/mode
//!   来自 manifest、mtime=ctime=[`FIXED_STAT_EPOCH_UNIX_NS`]）；目录 =
//!   `0o040755`（有 upper 用真实 mtime，否则固定纪元）；whiteout/not-found
//!   → `None`。零写副作用。
//! * **list / list_stat**：manifest 子项索引 + upper 真实 readdir 合并，
//!   按名排序、upper 覆盖 manifest；`.wtfs-tmp` 前缀临时文件对视图隐藏。
//!   `list_stat` 单次 readdir + 逐项一次 metadata，不逐子项全量重解析
//!   （与 B1 的 M13 优化同一形态，保证基准可比）。
//! * **read**：upper 命中读真实文件；manifest 命中经 [`HydrationCache::ensure`]
//!   灌入后读缓存文件——"读到即已物化"，未读路径零对象 I/O。目录报错；
//!   whiteout/not-found → `None`；gitlink（160000）对象库无 blob → 读报
//!   NotFound；symlink（120000）按 blob 内容（链接目标字节）读出。
//! * **write**（M18 与 B1 同语义）：暂存/就地内容经 git blob sha1
//!   （`blob <len>\0` + 内容）回查 manifest oid——一致 → 回收 upper 回落
//!   manifest（零 upper、零 fsync，[`WriteState::Lower`]）；不一致 →
//!   temp+fsync+rename 原子落 upper（恰好一次 fsync，[`WriteState::Upper`/
//!   `WriteState::New`]）。[`CasView::copy_up`] 支撑就地写：hydrate 后复制
//!   到 upper 并把 mtime 归一到固定纪元（copy-up 本身不打脏 index）。
//!   [`CasView::prepare_created_file`] 保证 create 回调后路径立即可解析
//!   （B100100 同款）；[`CasView::rename`] 文件级 = copy/rename upper +
//!   M12 标记，目录级 lower 有内容默认 EXDEV。
//! * **remove**：upper 删真实文件/目录；manifest 有内容（自身或子树）则
//!   记 whiteout（[`MarkerLog`]，M12 日志语义）。纯 manifest 删除零
//!   upper 文件操作——"删除不 hydrate"。
//! * **refresh**（M18）：[`CasView::refresh_head`] 在 HEAD 前进（commit 后）
//!   重载 manifest 并剪枝失效 whiteout（新 manifest 已无该路径的标记）；
//!   stat 不含 oid，刷新不动摇 index 对齐。
//!
//! upper 负存在缓存（M14 优化）未移植：MVP 每次解析一次真实探测
//! （`symlink_metadata`）；M14 的收益主要在 B1 的逐回调探测序列，C 的
//! 基准口径（stat 4,874 文件）不受影响，M18 按需移植。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use crate::cache::HydrationCache;
use crate::cat_file::CatFile;
use crate::manifest::{EntryKind, Manifest};
use crate::markers::MarkerLog;
use crate::{
    CasStat, DEFAULT_VOLUME_SERIAL, Error, FIXED_STAT_EPOCH_UNIX_NS, MODE_DIR, MODE_REGULAR,
    Result, TEMP_PREFIX, file_id, validate_relative_path,
};

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 单个 CAS worktree 的视图配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CasViewConfig {
    /// worktree 私有 upper 根（真实文件；挂载时由 FS 合成到视图根）。
    pub upper_root: PathBuf,
    /// 删除标记日志根（`<meta_root>/markers.log`；通常 = upper 父目录/meta，
    /// 与 M12 布局一致）。
    pub meta_root: PathBuf,
    /// manifest 文件固定 dev。
    pub volume_serial: u32,
    /// manifest 文件固定 mtime/ctime（Unix 纳秒）。
    pub fixed_stat_epoch_ns: i64,
}

impl CasViewConfig {
    /// 以默认常量构造（与 wtfs-core `OverlayConfig::new` 默认值一致）。
    pub fn new(upper_root: impl Into<PathBuf>, meta_root: impl Into<PathBuf>) -> Self {
        Self {
            upper_root: upper_root.into(),
            meta_root: meta_root.into(),
            volume_serial: DEFAULT_VOLUME_SERIAL,
            fixed_stat_epoch_ns: FIXED_STAT_EPOCH_UNIX_NS,
        }
    }
}

/// 解析结果（对挂载层公开；[`CasView::resolve`]）。
#[derive(Debug, Clone)]
pub enum CasResolution {
    NotFound,
    /// 被 whiteout 隐藏且 upper/lower 均无文件形态（upper 目录重建除外）。
    Whiteout,
    /// upper 命中（文件）；`path` 为 upper 物理路径。
    Upper {
        path: PathBuf,
    },
    /// manifest（lower）命中。
    CasFile,
    /// 目录：manifest/upper 两侧存在性（合并枚举用）。
    Directory {
        has_cas: bool,
        has_upper: bool,
    },
}

/// 每文件三态缓存：Lower（manifest）/ Upper（真实 upper 文件）/ Deleted
/// （whiteout）。目录仍走 manifest/upper 推导；status 热路径只关心文件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    Lower,
    Upper,
    Deleted,
}

/// [`CasView::list`] 的条目；`rel` 是相对视图根的完整路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CasDirEntry {
    pub name: String,
    pub rel: String,
    pub kind: EntryKind,
}

/// [`CasView::write`] / [`CasView::write_from`] / [`CasView::finish_staged`]
/// 的返回：`Lower` = 内容与 manifest 一致已回收（零 upper 回落），
/// `Upper` = 覆盖既有 manifest 路径落 upper，`New` = 全新路径落 upper。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteState {
    Lower,
    Upper,
    New,
}

/// [`CasView::write`] 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteOutcome {
    pub state: WriteState,
    pub bytes: u64,
}

/// 暂存写终裁的内容来源（[`CasView::finish_staged`]；与 wtfs-core
/// `StagedContent` 同构）。
#[derive(Debug)]
pub enum StagedSource<'a> {
    /// 完整内容已在内存。
    Memory(&'a [u8]),
    /// 溢写文件（upper 目录内 `.wtfs-tmp-w-*`，与最终 upper 路径同卷）。
    /// 调用方必须已关闭句柄（Windows 不允许 rename 打开中的文件）。
    Spill(&'a std::path::Path),
}

/// 一个 worktree 的 CAS 视图。读方法 `&self`，写方法 `&mut self`
/// （whiteout 镜像与日志追加的单写者约束；跨线程共享由调用方加锁）。
pub struct CasView {
    config: CasViewConfig,
    manifest: Arc<Manifest>,
    cat: Arc<CatFile>,
    cache: Arc<HydrationCache>,
    markers: MarkerLog,
    /// upper 负存在缓存（M14 同款）：只缓存“不存在”，upper 任何变更整体
    /// 失效。clean worktree 的 upper 几乎为空，`resolve` 每文件一次失败
    /// NTFS 探测是 status 的主要差价。
    upper_neg: Mutex<HashSet<String>>,
    /// 每文件三态索引：启动时从 manifest + upper 扫描 + marker replay 构建；
    /// 运行时由写/删/rename 增量更新。一次 `HashMap` 查询替代
    /// upper 探测 + whiteout + manifest 三次推导。
    state: Mutex<HashMap<String, FileState>>,
}

/// 启动时构建每文件三态索引：manifest 文件 → Lower；upper 递归扫描 →
/// Upper；marker replay → Deleted（upper 已存在时 upper 优先）。
fn build_file_state(
    upper_root: &std::path::Path,
    manifest: &Manifest,
    whites: &HashSet<String>,
) -> HashMap<String, FileState> {
    let mut state = HashMap::with_capacity(manifest.files().len() + whites.len());
    for path in manifest.files().keys() {
        state.insert(path.clone(), FileState::Lower);
    }
    scan_upper_files(upper_root, "", &mut state);
    for path in whites {
        let upper_path = upper_root.join(path.replace('/', "\\"));
        if upper_path.symlink_metadata().is_err() {
            state.insert(path.clone(), FileState::Deleted);
        }
    }
    state
}

fn scan_upper_files(
    dir: &std::path::Path,
    prefix: &str,
    state: &mut HashMap<String, FileState>,
) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if is_temp_name(name) {
            continue;
        }
        let rel = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            scan_upper_files(&entry.path(), &rel, state);
        } else {
            state.insert(rel, FileState::Upper);
        }
    }
}

impl CasView {
    /// 组装视图（manifest/cat/cache 由调用方构建共享；markers 按 config
    /// 打开重放）。
    pub fn open(
        config: CasViewConfig,
        manifest: Arc<Manifest>,
        cat: Arc<CatFile>,
        cache: Arc<HydrationCache>,
    ) -> Result<Self> {
        let markers = MarkerLog::open(&config.meta_root)?;
        let state = build_file_state(&config.upper_root, &manifest, markers.whites());
        Ok(Self {
            config,
            manifest,
            cat,
            cache,
            markers,
            upper_neg: Mutex::new(HashSet::new()),
            state: Mutex::new(state),
        })
    }

    pub fn config(&self) -> &CasViewConfig {
        &self.config
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn cache(&self) -> &HydrationCache {
        &self.cache
    }

    pub fn cat(&self) -> &CatFile {
        &self.cat
    }

    /// 存活 whiteout 集合（内存镜像）。
    pub fn whites(&self) -> &HashSet<String> {
        self.markers.whites()
    }

    /// upper 物理路径（`""` = upper 根）。
    ///
    /// 配置根可能带 `\\?\` 长路径前缀；verbatim 路径不接受 `/` 作为
    /// 分隔符（Win32 报 ERROR_INVALID_PARAMETER），这里统一换成 `\`。
    pub fn upper_abs(&self, rel: &str) -> PathBuf {
        if rel.is_empty() {
            self.config.upper_root.clone()
        } else {
            self.config.upper_root.join(rel.replace('/', "\\"))
        }
    }

    /// 稳定 file id（Windows FILE_ID 与 index ino 用）：与 wtfs-core 同一
    /// blake3(小写路径) 派生；空路径报 InvalidPath，根目录请用常量。
    pub fn file_id(&self, rel: &str) -> Result<u64> {
        validate_relative_path(rel)?;
        Ok(file_id(rel))
    }

    // ---- 解析 ----

    /// 解析视图路径（挂载层路由/写路径状态机用）。
    pub fn resolve(&self, rel: &str) -> Result<CasResolution> {
        Ok(self.resolve_with_md(rel)?.0)
    }

    /// 解析并回传 upper metadata（M13 同款）：`stat` 复用这次探测，避免
    /// upper 文件 resolve + stat 两次 NTFS metadata。
    fn resolve_with_md(
        &self,
        rel: &str,
    ) -> Result<(CasResolution, Option<std::fs::Metadata>)> {
        if !rel.is_empty() && is_temp_name(basename(rel)) {
            return Ok((CasResolution::NotFound, None));
        }
        if !rel.is_empty() {
            let cached = self.state.lock().get(rel).copied();
            if let Some(state) = cached {
                match state {
                    FileState::Lower => {
                        if self.markers.blocks(rel) {
                            return Ok((CasResolution::Whiteout, None));
                        }
                        return Ok((CasResolution::CasFile, None));
                    }
                    FileState::Upper => {
                        if self.markers.blocks(rel) {
                            return Ok((CasResolution::Whiteout, None));
                        }
                        let path = self.upper_abs(rel);
                        if let Some(md) = self.probe_upper(rel) {
                            return Ok((CasResolution::Upper { path }, Some(md)));
                        }
                        // 状态说 Upper 但探测不到（竞态）：回落通用推导。
                    }
                    FileState::Deleted => return Ok((CasResolution::Whiteout, None)),
                }
            }
        }
        let upper_path = self.upper_abs(rel);
        let upper_md = self.probe_upper(rel);
        let upper_is_file = upper_md.as_ref().is_some_and(|md| !md.is_dir());
        let upper_is_dir = upper_md.as_ref().is_some_and(|md| md.is_dir());
        if upper_is_file {
            return Ok((CasResolution::Upper { path: upper_path }, upper_md));
        }
        if !rel.is_empty() && self.markers.blocks(rel) {
            // upper 文件已查无；upper 目录可能已重建（opaque 语义：只显示
            // upper 内容，manifest 子树继续隐藏）。
            return Ok((
                if upper_is_dir {
                    CasResolution::Directory {
                        has_cas: false,
                        has_upper: true,
                    }
                } else {
                    CasResolution::Whiteout
                },
                upper_md,
            ));
        }
        if self.manifest.file(rel).is_some() {
            return Ok((CasResolution::CasFile, upper_md));
        }
        let has_cas = rel.is_empty() || self.manifest.has_dir(rel);
        if has_cas || upper_is_dir {
            return Ok((
                CasResolution::Directory {
                    has_cas,
                    has_upper: upper_is_dir,
                },
                upper_md,
            ));
        }
        Ok((CasResolution::NotFound, upper_md))
    }

    /// upper 负存在探测：命中缓存直接返回 None；探测到存在时回收负条目。
    /// 只缓存 NotFound，瞬态错误不缓存。
    fn probe_upper(&self, rel: &str) -> Option<std::fs::Metadata> {
        if rel.is_empty() {
            return std::fs::symlink_metadata(self.config.upper_root.clone()).ok();
        }
        if self.upper_neg.lock().contains(rel) {
            return None;
        }
        match std::fs::symlink_metadata(self.upper_abs(rel)) {
            Ok(md) => {
                self.upper_neg.lock().remove(rel);
                Some(md)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // 只缓存 manifest 文件的负结果；目录/未知路径不缓存，避免
                // 外部重建目录（测试/旁路）后 stale。
                if self.manifest.file(rel).is_some() {
                    self.upper_neg.lock().insert(rel.to_string());
                }
                None
            }
            Err(_) => None,
        }
    }

    /// upper 任何变更后整体失效（M14 口径：只缓存负状态，清空只会让
    /// 缓存更空，不会留下陈旧正/负条目）。
    pub fn invalidate_upper(&self) {
        self.upper_neg.lock().clear();
    }

    fn set_file_state(&self, rel: &str, state: FileState) {
        self.state.lock().insert(rel.to_string(), state);
    }

    fn remove_file_state(&self, rel: &str) {
        self.state.lock().remove(rel);
    }

    fn remove_subtree_state(&self, rel: &str) {
        let mut state = self.state.lock();
        state.remove(rel);
        let prefix = format!("{rel}/");
        state.retain(|path, _| !path.starts_with(&prefix));
    }

    /// upper 直接变更后的状态修正（VFS 侧 create/write/truncate 等无 CAS
    /// 返回值时调用）：按真实 upper 形态更新三态索引。
    pub fn note_upper_mutation(&self, rel: &str) {
        if rel.is_empty() {
            return;
        }
        match std::fs::symlink_metadata(self.upper_abs(rel)) {
            Ok(md) if !md.is_dir() => self.set_file_state(rel, FileState::Upper),
            Ok(_) => self.remove_file_state(rel),
            Err(_) if self.manifest.file(rel).is_some() => {
                self.set_file_state(rel, FileState::Lower)
            }
            Err(_) => self.remove_file_state(rel),
        }
        self.invalidate_upper();
    }

    // ---- stat ----

    /// 推导 stat；whiteout/not-found → `None`。纯读：零 hydrate、零写副作用。
    pub fn stat(&self, rel: &str) -> Result<Option<CasStat>> {
        validate_rel_or_root(rel)?;
        if !rel.is_empty() && is_temp_name(basename(rel)) {
            return Ok(None);
        }
        let vs = u64::from(self.config.volume_serial);
        let epoch = self.config.fixed_stat_epoch_ns;
        let (resolution, upper_md) = self.resolve_with_md(rel)?;
        match resolution {
            CasResolution::Whiteout | CasResolution::NotFound => Ok(None),
            CasResolution::Upper { path } => {
                let md = match upper_md {
                    Some(md) => md,
                    None => std::fs::symlink_metadata(&path)?,
                };
                let mode = match self.manifest.file(rel) {
                    // upper 覆盖既有 manifest 路径：保留 manifest 可执行位。
                    Some(f) if f.mode & 0o111 != 0 => 0o100755,
                    Some(_) => MODE_REGULAR,
                    // upper-only：无 manifest 依据，默认常规文件（与 B1 一致）。
                    None => MODE_REGULAR,
                };
                let mtime = metadata_mtime_ns(&md);
                Ok(Some(CasStat {
                    dev: vs,
                    ino: file_id(rel),
                    mode,
                    size: md.len(),
                    mtime_ns: mtime,
                    ctime_ns: mtime,
                }))
            }
            CasResolution::CasFile => {
                // 防御：resolve 已确认清单命中。
                let f = self.manifest.file(rel).expect("resolve confirmed manifest hit");
                Ok(Some(CasStat {
                    dev: vs,
                    ino: file_id(rel),
                    mode: f.mode,
                    size: f.size,
                    mtime_ns: epoch,
                    ctime_ns: epoch,
                }))
            }
            CasResolution::Directory { has_upper, .. } => {
                let mtime = if has_upper {
                    match upper_md {
                        Some(md) => metadata_mtime_ns(&md),
                        None => metadata_mtime_ns(&std::fs::symlink_metadata(self.upper_abs(rel))?),
                    }
                } else {
                    epoch
                };
                Ok(Some(CasStat {
                    dev: vs,
                    ino: file_id(rel),
                    mode: MODE_DIR,
                    size: 0,
                    mtime_ns: mtime,
                    ctime_ns: mtime,
                }))
            }
        }
    }

    // ---- 枚举 ----

    /// 目录合并枚举：manifest 子项索引 + upper 真实 readdir，按名字序、
    /// upper 覆盖 manifest；临时文件不可见。目录自身被 whiteout 时只出
    /// upper 内容（opaque 语义）。upper 目录不存在（manifest-only 目录，
    /// 干净视图常态）不报错。
    pub fn list(&self, dir_rel: &str) -> Result<Vec<CasDirEntry>> {
        validate_rel_or_root(dir_rel)?;
        let mut names: BTreeMap<String, EntryKind> = BTreeMap::new();

        // manifest 侧：目录被 whiteout 阻塞（自身或祖先）时整侧跳过。
        if !self.markers.blocks(dir_rel) {
            if let Some(entries) = self.manifest.children_of(dir_rel) {
                for (name, kind) in entries {
                    let child = join_rel(dir_rel, name);
                    // 子项路径自身/祖先被删 → 隐藏（与 stat 的阻塞口径一致）。
                    if self.markers.blocks(&child) {
                        continue;
                    }
                    names.insert(name.clone(), *kind);
                }
            }
        }

        // upper 侧：真实目录，覆盖/补充 manifest。
        if let Ok(rd) = std::fs::read_dir(self.upper_abs(dir_rel)) {
            for entry in rd.flatten() {
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

        Ok(names
            .into_iter()
            .map(|(name, kind)| CasDirEntry {
                rel: join_rel(dir_rel, &name),
                name,
                kind,
            })
            .collect())
    }

    /// 目录合并枚举 + 逐项 stat：与 [`CasView::list`] 同一合并规则，子项
    /// stat 由 manifest 清单/枚举元数据直接推导（不逐子项 [`CasView::stat`]
    /// 全量重解析——4,874 文件基准的 stat 口径）。
    /// 返回按名字排序的 (name, stat)。
    pub fn list_stat(&self, dir_rel: &str) -> Result<Vec<(String, CasStat)>> {
        validate_rel_or_root(dir_rel)?;
        let vs = u64::from(self.config.volume_serial);
        let epoch = self.config.fixed_stat_epoch_ns;
        let mut stats: BTreeMap<String, CasStat> = BTreeMap::new();

        if !self.markers.blocks(dir_rel) {
            if let Some(entries) = self.manifest.children_of(dir_rel) {
                for (name, kind) in entries {
                    let child = join_rel(dir_rel, name);
                    if self.markers.blocks(&child) {
                        continue;
                    }
                    let st = match kind {
                        EntryKind::File => {
                            let Some(f) = self.manifest.file(&child) else {
                                continue; // 防御：children 索引由文件行派生，File 必有清单
                            };
                            CasStat {
                                dev: vs,
                                ino: file_id(&child),
                                mode: f.mode,
                                size: f.size,
                                mtime_ns: epoch,
                                ctime_ns: epoch,
                            }
                        }
                        EntryKind::Directory => CasStat {
                            dev: vs,
                            ino: file_id(&child),
                            mode: MODE_DIR,
                            size: 0,
                            mtime_ns: epoch,
                            ctime_ns: epoch,
                        },
                    };
                    stats.insert(name.clone(), st);
                }
            }
        }

        let Ok(rd) = std::fs::read_dir(self.upper_abs(dir_rel)) else {
            return Ok(stats.into_iter().collect());
        };
        for entry in rd {
            let Ok(entry) = entry else { continue };
            let os_name = entry.file_name();
            let Some(name) = os_name.to_str() else {
                tracing::warn!(name = ?os_name, "skip non-UTF-8 upper name");
                continue;
            };
            if is_temp_name(name) {
                continue;
            }
            let view_child = join_rel(dir_rel, name);
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let md = entry.metadata()?;
            let st = if is_dir {
                let mtime = metadata_mtime_ns(&md);
                CasStat {
                    dev: vs,
                    ino: file_id(&view_child),
                    mode: MODE_DIR,
                    size: 0,
                    mtime_ns: mtime,
                    ctime_ns: mtime,
                }
            } else {
                let mode = match self.manifest.file(&view_child) {
                    Some(f) if f.mode & 0o111 != 0 => 0o100755,
                    Some(_) => MODE_REGULAR,
                    None => MODE_REGULAR,
                };
                let mtime = metadata_mtime_ns(&md);
                CasStat {
                    dev: vs,
                    ino: file_id(&view_child),
                    mode,
                    size: md.len(),
                    mtime_ns: mtime,
                    ctime_ns: mtime,
                }
            };
            stats.insert(name.to_string(), st);
        }
        Ok(stats.into_iter().collect())
    }

    // ---- 读 ----

    /// 可读文件的本地路径（挂载前的 hydration 契约）：upper 文件 → 真实
    /// 路径；manifest 文件 → hydration 缓存路径（未缓存则灌入）。
    /// whiteout/not-found → `None`；目录报错。
    pub fn source_path(&self, rel: &str) -> Result<Option<PathBuf>> {
        validate_rel_or_root(rel)?;
        if !rel.is_empty() && is_temp_name(basename(rel)) {
            return Ok(None);
        }
        match self.resolve(rel)? {
            CasResolution::Upper { path } => Ok(Some(path)),
            CasResolution::CasFile => {
                // 防御：resolve 已确认清单命中。
                let f = self.manifest.file(rel).expect("resolve confirmed manifest hit");
                let path = self.cache.ensure(&self.cat, &f.oid, f.size)?;
                Ok(Some(path))
            }
            CasResolution::Directory { .. } => Err(Error::Cas(format!("{rel:?} is a directory"))),
            _ => Ok(None),
        }
    }

    /// 读文件内容（upper 命中读真实文件；manifest 命中经缓存按需 hydrate）。
    pub fn read(&self, rel: &str) -> Result<Option<Vec<u8>>> {
        match self.source_path(rel)? {
            Some(path) => Ok(Some(std::fs::read(path)?)),
            None => Ok(None),
        }
    }

    // ---- 写 ----

    /// copy-up：manifest 内容物化到 upper 以便就地修改；已在 upper 则返回
    /// 其路径（B1 `copy_up` 同契约）。刚复制的副本与 manifest 内容一致，
    /// mtime 归一到固定纪元（best-effort）——stat 在 copy-up 前后保持固定
    /// 值，index 对齐不被 copy-up 打破；真实修改会自然刷新真实 mtime。
    pub fn copy_up(&self, rel: &str) -> Result<Option<PathBuf>> {
        validate_relative_path(rel)?;
        self.invalidate_upper();
        match self.resolve(rel)? {
            CasResolution::Upper { path } => {
                self.set_file_state(rel, FileState::Upper);
                Ok(Some(path))
            }
            CasResolution::CasFile => {
                let f = self.manifest.file(rel).expect("resolve confirmed manifest hit");
                let cached = self.cache.ensure(&self.cat, &f.oid, f.size)?;
                let final_path = self.upper_abs(rel);
                if let Some(parent) = final_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(&cached, &final_path)?;
                normalize_manifest_identical_mtime(&final_path, self.config.fixed_stat_epoch_ns);
                self.set_file_state(rel, FileState::Upper);
                Ok(Some(final_path))
            }
            _ => Ok(None),
        }
    }

    /// 全量写入：内容与 manifest 一致 → 回收（零 upper、零 fsync，
    /// [`WriteState::Lower`]）；否则 temp+fsync+rename 原子落 upper。
    /// 撤销 whiteout。
    pub fn write(&mut self, rel: &str, data: &[u8]) -> Result<WriteOutcome> {
        self.finish_staged(rel, data.len() as u64, StagedSource::Memory(data))
    }

    /// 流式写入（B1 `write_from` 同语义）：内容先落 upper 目录内隐藏临时
    /// 文件，随后按 git blob sha1 回查 manifest oid——一致 → 删临时文件与
    /// 既有 upper，零 fsync 回落 manifest（copy-up 后改回原内容的回收语义
    /// 与 B1 一致）；不一致 → fsync 后原子改名落 upper（恰好一次 fsync）。
    pub fn write_from(
        &mut self,
        rel: &str,
        reader: &mut impl std::io::Read,
    ) -> Result<WriteOutcome> {
        self.validate_write_rel(rel)?;
        self.invalidate_upper();
        let final_path = self.upper_abs(rel);
        let parent = self.upper_parent(&final_path)?;
        std::fs::create_dir_all(parent)?;
        let tmp = self.temp_path(parent);
        let staged = (|| -> std::io::Result<u64> {
            let mut f = std::fs::File::create(&tmp)?;
            let n = std::io::copy(reader, &mut f)?;
            Ok(n)
        })();
        let size = match staged {
            Ok(v) => v,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e.into());
            }
        };
        let oid = match git_blob_oid_reader(size, &mut std::fs::File::open(&tmp)?) {
            Ok(v) => v,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e.into());
            }
        };
        if self.manifest.file(rel).is_some_and(|f| f.oid == oid) {
            // 与 manifest 一致：零 fsync 回收（临时文件未 sync 即删）。
            let _ = std::fs::remove_file(&tmp);
            if final_path.symlink_metadata().is_ok() {
                std::fs::remove_file(&final_path)?;
            }
            self.markers.remove(rel)?;
            self.set_file_state(rel, FileState::Lower);
            return Ok(WriteOutcome { state: WriteState::Lower, bytes: size });
        }
        if let Err(e) = self.sync_and_publish(&tmp, &final_path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        self.markers.remove(rel)?;
        self.set_file_state(rel, FileState::Upper);
        Ok(WriteOutcome {
            state: if self.manifest.file(rel).is_some() {
                WriteState::Upper
            } else {
                WriteState::New
            },
            bytes: size,
        })
    }

    /// 暂存写终裁（M9/B090100 同款）：暂存期内容（内存/溢写）按 git blob
    /// sha1 回查 manifest oid——一致 → 丢弃内容回落 manifest（零 upper、
    /// 零 fsync）；否则物化 upper（溢写先 fsync 再同卷改名 / 内存经临时
    /// 文件 fsync 后原子改名，恰好一次 fsync）。失败时溢写文件保留
    /// （调用方 best-effort 清理），upper 既有内容不受影响。
    pub fn finish_staged(
        &mut self,
        rel: &str,
        size: u64,
        source: StagedSource<'_>,
    ) -> Result<WriteOutcome> {
        self.validate_write_rel(rel)?;
        self.invalidate_upper();
        let final_path = self.upper_abs(rel);
        let oid = match &source {
            StagedSource::Memory(bytes) => {
                debug_assert_eq!(bytes.len() as u64, size, "memory staged size mismatch");
                git_blob_oid_memory(bytes)
            }
            StagedSource::Spill(path) => git_blob_oid_reader(size, &mut std::fs::File::open(path)?)?,
        };
        if self.manifest.file(rel).is_some_and(|f| f.oid == oid) {
            // 与 manifest 一致：回收（删溢写/既有 upper，零 fsync）。
            if let StagedSource::Spill(path) = &source {
                std::fs::remove_file(path)?;
            }
            if final_path.symlink_metadata().is_ok() {
                std::fs::remove_file(&final_path)?;
            }
            self.markers.remove(rel)?;
            self.set_file_state(rel, FileState::Lower);
            return Ok(WriteOutcome { state: WriteState::Lower, bytes: size });
        }
        let parent = self.upper_parent(&final_path)?;
        std::fs::create_dir_all(parent)?;
        match source {
            StagedSource::Spill(path) => {
                // Windows 的 FlushFileBuffers 要求写访问句柄，只读打开必报
                // ACCESS_DENIED，以读+写重开；fsync 后同卷改名（数据先于
                // 可见性，崩溃不留半截可见文件）。
                let f = std::fs::OpenOptions::new().read(true).write(true).open(&path)?;
                f.sync_all()?;
                drop(f);
                std::fs::rename(&path, &final_path)?;
            }
            StagedSource::Memory(data) => {
                let tmp = self.temp_path(parent);
                let staged = (|| -> std::io::Result<()> {
                    let mut f = std::fs::File::create(&tmp)?;
                    f.write_all(data)?;
                    f.sync_all()?;
                    Ok(())
                })();
                if let Err(e) = staged {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e.into());
                }
                if let Err(e) = std::fs::rename(&tmp, &final_path) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e.into());
                }
            }
        }
        self.markers.remove(rel)?;
        self.set_file_state(rel, FileState::Upper);
        Ok(WriteOutcome {
            state: if self.manifest.file(rel).is_some() {
                WriteState::Upper
            } else {
                WriteState::New
            },
            bytes: size,
        })
    }

    /// B100100：文件 create 回调返回后路径必须立即可解析——whiteout 撤销
    /// （视图立即回落 manifest；与 B1 的"空 blob 零拷贝终裁/撤销"同效）；
    /// manifest 无覆盖时物化空 upper 占位（plain create，无 fsync；持久性
    /// 由后续写终裁或 cleanup 的零写终裁保证）。其余状态已可解析，no-op。
    pub fn prepare_created_file(&mut self, rel: &str) -> Result<()> {
        validate_relative_path(rel)?;
        match self.resolve(rel)? {
            CasResolution::Whiteout => {
                self.markers.remove(rel)?;
                if self.manifest.file(rel).is_some() {
                    self.set_file_state(rel, FileState::Lower);
                } else {
                    self.remove_file_state(rel);
                }
            }
            CasResolution::NotFound => {
                let path = self.upper_abs(rel);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::File::create(&path)?;
                self.set_file_state(rel, FileState::Upper);
            }
            _ => {}
        }
        Ok(())
    }

    /// 重命名（M12 语义）：文件级 = upper 搬移/hydrate 复制 + whiteout 源 +
    /// 撤销目标标记（POSIX 覆盖语义：目标是文件则先删）。目录级对齐
    /// M12/B120100：manifest 有内容的目录默认 EXDEV（调用方按跨设备语义
    /// 退化），仅 upper-only 目录纯物理改名、零标记。
    pub fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        validate_relative_path(from)?;
        validate_relative_path(to)?;
        self.invalidate_upper();
        if from == to {
            return Ok(());
        }
        if from.starts_with(&format!("{to}/")) || to.starts_with(&format!("{from}/")) {
            return Err(Error::Cas(format!(
                "rename {from:?} -> {to:?}: cannot rename into own subtree"
            )));
        }
        let source = self.resolve(from)?;
        if matches!(source, CasResolution::NotFound | CasResolution::Whiteout) {
            return Err(Error::NotFound(from.to_string()));
        }
        if let CasResolution::Directory { has_cas, has_upper } = source {
            if has_cas || self.manifest.has_file_or_dir(from) {
                return Err(Error::CrossDevice(format!(
                    "directory rename {from:?} -> {to:?} spans overlay layers (EXDEV)"
                )));
            }
            if has_upper {
                let to_upper = self.upper_abs(to);
                if let Some(parent) = to_upper.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::rename(self.upper_abs(from), &to_upper)?;
            }
            self.markers.remove(to)?;
            return Ok(());
        }
        if matches!(self.resolve(to)?, CasResolution::Directory { .. }) {
            return Err(Error::Cas(format!(
                "rename target {to:?} is an existing directory"
            )));
        }
        let to_upper = self.upper_abs(to);
        if let Some(parent) = to_upper.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if to_upper.symlink_metadata().is_ok() {
            std::fs::remove_file(&to_upper)?;
        }
        match source {
            CasResolution::Upper { path } => {
                std::fs::rename(&path, &to_upper)?;
            }
            // lower（manifest）经改名落 upper：内容与 manifest 一致，mtime
            // 归一到固定纪元，index 对齐不因改名破坏。
            CasResolution::CasFile => {
                let f = self.manifest.file(from).expect("resolve confirmed manifest hit");
                let cached = self.cache.ensure(&self.cat, &f.oid, f.size)?;
                std::fs::copy(&cached, &to_upper)?;
                normalize_manifest_identical_mtime(&to_upper, self.config.fixed_stat_epoch_ns);
            }
            _ => unreachable!("checked above"),
        }
        if self.manifest.has_file_or_dir(from) {
            self.markers.add(from)?;
            self.set_file_state(from, FileState::Deleted);
        } else {
            self.remove_file_state(from);
        }
        self.markers.remove(to)?;
        self.set_file_state(to, FileState::Upper);
        Ok(())
    }

    /// HEAD 前进（commit 等）后重载 manifest（M18 刷新）：返回 `true` =
    /// manifest 已重载。失效 whiteout 剪枝：新 manifest 已无该路径的标记
    /// 一并移除（已提交的删除不再需要隐藏）；新 manifest 仍有该路径的
    /// 标记保留（未提交的删除继续生效）。stat 不含 oid，重载不动摇
    /// index 对齐。分支切换的完整语义（跨分支路径复用等）不在 M18。
    pub fn refresh_head(&mut self, repo: &std::path::Path) -> Result<bool> {
        let head = crate::gitx::head_commit(repo)?;
        if head == self.manifest.commit() {
            return Ok(false);
        }
        let manifest = Arc::new(Manifest::load(repo, &head)?);
        let stale: Vec<String> = self
            .markers
            .whites()
            .iter()
            .filter(|p| !manifest.has_file_or_dir(p))
            .cloned()
            .collect();
        for p in stale {
            self.markers.remove(&p)?;
        }
        self.manifest = manifest;
        let state = build_file_state(
            &self.config.upper_root,
            &self.manifest,
            self.markers.whites(),
        );
        *self.state.lock() = state;
        self.invalidate_upper();
        Ok(true)
    }

    // ---- 写路径内部 ----

    fn validate_write_rel(&self, rel: &str) -> Result<()> {
        validate_relative_path(rel)?;
        if is_temp_name(basename(rel)) {
            return Err(Error::Cas(format!(
                "path {rel:?} uses reserved temp prefix {TEMP_PREFIX:?}"
            )));
        }
        Ok(())
    }

    fn upper_parent<'a>(&self, final_path: &'a std::path::Path) -> Result<&'a std::path::Path> {
        final_path
            .parent()
            .ok_or_else(|| Error::Cas(format!("bad upper path {final_path:?}")))
    }

    fn temp_path(&self, parent: &std::path::Path) -> std::path::PathBuf {
        parent.join(format!(
            "{TEMP_PREFIX}-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// fsync 临时文件后原子改名到最终 upper 路径（恰好一次 fsync）。
    fn sync_and_publish(&self, tmp: &std::path::Path, final_path: &std::path::Path) -> Result<()> {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(tmp)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(tmp, final_path)?;
        Ok(())
    }

    /// 删除：upper 删真实文件/目录；manifest 有内容（自身或子树）则记
    /// whiteout。纯 manifest 删除零 upper 文件操作、零对象读取（不 hydrate）。
    pub fn remove(&mut self, rel: &str) -> Result<bool> {
        validate_relative_path(rel)?;
        self.invalidate_upper();
        let upper_path = self.upper_abs(rel);
        match self.resolve(rel)? {
            CasResolution::Upper { .. } => {
                std::fs::remove_file(&upper_path)?;
                if self.manifest.file(rel).is_some() {
                    self.markers.add(rel)?;
                    self.set_file_state(rel, FileState::Deleted);
                } else {
                    self.remove_file_state(rel);
                }
                Ok(true)
            }
            CasResolution::Directory { has_cas, has_upper } => {
                if has_upper {
                    std::fs::remove_dir_all(&upper_path)?;
                }
                self.remove_subtree_state(rel);
                if has_cas || self.manifest.has_file_or_dir(rel) {
                    self.markers.add(rel)?;
                }
                Ok(true)
            }
            CasResolution::CasFile => {
                self.markers.add(rel)?;
                self.set_file_state(rel, FileState::Deleted);
                Ok(true)
            }
            CasResolution::Whiteout | CasResolution::NotFound => Ok(false),
        }
    }
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

/// stat/list 接受 `""`（视图根），其余走完整校验。
fn validate_rel_or_root(rel: &str) -> Result<()> {
    if rel.is_empty() {
        return Ok(());
    }
    validate_relative_path(rel)
}

/// 把与 manifest 内容一致的 upper 副本 mtime 归一到固定纪元（best-effort：
/// 失败只影响 git 一次额外 rehash，正确性不受影响）。与 wtfs-core 的
/// `normalize_template_identical_mtime` 同语义。
fn normalize_manifest_identical_mtime(path: &std::path::Path, epoch_ns: i64) {
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|f| {
            f.set_modified(
                std::time::UNIX_EPOCH
                    + std::time::Duration::from_nanos(epoch_ns.max(0) as u64),
            )
        });
}

/// git blob 的对象 id：sha1(`blob <len>\0` + 内容)——写路径 dedupe 与
/// manifest oid 的对齐基准（M17 的 manifest oid 来自 `ls-tree`，同一算法）。
fn git_blob_oid_memory(bytes: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(format!("blob {}\0", bytes.len()).into_bytes());
    h.update(bytes);
    hex_lower(&h.finalize())
}

/// 流式形态（长度已知：暂存文件/溢写文件）；一遍顺序读。
fn git_blob_oid_reader(size: u64, read: &mut impl std::io::Read) -> std::io::Result<String> {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(format!("blob {size}\0").into_bytes());
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = read.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex_lower(&h.finalize()))
}

fn hex_lower(digest: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// 文件系统真实 mtime → Unix 纳秒（早于 Unix 纪元截为 0；与 wtfs-core 一致）。
fn metadata_mtime_ns(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as i64)
}
