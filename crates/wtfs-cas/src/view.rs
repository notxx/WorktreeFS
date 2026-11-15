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
//! * **write**（MVP）：恒落 upper（temp+fsync+rename 原子），撤销路径的
//!   whiteout；不做写路径内容 dedupe（B1 的 ToLower 路径是它的差异化
//!   设计，C 的 M18 形态再议——届时可对写入流式 blake3 后回查 manifest）。
//!   返回 [`WriteState::Upper`]（manifest 有路径）/ [`WriteState::New`]。
//! * **remove**：upper 删真实文件/目录；manifest 有内容（自身或子树）则
//!   记 whiteout（[`MarkerLog`]，M12 日志语义）。纯 manifest 删除零
//!   upper 文件操作——"删除不 hydrate"。
//! * rename 不在 MVP（目录级 B1 也默认 EXDEV；文件级由 M18 的 VFS 层按
//!   copy+whiteout 组合即可，无需 core 支持）。
//!
//! upper 负存在缓存（M14 优化）未移植：MVP 每次解析一次真实探测
//! （`symlink_metadata`）；M14 的收益主要在 B1 的逐回调探测序列，C 的
//! 基准口径（stat 4,874 文件）不受影响，M18 按需移植。

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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

/// 解析结果（内部）。
#[derive(Debug, Clone)]
enum Resolution {
    NotFound,
    /// 被 whiteout 隐藏且 upper/lower 均无文件形态（upper 目录重建除外）。
    Whiteout,
    /// upper 命中（文件）。
    Upper {
        path: PathBuf,
    },
    /// manifest（lower）命中。
    CasFile {
        file: crate::manifest::ManifestFile,
    },
    /// 目录：manifest/upper 两侧存在性（合并枚举用）。
    Directory {
        has_cas: bool,
        has_upper: bool,
    },
}

/// [`CasView::list`] 的条目；`rel` 是相对视图根的完整路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CasDirEntry {
    pub name: String,
    pub rel: String,
    pub kind: EntryKind,
}

/// [`CasView::write`] 的返回：MVP 恒落 upper，state 只表达 manifest 覆盖
/// （Upper = 覆盖既有 manifest 路径，New = 全新路径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteState {
    Upper,
    New,
}

/// [`CasView::write`] 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteOutcome {
    pub state: WriteState,
    pub bytes: u64,
}

/// 一个 worktree 的 CAS 视图。读方法 `&self`，写方法 `&mut self`
/// （whiteout 镜像与日志追加的单写者约束；跨线程共享由调用方加锁）。
pub struct CasView {
    config: CasViewConfig,
    manifest: Arc<Manifest>,
    cat: Arc<CatFile>,
    cache: Arc<HydrationCache>,
    markers: MarkerLog,
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
        Ok(Self {
            config,
            manifest,
            cat,
            cache,
            markers,
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
    pub fn upper_abs(&self, rel: &str) -> PathBuf {
        if rel.is_empty() {
            self.config.upper_root.clone()
        } else {
            self.config.upper_root.join(rel)
        }
    }

    /// 稳定 file id（Windows FILE_ID 与 index ino 用）：与 wtfs-core 同一
    /// blake3(小写路径) 派生；空路径报 InvalidPath，根目录请用常量。
    pub fn file_id(&self, rel: &str) -> Result<u64> {
        validate_relative_path(rel)?;
        Ok(file_id(rel))
    }

    // ---- 解析 ----

    fn resolve(&self, rel: &str) -> Result<Resolution> {
        let upper_path = self.upper_abs(rel);
        let upper_md = std::fs::symlink_metadata(&upper_path).ok();
        let upper_is_file = upper_md.as_ref().is_some_and(|md| !md.is_dir());
        let upper_is_dir = upper_md.as_ref().is_some_and(|md| md.is_dir());
        if upper_is_file {
            return Ok(Resolution::Upper { path: upper_path });
        }
        if !rel.is_empty() && self.markers.blocks(rel) {
            // upper 文件已查无；upper 目录可能已重建（opaque 语义：只显示
            // upper 内容，manifest 子树继续隐藏）。
            return Ok(if upper_is_dir {
                Resolution::Directory {
                    has_cas: false,
                    has_upper: true,
                }
            } else {
                Resolution::Whiteout
            });
        }
        if let Some(file) = self.manifest.file(rel) {
            return Ok(Resolution::CasFile { file: file.clone() });
        }
        let has_cas = rel.is_empty() || self.manifest.has_dir(rel);
        if has_cas || upper_is_dir {
            return Ok(Resolution::Directory {
                has_cas,
                has_upper: upper_is_dir,
            });
        }
        Ok(Resolution::NotFound)
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
        match self.resolve(rel)? {
            Resolution::Whiteout | Resolution::NotFound => Ok(None),
            Resolution::Upper { path } => {
                let md = std::fs::symlink_metadata(&path)?;
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
            Resolution::CasFile { file } => Ok(Some(CasStat {
                dev: vs,
                ino: file_id(rel),
                mode: file.mode,
                size: file.size,
                mtime_ns: epoch,
                ctime_ns: epoch,
            })),
            Resolution::Directory { has_upper, .. } => {
                let mtime = if has_upper {
                    metadata_mtime_ns(&std::fs::symlink_metadata(self.upper_abs(rel))?)
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
            Resolution::Upper { path } => Ok(Some(path)),
            Resolution::CasFile { file } => {
                let path = self.cache.ensure(&self.cat, &file.oid, file.size)?;
                Ok(Some(path))
            }
            Resolution::Directory { .. } => Err(Error::Cas(format!("{rel:?} is a directory"))),
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

    /// 全量写入（MVP 恒落 upper：temp+fsync+rename 原子；撤销 whiteout）。
    /// 与 B1 的 `write_from` 同一落盘纪律（恰好一次 fsync）。
    pub fn write(&mut self, rel: &str, data: &[u8]) -> Result<WriteOutcome> {
        validate_relative_path(rel)?;
        if is_temp_name(basename(rel)) {
            return Err(Error::Cas(format!(
                "path {rel:?} uses reserved temp prefix {TEMP_PREFIX:?}"
            )));
        }
        let final_path = self.upper_abs(rel);
        let parent = final_path
            .parent()
            .ok_or_else(|| Error::Cas(format!("bad upper path for {rel:?}")))?;
        std::fs::create_dir_all(parent)?;
        let tmp = parent.join(format!(
            "{TEMP_PREFIX}-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
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
        self.markers.remove(rel)?;
        Ok(WriteOutcome {
            state: if self.manifest.file(rel).is_some() {
                WriteState::Upper
            } else {
                WriteState::New
            },
            bytes: data.len() as u64,
        })
    }

    /// 删除：upper 删真实文件/目录；manifest 有内容（自身或子树）则记
    /// whiteout。纯 manifest 删除零 upper 文件操作、零对象读取（不 hydrate）。
    pub fn remove(&mut self, rel: &str) -> Result<bool> {
        validate_relative_path(rel)?;
        let upper_path = self.upper_abs(rel);
        match self.resolve(rel)? {
            Resolution::Upper { .. } => {
                std::fs::remove_file(&upper_path)?;
                if self.manifest.has_file_or_dir(rel) {
                    self.markers.add(rel)?;
                }
                Ok(true)
            }
            Resolution::Directory { has_cas, has_upper } => {
                if has_upper {
                    std::fs::remove_dir_all(&upper_path)?;
                }
                if has_cas || self.manifest.has_file_or_dir(rel) {
                    self.markers.add(rel)?;
                }
                Ok(true)
            }
            Resolution::CasFile { .. } => {
                self.markers.add(rel)?;
                Ok(true)
            }
            Resolution::Whiteout | Resolution::NotFound => Ok(false),
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

/// 文件系统真实 mtime → Unix 纳秒（早于 Unix 纪元截为 0；与 wtfs-core 一致）。
fn metadata_mtime_ns(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as i64)
}
