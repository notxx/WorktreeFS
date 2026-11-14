//! 单个 worktree 的 overlay 视图引擎（spec §6.5/§7：[`OverlayStore`] 之上的语义层）。
//!
//! * 解析顺序：upper > whiteout > lower；目录 = lower ∪ upper 再剔除 whiteout。
//! * 目录级重命名走 rename map（lower 侧路径改写），文件级 = whiteout + upper 搬移。
//! * 写入路径 dedupe：流式写临时文件并同步 blake3，与模板一致则丢弃副本回落
//!   lower，不同则落 upper；模板缺失时全部 upper（全量 copy 兜底），绝不丢数据。
//! * stat 纯推导（M11/B110100，OverlayFS 三态口径）：lower 文件固定
//!   dev/ino/mtime/ctime（size/mode 来自模板，mtime = per-worktree epoch）；
//!   upper 文件/目录 = 真实文件系统 mtime；ino = 路径稳定哈希（根目录固定值）；
//!   whiteout/not-found → None。**零写副作用、零 DB 读**：whiteouts/renames
//!   打开时一次性缓存进内存（写路径同步镜像 DB，M12 前表保留），模板清单在
//!   打开时构建成目录索引（目录集合 + 每目录子项），status/list 热路径不再
//!   逐 entry 查库。
//! * M8/B080100 批量元数据：`initializing` 阶段（对 readdir 隐藏的安全批量窗口）
//!   把 whiteouts / renames 累积在内存（[`InitBatch`]），**每文件零事务、零 DB
//!   读**；[`Overlay::mark_ready`] 在单个 SQLite 事务内整批落库并切 ready
//!   （崩溃安全：提交前崩溃保持 initializing、upper 文件完整，缓冲丢失可接受，
//!   spec §6.3.6）。ready 后的编辑回到逐操作持久化（spec 允许）。initializing
//!   标志缓存在内存，per-file 操作不再查询 `worktrees.status`。
//! * M9/B090100 暂存写零拷贝终裁：[`Overlay::finish_staged`] 接收暂存期已
//!   流式算好的哈希/长度/内容（内存或溢写路径），一致 → 丢弃回落 lower
//!   （零 upper 文件、零 fsync、零临时副本）；否则溢写同卷改名 / 内存经
//!   TEMP_PREFIX 临时文件原子改名，恰好一次 fsync。[`Overlay::write_from`]
//!   保留给 ready 就地与通用写；[`OverlayIoStats`] 插桩 upper 物化与 fsync
//!   次数（测试/诊断）。
//! * M11：`dedupe` / `file_ids` / `stat_cache` 三张派生表删除（schema v3）。
//!   写入决策仍然当场计算（决定内容回落 lower 还是留 upper），只是不再落库
//!   ——upper 物理文件存在即 Upper、缺失即 Lower，状态天然可推导；copy-up /
//!   lower 经 rename 落 upper 的内容与模板一致，mtime 归一到 per-worktree
//!   epoch，index 对齐不因 copy-up 破坏。
//!
//! 约定：upper 根下以 `.wtfs-tmp` 开头的名字是写入临时文件，对视图不可见；
//! 写入请求若使用该前缀名会被拒绝。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::{Mutex, MutexGuard};

use crate::overlay_batch::InitBatch;
use crate::overlay_store::{SqliteOverlayStore, derive_ino};
use crate::{
    DedupeState, Error, FileAttributes, FileHash, FileHashMap, OverlayConfig, OverlayStore,
    Result, Stat, TemplateFile, WorktreeId, validate_relative_path,
};

/// upper 内写入临时文件的名字前缀（对视图隐藏，崩溃残留由 gc 清理）。
pub const TEMP_PREFIX: &str = ".wtfs-tmp";

/// 暂存写终裁的内容来源（[`Overlay::finish_staged`]）。
#[derive(Debug)]
pub enum StagedContent<'a> {
    /// 完整内容已在内存。
    Memory(&'a [u8]),
    /// 溢写文件（upper 目录内 `.wtfs-tmp-w-*`，与最终 upper 路径同卷）。
    /// 调用方必须已关闭句柄（Windows 不允许 rename 打开中的文件）；
    /// core 负责 fsync 与 rename/删除消费。
    Spill(&'a Path),
}

/// overlay I/O 计数器（M9/B090100 测试插桩/诊断）：upper 物化与 fsync
/// （`sync_all`）累计次数。按 overlay 实例隔离，并行测试可各自断言；
/// 非稳定性承诺，不作为公共 API 维护。
#[derive(Debug, Default)]
pub struct OverlayIoStats {
    upper_writes: AtomicU64,
    fsyncs: AtomicU64,
}

impl OverlayIoStats {
    /// upper 侧文件物化次数（`finish_staged`/`write_from` 落 upper、
    /// `copy_up`、VFS 新建 upper 写句柄）。
    pub fn upper_writes(&self) -> u64 {
        self.upper_writes.load(Ordering::Relaxed)
    }

    /// `File::sync_all` 调用次数（M9 起 = 物化 upper 的文件数，干净 checkout
    /// 为 0）。
    pub fn fsyncs(&self) -> u64 {
        self.fsyncs.load(Ordering::Relaxed)
    }

    /// 清零（测试分段断言用）。
    pub fn reset(&self) {
        self.upper_writes.store(0, Ordering::Relaxed);
        self.fsyncs.store(0, Ordering::Relaxed);
    }

    /// 记一次 upper 物化（core 内部写路径 + VFS 新建 upper 写句柄共用）。
    pub fn record_upper_write(&self) {
        self.upper_writes.fetch_add(1, Ordering::Relaxed);
    }

    fn record_fsync(&self) {
        self.fsyncs.fetch_add(1, Ordering::Relaxed);
    }
}

/// overlay 根目录的固定 ino（路径哈希前的特例值）。
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

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// lower（模板清单）的内存索引：文件表 + 目录集合 + 每目录直接子项。
/// Overlay 打开时一次构建（O(模板文件数)）；解析/枚举/stat 全部内存查询——
/// 旧的"list 全表前缀过滤"与"has_lower_dir 逐 key starts_with"分别被
/// O(子项数) 与 O(1) 取代。
#[derive(Debug)]
struct LowerIndex {
    files: FileHashMap,
    /// 全部目录路径（不含根 `""`）；`a/b.txt` 注册 `a`。
    dirs: HashSet<String>,
    /// 目录 → 直接子项（name → kind）；根目录键 `""`。
    children: HashMap<String, BTreeMap<String, EntryKind>>,
}

impl LowerIndex {
    fn build(files: FileHashMap) -> Self {
        let mut dirs = HashSet::new();
        let mut children: HashMap<String, BTreeMap<String, EntryKind>> = HashMap::new();
        for path in files.keys() {
            let comps: Vec<&str> = path.split('/').collect();
            let mut parent = String::new();
            for (i, comp) in comps.iter().enumerate() {
                let is_last = i + 1 == comps.len();
                children
                    .entry(parent.clone())
                    .or_default()
                    .insert(
                        (*comp).to_string(),
                        if is_last { EntryKind::File } else { EntryKind::Directory },
                    );
                if !is_last {
                    parent = if parent.is_empty() {
                        (*comp).to_string()
                    } else {
                        format!("{parent}/{comp}")
                    };
                    dirs.insert(parent.clone());
                }
            }
        }
        Self { files, dirs, children }
    }

    fn file(&self, mapped: &str) -> Option<&TemplateFile> {
        self.files.get(mapped)
    }

    fn has_dir(&self, mapped: &str) -> bool {
        self.dirs.contains(mapped)
    }

    fn has_file_or_dir(&self, mapped: &str) -> bool {
        self.files.contains_key(mapped) || self.dirs.contains(mapped)
    }

    fn children_of(&self, mapped: &str) -> Option<&BTreeMap<String, EntryKind>> {
        self.children.get(mapped)
    }
}

/// overlay 视图：config + lower 模板索引 + 元数据存储 + whiteout/rename 内存缓存。
pub struct Overlay {
    config: OverlayConfig,
    /// lower 模板索引；`None` = 无模板（全量 copy 模式）。
    lower: Option<LowerIndex>,
    store: Arc<SqliteOverlayStore>,
    /// initializing 标志 + whiteout/rename 内存缓存（同锁）：标志缓存自
    /// worktrees.status；initializing 期 whiteouts/renames 累积在 batch，
    /// ready 后读缓存、写路径同步镜像 DB。per-file 操作不再查库。
    init_meta: Mutex<InitMeta>,
    /// I/O 插桩计数（upper 物化 / fsync；测试与诊断用）。
    io: OverlayIoStats,
}

/// [`Overlay::init_meta` 的守卫状态]：initializing 标志 + whiteout/rename 视图。
///
/// * initializing：权威视图在 `batch`（mark_ready 整批落库）。
/// * ready：`whites`/`renames` 为打开时从 DB 一次性装载的缓存；写路径
///   先更新缓存再镜像 DB（缓存与表保持一致）。
#[derive(Debug, Default)]
struct InitMeta {
    initializing: bool,
    /// initializing 期的权威元数据视图；ready/failed 后为 `None`。
    batch: Option<InitBatch>,
    /// whiteout 集合（lower 命名空间路径）。
    whites: HashSet<String>,
    /// 目录重命名映射 (from → to)，保持插入序（与 store 读取序一致）。
    renames: Vec<(String, String)>,
}

/// 元数据操作的上下文：initializing = 持锁内存缓冲（零 DB）；
/// ready = 持锁缓存 + 直连 store 镜像（逐操作持久化）。由
/// [`Overlay::meta_ctx`] 在锁内判定，不存在"初始化中但缓冲缺席"的中间态。
enum MetaCtx<'a> {
    Init {
        guard: MutexGuard<'a, InitMeta>,
    },
    Ready {
        guard: MutexGuard<'a, InitMeta>,
        id: WorktreeId,
        store: &'a SqliteOverlayStore,
    },
}

impl<'a> MetaCtx<'a> {
    fn batch(&self) -> &InitBatch {
        match self {
            MetaCtx::Init { guard, .. } => guard
                .batch
                .as_ref()
                .expect("Init meta context without batch"),
            MetaCtx::Ready { .. } => unreachable!("batch access in Ready context"),
        }
    }

    fn batch_mut(&mut self) -> &mut InitBatch {
        match self {
            MetaCtx::Init { guard, .. } => guard
                .batch
                .as_mut()
                .expect("Init meta context without batch"),
            MetaCtx::Ready { .. } => unreachable!("batch access in Ready context"),
        }
    }

    /// 全部目录重命名映射 (from → to)；内存视图，零 DB。
    fn renames(&self) -> &[(String, String)] {
        match self {
            MetaCtx::Init { .. } => self.batch().renames(),
            MetaCtx::Ready { guard, .. } => &guard.renames,
        }
    }

    /// 全部 whiteout（lower 命名空间路径）；内存视图，零 DB。
    fn whites(&self) -> &HashSet<String> {
        match self {
            MetaCtx::Init { .. } => self.batch().whiteouts(),
            MetaCtx::Ready { guard, .. } => &guard.whites,
        }
    }

    fn add_whiteout(&mut self, path: &str) -> Result<()> {
        validate_relative_path(path)?;
        match self {
            MetaCtx::Init { .. } => {
                self.batch_mut().add_whiteout(path);
                Ok(())
            }
            MetaCtx::Ready { guard, id, store } => {
                // 缓存未命中 = DB 也没有（缓存是打开时装载 + 写路径镜像）：
                // 跳过事务（M8 无用功短路的延伸）。
                if guard.whites.insert(path.to_string()) {
                    store.add_whiteout(*id, path)?;
                }
                Ok(())
            }
        }
    }

    /// 撤销 whiteout；缓存无条目时零事务（M8：per-file 无用功短路）。
    fn remove_whiteout(&mut self, path: &str) -> Result<()> {
        validate_relative_path(path)?;
        match self {
            MetaCtx::Init { .. } => {
                self.batch_mut().remove_whiteout(path);
                Ok(())
            }
            MetaCtx::Ready { guard, id, store } => {
                if guard.whites.remove(path) {
                    store.remove_whiteout(*id, path)?;
                }
                Ok(())
            }
        }
    }

    fn record_rename(&mut self, from: &str, to: &str) -> Result<()> {
        validate_relative_path(from)?;
        validate_relative_path(to)?;
        if from == to {
            return Err(Error::Overlay(format!(
                "rename source and target are identical: {from}"
            )));
        }
        match self {
            MetaCtx::Init { .. } => {
                self.batch_mut().record_rename(from, to);
                Ok(())
            }
            MetaCtx::Ready { guard, id, store } => {
                match guard.renames.iter_mut().find(|(f, _)| f == from) {
                    Some(pair) => pair.1 = to.to_string(),
                    None => guard.renames.push((from.to_string(), to.to_string())),
                }
                store.record_rename(*id, from, to)
            }
        }
    }
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
            lower: lower_files.map(LowerIndex::build),
            store,
            init_meta: Mutex::new(InitMeta {
                initializing: true,
                batch: Some(InitBatch::empty()),
                whites: HashSet::new(),
                renames: Vec::new(),
            }),
            config,
            io: OverlayIoStats::default(),
        })
    }

    /// 重新挂到已注册的 worktree（崩溃恢复/重开视图）；未注册报 [`Error::NotFound`]。
    /// initializing 行（崩溃于 ready 前）：一次性从 DB 装载既有 whiteouts/
    /// renames 为缓冲，之后继续零事务累积，mark_ready 时整批落库。
    /// ready 行：whiteouts/renames 一次性装载进内存缓存（此后读路径零 DB）。
    pub fn reopen(
        config: OverlayConfig,
        lower_files: Option<FileHashMap>,
        store: Arc<SqliteOverlayStore>,
    ) -> Result<Self> {
        let state = store
            .state(config.worktree_id)?
            .ok_or_else(|| Error::NotFound(format!("worktree {} not registered", config.worktree_id)))?;
        let initializing = state.hides_entries();
        let (batch, whites, renames) = if initializing {
            let batch = InitBatch::load(&store, config.worktree_id)?;
            let whites = batch.whiteouts().clone();
            let renames = batch.renames().to_vec();
            (Some(batch), whites, renames)
        } else {
            (None, store.whiteouts(config.worktree_id)?, store.renames(config.worktree_id)?)
        };
        Ok(Self {
            lower: lower_files.map(LowerIndex::build),
            store,
            init_meta: Mutex::new(InitMeta { initializing, batch, whites, renames }),
            config,
            io: OverlayIoStats::default(),
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
        self.lower.as_ref().map(|idx| &idx.files)
    }

    pub fn worktree_id(&self) -> WorktreeId {
        self.config.worktree_id
    }

    /// I/O 插桩计数（upper 物化 / fsync；测试与诊断用，非稳定性承诺）。
    pub fn io_stats(&self) -> &OverlayIoStats {
        &self.io
    }

    // ---- 状态机 ----

    /// initializing 标志（内存缓存；per-file 调用不再查询 `worktrees.status`）。
    pub fn is_initializing(&self) -> Result<bool> {
        Ok(self.init_meta.lock().initializing)
    }

    /// `initializing → ready`：缓冲元数据整批 + 状态切换在**同一事务**落库
    /// （崩溃安全：提交前崩溃 = 保持 initializing、upper 文件完整，缓冲丢失
    /// 可接受）。持锁执行落库——落库期间并发操作短暂阻塞，切 ready 后一次性
    /// 看到完整视图。无缓冲（已 ready 的重开/幂等调用）时仅切状态，
    /// 不重写任何既有行。
    pub fn mark_ready(&self) -> Result<()> {
        let mut guard = self.init_meta.lock();
        match guard.batch.take() {
            Some(batch) => match self.store.flush_init_batch(
                self.config.worktree_id,
                &batch,
                crate::WorktreeState::Ready,
            ) {
                Ok(()) => {
                    // 批量内容（whiteouts/renames）平移为 ready 缓存，
                    // 之后读路径零 DB、写路径镜像 DB。
                    guard.whites = batch.whiteouts().clone();
                    guard.renames = batch.renames().to_vec();
                    guard.initializing = false;
                    Ok(())
                }
                Err(e) => {
                    // 落库失败：恢复缓冲，保持 initializing，可重试。
                    guard.batch = Some(batch);
                    Err(e)
                }
            },
            None => {
                self.store
                    .set_state(self.config.worktree_id, crate::WorktreeState::Ready)?;
                guard.initializing = false;
                Ok(())
            }
        }
    }

    /// 初始化失败标记：丢弃未落库缓冲（upper 文件是数据真相，保留），
    /// 状态落库为 failed——不允许 worker 在半初始化的 worktree 上启动
    /// （spec §6.3.6）。
    pub fn mark_failed(&self) -> Result<()> {
        let mut guard = self.init_meta.lock();
        self.store
            .set_state(self.config.worktree_id, crate::WorktreeState::Failed)?;
        guard.batch = None;
        guard.initializing = false;
        Ok(())
    }

    /// 元数据操作上下文：initializing → 持锁缓冲；ready → 持锁缓存 + store 镜像。
    fn meta_ctx(&self) -> MetaCtx<'_> {
        let guard = self.init_meta.lock();
        if guard.initializing {
            MetaCtx::Init { guard }
        } else {
            MetaCtx::Ready {
                guard,
                id: self.config.worktree_id,
                store: &self.store,
            }
        }
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

    /// lower 命名空间改写；`MovedAway` 时写入场景退回原路径（upper 物理位置）。
    /// `ctx` 由调用方持有（勿经 [`Overlay::meta_ctx`] 重复加锁）。
    fn mapped_or_same(&self, ctx: &MetaCtx<'_>, rel: &str) -> Result<String> {
        match map_to_lower_with(ctx.renames(), rel) {
            LowerLookup::Same(s) | LowerLookup::Mapped(s) => Ok(s),
            LowerLookup::MovedAway => Ok(rel.to_string()),
        }
    }

    fn lookup_lower(&self, mapped: &str) -> Option<TemplateFile> {
        self.lower.as_ref().and_then(|m| m.file(mapped)).cloned()
    }

    fn has_lower_dir(&self, mapped: &str) -> bool {
        self.lower.as_ref().is_some_and(|m| m.has_dir(mapped))
    }

    fn lower_has_file_or_dir(&self, mapped: &str) -> bool {
        self.lower.as_ref().is_some_and(|m| m.has_file_or_dir(mapped))
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

    /// 解析顺序：upper > whiteout > lower（spec §6.5）。`ctx` 由调用方持有，
    /// 同一逻辑操作的元数据视图一致（initializing 期 = 内存缓冲，零 DB 读；
    /// ready = 内存缓存，同样零 DB 读）。
    fn resolve_with(&self, ctx: &MetaCtx<'_>, rel: &str) -> Result<Resolution> {
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
        let mapped = match map_to_lower_with(ctx.renames(), rel) {
            LowerLookup::Same(s) | LowerLookup::Mapped(s) => s,
            LowerLookup::MovedAway => return Ok(Resolution::NotFound),
        };
        let whites = ctx.whites();
        let blocked = whiteout_blocks(&mapped, whites);
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

    /// 解析顺序：upper > whiteout > lower（spec §6.5）。
    pub fn resolve(&self, rel: &str) -> Result<Resolution> {
        let ctx = self.meta_ctx();
        self.resolve_with(&ctx, rel)
    }

    /// 目录合并枚举；`initializing` 状态返回空（让 Git 通过"目录必须为空"检查），
    /// 只读内存标志 + 内存 lower 索引 + 真实 upper 目录，零 DB。
    pub fn list(&self, dir_rel: &str) -> Result<Vec<OverlayDirEntry>> {
        self.validate_rel_or_root(dir_rel)?;
        if self.is_initializing()? {
            return Ok(Vec::new());
        }
        let ctx = self.meta_ctx();
        let whites = ctx.whites();
        let renames = ctx.renames();
        let mut names: BTreeMap<String, EntryKind> = BTreeMap::new();

        // lower 侧：内存索引按目录取直接子项，逐名应用 whiteout。
        match map_to_lower_with(renames, dir_rel) {
            LowerLookup::MovedAway => {}
            LowerLookup::Same(m) | LowerLookup::Mapped(m) => {
                if !whiteout_blocks(&m, whites) {
                    if let Some(idx) = self.lower.as_ref() {
                        if let Some(entries) = idx.children_of(&m) {
                            for (name, kind) in entries {
                                let mapped_name = if m.is_empty() {
                                    name.clone()
                                } else {
                                    format!("{m}/{name}")
                                };
                                if whiteout_blocks(&mapped_name, whites) {
                                    continue;
                                }
                                names.insert(name.clone(), *kind);
                            }
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
    /// initializing 期元数据全部进内存缓冲（零事务）；ready 后逐操作持久化。
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
            self.io.record_fsync();
            Ok((FileHash(*hasher.finalize().as_bytes()), size))
        })();
        let (hash, size) = match staged {
            Ok(v) => v,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e.into());
            }
        };

        let mut ctx = self.meta_ctx();
        let mapped = self.mapped_or_same(&ctx, rel)?;
        let template_entry = self.lookup_lower(&mapped);
        if self.config.dedupe_enabled {
            if let Some(t) = &template_entry {
                if t.hash == hash {
                    if !ancestor_whiteout_blocks(&mapped, ctx.whites()) {
                        // 与模板一致且 lower 视图可见（路径自身的 whiteout 会被
                        // remove_whiteout 撤销）：丢弃副本回落 lower。
                        // copy-up 后改回模板内容同样回收。
                        let _ = std::fs::remove_file(&tmp);
                        if final_path.symlink_metadata().is_ok() {
                            std::fs::remove_file(&final_path)?;
                        }
                        // 缓存无条目时零事务（M8：per-file 无用功短路）。
                        ctx.remove_whiteout(&mapped)?;
                        return Ok((DedupeState::Lower, size));
                    }
                    // 祖先目录被 whiteout：回落会让刚写入的文件不可见——
                    // 保留内容继续走 Upper 路径（仅错过一次 dedupe 机会）。
                }
            }
        }
        // 落 upper（模板缺失 → 全量 copy 兜底，一样正确）；mtime = 真实文件
        // mtime（rename 保留写入时刻），无需发放/落库。
        std::fs::rename(&tmp, &final_path)?;
        self.io.record_upper_write();
        let state = if template_entry.is_some() {
            DedupeState::Upper
        } else {
            DedupeState::New
        };
        ctx.remove_whiteout(&mapped)?;
        Ok((state, size))
    }

    /// M9/B090100：暂存写终裁（git checkout 流的 cleanup/flush 路径）。
    ///
    /// VFS 暂存期已流式算好 blake3（`hash`）与字节数（`size`）；本方法直接
    /// 据此终裁，不再把内容重写进临时副本：
    ///
    /// * `hash` 与模板一致且无祖先目录 whiteout → 丢弃内容、记
    ///   [`DedupeState::Lower`]、删除溢写文件：**零 upper 文件、零 fsync、
    ///   零临时副本**（干净 checkout 的目标路径；copy-up 后改回模板内容的
    ///   回收语义与 `write_from` 一致，既有 upper 一并删除）。
    /// * 否则物化 upper：[`StagedContent::Spill`] 在 upper 目录内同卷改名到
    ///   最终 upper 路径，[`StagedContent::Memory`] 写 upper 内 TEMP_PREFIX
    ///   隐藏临时文件后原子改名（P2-1：两条路径的最终 upper 路径都零半截
    ///   文件），随后**恰好一次 fsync**（溢写先 fsync 再 rename：数据先于
    ///   可见性落盘，崩溃不留半截可见文件）。记 `Upper`/`New` 决策、撤销
    ///   whiteout（initializing 期进 InitBatch 内存缓冲，ready 后镜像落库）；
    ///   mtime = 真实文件 mtime。
    ///
    /// `Spill` 的句柄必须已由调用方关闭。返回 (决策, 字节数)；失败时溢写
    /// 文件保留（调用方 best-effort 清理），upper 既有内容不受影响。
    pub fn finish_staged(
        &self,
        rel: &str,
        hash: FileHash,
        size: u64,
        content: StagedContent<'_>,
    ) -> Result<(DedupeState, u64)> {
        self.validate_rel(rel)?;
        if is_temp_name(basename(rel)) {
            return Err(Error::Overlay(format!(
                "path {rel:?} uses reserved temp prefix {TEMP_PREFIX:?}"
            )));
        }
        let final_path = self.upper_abs(rel);
        let mut ctx = self.meta_ctx();
        let mapped = self.mapped_or_same(&ctx, rel)?;
        let template_entry = self.lookup_lower(&mapped);
        if self.config.dedupe_enabled && template_entry.as_ref().is_some_and(|t| t.hash == hash) {
            if !ancestor_whiteout_blocks(&mapped, ctx.whites()) {
                // 与模板一致且 lower 视图可见（路径自身的 whiteout 会被
                // remove_whiteout 撤销）：丢弃暂存内容回落 lower。
                if let StagedContent::Spill(spill) = &content {
                    std::fs::remove_file(spill)?;
                }
                if final_path.symlink_metadata().is_ok() {
                    std::fs::remove_file(&final_path)?;
                }
                // 缓存无条目时零事务（M8：per-file 无用功短路）。
                ctx.remove_whiteout(&mapped)?;
                return Ok((DedupeState::Lower, size));
            }
            // 祖先目录被 whiteout：回落会让刚写入的文件不可见——保留内容
            // 继续走 Upper 路径（仅错过一次 dedupe 机会）。
        }
        let parent = final_path
            .parent()
            .ok_or_else(|| Error::Overlay(format!("bad upper path for {rel:?}")))?;
        std::fs::create_dir_all(parent)?;
        match content {
            StagedContent::Spill(spill) => {
                // 先 fsync 溢写内容再同卷 rename：恰好一次 fsync，且数据先于
                // 文件出现在视图中（崩溃不留半截可见文件）。Windows 的
                // FlushFileBuffers 要求写访问句柄，只读打开必报
                // ERROR_ACCESS_DENIED，因此以读+写重开。
                let f = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&spill)?;
                f.sync_all()?;
                drop(f);
                self.io.record_fsync();
                std::fs::rename(&spill, &final_path)?;
            }
            StagedContent::Memory(data) => {
                debug_assert_eq!(data.len() as u64, size, "memory staged size mismatch");
                // P2-1：内存物化同样经 TEMP_PREFIX 隐藏临时文件 → fsync →
                // 原子 rename——最终 upper 路径零半截文件（与 write_from 同
                // 语义，代价同为恰好一次 fsync，只多一次 rename）；任何中途
                // 失败只残留对视图隐藏的临时文件（gc 回收）。
                let tmp = parent.join(format!(
                    "{TEMP_PREFIX}-{}-{}",
                    std::process::id(),
                    TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
                ));
                let staged_io = (|| -> std::io::Result<()> {
                    let mut file = std::fs::File::create(&tmp)?;
                    file.write_all(data)?;
                    file.sync_all()?;
                    Ok(())
                })();
                if let Err(e) = staged_io {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e.into());
                }
                self.io.record_fsync();
                if let Err(e) = std::fs::rename(&tmp, &final_path) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e.into());
                }
            }
        }
        self.io.record_upper_write();
        let state = if template_entry.is_some() {
            DedupeState::Upper
        } else {
            DedupeState::New
        };
        ctx.remove_whiteout(&mapped)?;
        Ok((state, size))
    }

    /// B100100：文件 `create` 回调返回后路径必须立即可解析——WinFSP 在
    /// create 之后会对该路径做按名查询，查不到则整个 create 被判不存在，
    /// 零字节文件（写 0 字节、无 staged 终裁）"刚写入即消失"，git 报
    /// "unable to stat just-written file"。
    ///
    /// * whiteout（git force 检出的前置 unlink）：撤销之。模板命中且
    ///   size == 0（空 blob）时经 [`Overlay::finish_staged`] 零拷贝终裁——
    ///   回落 lower，零 upper 物化、零 fsync（干净检出的 0-upper/0-fsync
    ///   性质不受影响）；模板命中且非空仅撤销（真实内容随后续写入的
    ///   staged 终裁落地），不物化空 upper。
    /// * NotFound（无模板覆盖，含全量 copy 兜底）：物化空 upper 占位文件
    ///   （plain create，无 fsync；持久性由后续写入的 staged 终裁或
    ///   cleanup 的零写入终裁保证）。
    /// * 其余（Upper/Lower/Directory）：已可解析，no-op。
    pub fn prepare_created_file(&self, rel: &str) -> Result<()> {
        self.validate_rel(rel)?;
        let mut ctx = self.meta_ctx();
        match self.resolve_with(&ctx, rel)? {
            Resolution::Whiteout => {
                let mapped = self.mapped_or_same(&ctx, rel)?;
                if self.lookup_lower(&mapped).is_some_and(|t| t.size == 0) {
                    drop(ctx);
                    self.finish_staged(
                        rel,
                        crate::dedupe::hash_bytes(&[]),
                        0,
                        StagedContent::Memory(&[]),
                    )?;
                } else {
                    ctx.remove_whiteout(&mapped)?;
                }
                Ok(())
            }
            Resolution::NotFound => {
                let path = self.upper_abs(rel);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::File::create(&path)?;
                self.io.record_upper_write();
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// copy-up：lower 内容复制到 upper 以便就地修改；已在 upper 则返回其路径。
    /// 刚复制的副本与模板内容一致，mtime 归一到 per-worktree epoch
    /// （best-effort）——stat 在 copy-up 前后保持 lower 固定值，git index
    /// 对齐不被 copy-up 打破；真实修改会自然刷新真实 mtime。
    pub fn copy_up(&self, rel: &str) -> Result<Option<PathBuf>> {
        self.validate_rel(rel)?;
        let final_path = self.upper_abs(rel);
        let mut ctx = self.meta_ctx();
        match self.resolve_with(&ctx, rel)? {
            Resolution::Lower { path, .. } => {
                if let Some(parent) = final_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(&path, &final_path)?;
                self.io.record_upper_write();
                let mapped = self.mapped_or_same(&ctx, rel)?;
                ctx.remove_whiteout(&mapped)?;
                normalize_template_identical_mtime(&final_path, self.config.fixed_stat_epoch_ns);
                Ok(Some(final_path))
            }
            Resolution::Upper { path } => Ok(Some(path)),
            _ => Ok(None),
        }
    }

    /// 删除：upper 删真实文件；lower 有则记 whiteout（否则被删文件会在合并视图重现）。
    pub fn remove(&self, rel: &str) -> Result<bool> {
        self.validate_rel(rel)?;
        let mut ctx = self.meta_ctx();
        let upper_path = self.upper_abs(rel);
        match self.resolve_with(&ctx, rel)? {
            Resolution::Upper { .. } => {
                std::fs::remove_file(&upper_path)?;
                let mapped = self.mapped_or_same(&ctx, rel)?;
                if self.lower_has_file_or_dir(&mapped) {
                    ctx.add_whiteout(&mapped)?;
                }
                Ok(true)
            }
            Resolution::Directory { has_lower, has_upper } => {
                if has_upper {
                    std::fs::remove_dir_all(&upper_path)?;
                }
                let mapped = self.mapped_or_same(&ctx, rel)?;
                if has_lower || self.lower_has_file_or_dir(&mapped) {
                    ctx.add_whiteout(&mapped)?;
                }
                Ok(true)
            }
            Resolution::Lower { .. } => {
                let mapped = self.mapped_or_same(&ctx, rel)?;
                ctx.add_whiteout(&mapped)?;
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
        let mut ctx = self.meta_ctx();
        let source = self.resolve_with(&ctx, from)?;
        if matches!(source, Resolution::NotFound | Resolution::Whiteout) {
            return Err(Error::NotFound(from.to_string()));
        }
        let target = self.resolve_with(&ctx, to)?;
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
        let mapped_from = self.mapped_or_same(&ctx, from)?;
        let mapped_to = self.mapped_or_same(&ctx, to)?;
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
                    // lower 经 rename 落 upper：内容与模板一致，mtime 归一到
                    // epoch（best-effort），index 对齐不因改名破坏。
                    normalize_template_identical_mtime(&to_upper, self.config.fixed_stat_epoch_ns);
                }
                if self.lower_has_file_or_dir(&mapped_from) {
                    ctx.add_whiteout(&mapped_from)?;
                }
                ctx.remove_whiteout(&mapped_to)?;
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
                    ctx.record_rename(&mapped_from, &mapped_to)?;
                }
                // whiteout 保留在 lower 命名空间（改写后的 lower 查询仍能命中）。
            }
            Resolution::Whiteout | Resolution::NotFound => unreachable!("checked above"),
        }
        Ok(())
    }

    // ---- stat（暴露给 VFS 的稳定值） ----

    /// 推导 stat（M11/B110100）：lower 固定（dev/ino/mtime/ctime 恒定，
    /// size/mode 来自模板，mtime = per-worktree epoch）；upper 用真实文件
    /// mtime；ino = 路径稳定哈希（根目录固定值）；目录 `0o040755`。
    /// whiteout/not-found → None。纯读：零 DB、零写副作用、零内存发放。
    pub fn stat(&self, rel: &str) -> Result<Option<Stat>> {
        self.validate_rel_or_root(rel)?;
        if !rel.is_empty() && is_temp_name(basename(rel)) {
            return Ok(None);
        }
        let vs = u64::from(self.config.volume_serial);
        let epoch = self.config.fixed_stat_epoch_ns;
        let ctx = self.meta_ctx();
        let resolution = self.resolve_with(&ctx, rel)?;
        let out = match resolution {
            Resolution::Whiteout | Resolution::NotFound => return Ok(None),
            Resolution::Upper { path } => {
                let md = std::fs::metadata(&path)?;
                let mode = if self.lower_mode_is_exec(&ctx, rel)? {
                    0o100755
                } else {
                    0o100644
                };
                let mtime = metadata_mtime_ns(&md);
                Stat {
                    dev: vs,
                    ino: file_id_for(rel),
                    mode,
                    size: md.len(),
                    mtime_ns: mtime,
                    ctime_ns: mtime,
                }
            }
            Resolution::Lower { file, .. } => Stat {
                dev: vs,
                ino: file_id_for(rel),
                mode: file.mode,
                size: file.size,
                mtime_ns: epoch,
                ctime_ns: epoch,
            },
            Resolution::Directory { has_upper, .. } => {
                let mtime = if has_upper {
                    let md = std::fs::metadata(self.upper_abs(rel))?;
                    metadata_mtime_ns(&md)
                } else {
                    epoch
                };
                Stat {
                    dev: vs,
                    ino: file_id_for(rel),
                    mode: 0o040755,
                    size: 0,
                    mtime_ns: mtime,
                    ctime_ns: mtime,
                }
            }
        };
        Ok(Some(out))
    }

    /// 稳定 file id（Windows FILE_ID 与 index ino 用）：blake3(小写路径)
    /// 直接派生，路径不变则不变，跨进程/跨重开天然一致；无持久映射、无
    /// 冲突探测（62 位哈希，冲突概率对仓库规模可忽略）。空路径非法。
    pub fn file_id(&self, rel: &str) -> Result<u64> {
        validate_relative_path(rel)?;
        Ok(file_id_for(rel))
    }

    fn lower_mode_is_exec(&self, ctx: &MetaCtx<'_>, rel: &str) -> Result<bool> {
        let mapped = self.mapped_or_same(ctx, rel)?;
        Ok(self
            .lower
            .as_ref()
            .and_then(|m| m.file(&mapped))
            .map(|f| f.mode & 0o111 != 0)
            .unwrap_or(false))
    }
}

/// 路径 → 稳定 ino；根目录（`""`）用固定值（[`ROOT_FILE_ID`]）。
fn file_id_for(rel: &str) -> u64 {
    if rel.is_empty() {
        ROOT_FILE_ID
    } else {
        derive_ino(rel) as u64
    }
}

/// 文件系统真实 mtime → Unix 纳秒（早于 Unix 纪元截为 0）。
fn metadata_mtime_ns(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as i64)
}

/// 把与模板内容一致的 upper 副本 mtime 归一到 per-worktree epoch
/// （best-effort：失败只影响 git 一次额外 rehash，正确性不受影响）。
fn normalize_template_identical_mtime(path: &Path, epoch_ns: i64) {
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|f| {
            f.set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_nanos(epoch_ns.max(0) as u64),
            )
        });
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
        // c.txt 内容与模板一致（rename 即 copy-up）：mtime 归一到 per-worktree
        // epoch，stat 在改名前后保持 lower 固定值（index 对齐不破坏）。
        let st = of.stat("c.txt").unwrap().unwrap();
        assert_eq!(st.mtime_ns, crate::FIXED_STAT_EPOCH_UNIX_NS);
        assert_eq!(st.mode, 0o100644);
    }

    #[test]
    fn stat_fixed_lower_real_upper_mtime_and_stable_across_stats() {
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
        // 目录（lower-only → epoch；有 upper → 真实 mtime）
        let dir_st = of.stat("a").unwrap().unwrap();
        assert_eq!(dir_st.mode, 0o040755);
        assert_eq!(dir_st.size, 0);
        assert_eq!(dir_st.mtime_ns, epoch, "lower-only 目录用 epoch");
        assert_eq!(attributes_for(&dir_st), FileAttributes::DIRECTORY);
        assert_eq!(attributes_for(&st), FileAttributes::ARCHIVE);

        // upper：真实文件 mtime，保留模板可执行位
        of.write("exec.sh", b"modified").unwrap();
        let st1 = of.stat("exec.sh").unwrap().unwrap();
        assert_eq!(st1.mode, 0o100755, "exec bit survives copy-up");
        assert!(st1.mtime_ns > epoch);
        assert_eq!(st1.size, 8);
        assert_eq!(st1.ino, of.file_id("exec.sh").unwrap());
        // mtime = upper 物理文件的真实 mtime
        let real = std::fs::metadata(fx.upper_dir.join("exec.sh"))
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;
        assert_eq!(st1.mtime_ns, real);
        // 同一路径重复 stat 稳定（真实 mtime，无发放时钟）
        assert_eq!(of.stat("exec.sh").unwrap().unwrap().mtime_ns, st1.mtime_ns);

        // upper 目录的真实 mtime
        std::fs::create_dir_all(fx.upper_dir.join("u")).unwrap();
        let upper_dir_st = of.stat("u").unwrap().unwrap();
        assert!(upper_dir_st.mtime_ns > epoch, "有 upper 的目录报真实 mtime");

        // 模板一致重写回落 lower → stat 回到固定 epoch
        let (state, _) = of.write("a/1.txt", ONE).unwrap();
        assert_eq!(state, DedupeState::Lower, "模板一致内容回落 lower");
        assert_eq!(of.stat("a/1.txt").unwrap().unwrap().mtime_ns, epoch);
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
            // M8：initializing 期元数据在内存缓冲，mark_ready 单事务落库；
            // 此处 flush 后重开，验证持久化往返（崩溃于 ready 前 = 缓冲丢失，
            // 见 crash_before_ready_keeps_initializing_and_upper_files）。
            of.mark_ready().unwrap();
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
        // file id（路径哈希）与真实 mtime 持久（不重发放）
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

    // ---- M9/B090100：暂存写零拷贝终裁 ----

    /// 递归统计目录下的文件数（零残留断言用）。
    fn count_files(dir: &std::path::Path) -> usize {
        let mut files = 0;
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            if let Ok(rd) = std::fs::read_dir(&d) {
                for entry in rd.flatten() {
                    if entry.path().is_dir() {
                        stack.push(entry.path());
                    } else {
                        files += 1;
                    }
                }
            }
        }
        files
    }

    /// 干净 N 文件 checkout：finish_staged 全部与模板一致 → 零 upper 文件、
    /// 零 fsync、零临时副本；决策记 InitBatch，mark_ready 单事务落库。
    #[test]
    fn finish_staged_clean_checkout_zero_upper_zero_fsync() {
        let fx = fixture(true);
        let of = make(&fx, true);
        for (path, content) in
            [("a/1.txt", ONE), ("a/2.txt", TWO), ("b.txt", BEE), ("exec.sh", RUN)]
        {
            let (state, size) = of
                .finish_staged(
                    path,
                    crate::dedupe::hash_bytes(content),
                    content.len() as u64,
                    StagedContent::Memory(content),
                )
                .unwrap();
            assert_eq!(state, DedupeState::Lower);
            assert_eq!(size, content.len() as u64);
        }
        assert_eq!(of.io_stats().fsyncs(), 0, "干净 checkout 零 fsync");
        assert_eq!(of.io_stats().upper_writes(), 0, "干净 checkout 零 upper 物化");
        assert_eq!(count_files(&fx.upper_dir), 0, "upper 整棵零文件（零临时副本）");
        // 视图回落 lower；决策不再落库（M11：dedupe 表已删除），upper 空 +
        // 视图可见即语义。
        assert_eq!(of.read("b.txt").unwrap().as_deref(), Some(BEE));
        of.mark_ready().unwrap();
        assert_eq!(fx.store.state(WorktreeId(1)).unwrap(), Some(crate::WorktreeState::Ready));
        for p in ["a/1.txt", "a/2.txt", "b.txt", "exec.sh"] {
            assert!(
                matches!(of.resolve(p).unwrap(), Resolution::Lower { .. }),
                "{p} must resolve to lower after clean checkout"
            );
        }
        assert!(fx.store.whiteouts(WorktreeId(1)).unwrap().is_empty(), "lower 无 whiteout");
    }

    /// 改动文件恰好一次物化 + 一次 fsync；再写回模板内容回收 upper
    /// （dedupe-revert 零额外 fsync/物化）。
    #[test]
    fn finish_staged_materializes_upper_with_single_fsync_and_reverts() {
        let fx = fixture(true);
        let of = make(&fx, true);
        let (state, _) = of
            .finish_staged("b.txt", crate::dedupe::hash_bytes(b"changed"), 7, StagedContent::Memory(b"changed"))
            .unwrap();
        assert_eq!(state, DedupeState::Upper);
        assert_eq!(of.io_stats().fsyncs(), 1);
        assert_eq!(of.io_stats().upper_writes(), 1);
        assert!(fx.upper_dir.join("b.txt").is_file());
        assert_eq!(of.read("b.txt").unwrap().as_deref(), Some(b"changed".as_slice()));

        // dedupe-revert：写回模板一致内容 → 回收既有 upper 回落 lower
        let (state, _) = of
            .finish_staged("b.txt", crate::dedupe::hash_bytes(BEE), BEE.len() as u64, StagedContent::Memory(BEE))
            .unwrap();
        assert_eq!(state, DedupeState::Lower);
        assert!(!fx.upper_dir.join("b.txt").exists());
        assert_eq!(of.io_stats().fsyncs(), 1, "dedupe 不新增 fsync");
        assert_eq!(of.io_stats().upper_writes(), 1, "dedupe 不新增物化");
        assert_eq!(of.read("b.txt").unwrap().as_deref(), Some(BEE));
    }

    /// 新文件（模板缺路径）→ New，同样一次物化 + 一次 fsync。
    #[test]
    fn finish_staged_new_file_counts_as_new_with_one_fsync() {
        let fx = fixture(true);
        let of = make(&fx, true);
        let (state, size) = of
            .finish_staged("new.txt", crate::dedupe::hash_bytes(b"fresh"), 5, StagedContent::Memory(b"fresh"))
            .unwrap();
        assert_eq!(state, DedupeState::New);
        assert_eq!(size, 5);
        assert_eq!(of.io_stats().fsyncs(), 1);
        assert_eq!(of.io_stats().upper_writes(), 1);
        assert!(matches!(of.resolve("new.txt").unwrap(), Resolution::Upper { .. }));
    }

    /// 溢写路径物化：同卷 rename 消费溢写文件，恰好一次 fsync；
    /// 溢写 + 模板一致 → 删除溢写、零 fsync。
    #[test]
    fn finish_staged_spill_renames_to_upper_and_discards_on_dedupe() {
        let fx = fixture(true);
        let of = make(&fx, true);

        // 一致内容：溢写删除、零 fsync、零物化
        let spill = fx.upper_dir.join(".wtfs-tmp-w-test-0");
        std::fs::write(&spill, BEE).unwrap();
        let (state, _) = of
            .finish_staged("b.txt", crate::dedupe::hash_bytes(BEE), BEE.len() as u64, StagedContent::Spill(&spill))
            .unwrap();
        assert_eq!(state, DedupeState::Lower);
        assert!(!spill.exists(), "一致的溢写文件必须删除");
        assert!(!fx.upper_dir.join("b.txt").exists());
        assert_eq!(of.io_stats().fsyncs(), 0);
        assert_eq!(of.io_stats().upper_writes(), 0);

        // 不同内容：rename 到最终路径，恰好一次 fsync
        let spill = fx.upper_dir.join(".wtfs-tmp-w-test-1");
        std::fs::write(&spill, b"spilled payload").unwrap();
        let (state, size) = of
            .finish_staged("new.txt", crate::dedupe::hash_bytes(b"spilled payload"), 15, StagedContent::Spill(&spill))
            .unwrap();
        assert_eq!(state, DedupeState::New);
        assert_eq!(size, 15);
        assert!(!spill.exists(), "溢写文件经 rename 消费");
        assert!(fx.upper_dir.join("new.txt").is_file());
        assert_eq!(of.read("new.txt").unwrap().as_deref(), Some(b"spilled payload".as_slice()));
        assert_eq!(of.io_stats().fsyncs(), 1);
        assert_eq!(of.io_stats().upper_writes(), 1);
    }

    /// 祖先目录 whiteout 守护（P1-1 在 finish_staged 同样生效）：一致内容
    /// 不回落；精确 whiteout 的文件写一致内容仍回落。
    #[test]
    fn finish_staged_under_ancestor_whiteout_stays_upper() {
        let fx = fixture(true);
        let of = make(&fx, true);
        assert!(of.remove("a").unwrap());
        let (state, _) = of
            .finish_staged("a/1.txt", crate::dedupe::hash_bytes(ONE), ONE.len() as u64, StagedContent::Memory(ONE))
            .unwrap();
        assert_eq!(state, DedupeState::Upper, "祖先目录 whiteout 不得回落");
        assert!(fx.upper_dir.join("a/1.txt").is_file());
        assert_eq!(of.io_stats().fsyncs(), 1);

        // 对照：文件级 whiteout 的精确路径一致重写回落 lower（零 fsync）
        assert!(of.remove("b.txt").unwrap());
        let (state, _) = of
            .finish_staged("b.txt", crate::dedupe::hash_bytes(BEE), BEE.len() as u64, StagedContent::Memory(BEE))
            .unwrap();
        assert_eq!(state, DedupeState::Lower);
        assert_eq!(of.io_stats().fsyncs(), 1, "回落路径不 fsync");
    }

    /// B100100（core 语义）：prepare_created_file——whiteout 撤销（空 blob
    /// 零拷贝终裁回落 lower / 非空仅撤销不物化）；NotFound 物化空 upper
    /// 占位（路径立即可解析）。
    #[test]
    fn prepare_created_file_restores_resolvability() {
        let fx = fixture(true);

        // 非空模板条目：仅撤销 whiteout。
        let of = make(&fx, true);
        assert!(of.remove("b.txt").unwrap());
        assert_eq!(of.resolve("b.txt").unwrap(), Resolution::Whiteout);
        of.prepare_created_file("b.txt").unwrap();
        assert!(matches!(of.resolve("b.txt").unwrap(), Resolution::Lower { .. }));
        assert!(!fx.upper_dir.join("b.txt").exists(), "不得物化空 upper");
        assert_eq!(of.io_stats().fsyncs(), 0);
        assert_eq!(of.io_stats().upper_writes(), 0);
        of.mark_ready().unwrap();

        // 空 blob 模板条目（B100100 主场景）：零拷贝终裁撤销 whiteout、
        // 回落 lower，零物化零 fsync；决策随 mark_ready 落库。
        let mut cfg = OverlayConfig::new(WorktreeId(2), &fx.upper_dir);
        cfg.lower_root = Some(fx.lower_dir.clone());
        cfg.template_id = Some(TemplateId(1));
        let mut map = lower_fixture(&fx.lower_dir);
        map.insert(
            "empty.txt".to_string(),
            TemplateFile {
                path: "empty.txt".to_string(),
                hash: crate::dedupe::hash_bytes(&[]),
                size: 0,
                mode: 0o100644,
            },
        );
        let of2 = Overlay::new(cfg, Some(map), fx.store.clone()).unwrap();
        assert!(of2.remove("empty.txt").unwrap());
        assert_eq!(of2.resolve("empty.txt").unwrap(), Resolution::Whiteout);
        of2.prepare_created_file("empty.txt").unwrap();
        assert!(matches!(of2.resolve("empty.txt").unwrap(), Resolution::Lower { .. }));
        assert!(!fx.upper_dir.join("empty.txt").exists());
        assert_eq!(of2.io_stats().fsyncs(), 0);
        assert_eq!(of2.io_stats().upper_writes(), 0);
        of2.mark_ready().unwrap();
        // 撤销后的 whiteout 不落库（回落 lower 视图）。
        assert!(fx.store.whiteouts(WorktreeId(2)).unwrap().is_empty());
        assert!(matches!(of2.resolve("empty.txt").unwrap(), Resolution::Lower { .. }));
    }

    /// B100100：NotFound（无模板覆盖）上 prepare_created_file 物化空 upper
    /// 占位——create 返回后路径立即可解析（全量 copy 兜底模式同此路径）。
    #[test]
    fn prepare_created_file_materializes_placeholder_for_uncovered_paths() {
        let fx = fixture(true);
        let of = make(&fx, true);

        of.prepare_created_file("brand-new.txt").unwrap();
        assert!(matches!(
            of.resolve("brand-new.txt").unwrap(),
            Resolution::Upper { .. }
        ));
        let st = of.stat("brand-new.txt").unwrap().unwrap();
        assert_eq!(st.size, 0);
        assert_eq!(
            fx.upper_dir.join("brand-new.txt").metadata().unwrap().len(),
            0,
            "占位文件 EOF 精确为 0"
        );
        assert_eq!(of.io_stats().fsyncs(), 0, "占位不 fsync（持久性由终裁保证）");
        assert_eq!(of.io_stats().upper_writes(), 1);

        // 已可解析的路径 no-op（幂等）。
        of.prepare_created_file("brand-new.txt").unwrap();
        assert_eq!(of.io_stats().upper_writes(), 1);
        of.mark_ready().unwrap();
        // 占位 upper 是数据真相（无 dedupe 行落库，M11）。
        assert!(matches!(
            of.resolve("brand-new.txt").unwrap(),
            Resolution::Upper { .. }
        ));
    }

    /// 崩溃于 mark_ready 前：finish_staged 物化的 upper 文件完整保留
    /// （数据真相），一致内容无残留；重开保持 initializing、可继续累积。
    #[test]
    fn finish_staged_crash_before_ready_keeps_upper_intact() {
        let fx = fixture(true);
        let of = make(&fx, true);
        of.finish_staged("a/1.txt", crate::dedupe::hash_bytes(b"changed!"), 8, StagedContent::Memory(b"changed!"))
            .unwrap();
        of.finish_staged("b.txt", crate::dedupe::hash_bytes(BEE), BEE.len() as u64, StagedContent::Memory(BEE))
            .unwrap();
        of.finish_staged("new.txt", crate::dedupe::hash_bytes(b"fresh"), 5, StagedContent::Memory(b"fresh"))
            .unwrap();
        drop(of); // 模拟崩溃：缓冲不 flush

        assert_eq!(
            fx.store.state(WorktreeId(1)).unwrap(),
            Some(crate::WorktreeState::Initializing)
        );
        assert!(fx.upper_dir.join("a/1.txt").is_file());
        assert!(fx.upper_dir.join("new.txt").is_file(), "upper 文件是数据真相");
        assert!(!fx.upper_dir.join("b.txt").exists(), "一致内容零残留");

        let mut cfg = OverlayConfig::new(WorktreeId(1), &fx.upper_dir);
        cfg.lower_root = Some(fx.lower_dir.clone());
        cfg.template_id = Some(TemplateId(1));
        let of2 = Overlay::reopen(cfg, Some(lower_fixture(&fx.lower_dir)), fx.store.clone()).unwrap();
        assert!(of2.is_initializing().unwrap());
        assert_eq!(of2.list("").unwrap().len(), 0, "readdir 仍为空");
        assert_eq!(of2.read("new.txt").unwrap().as_deref(), Some(b"fresh".as_slice()));
        let (state, _) = of2.write("late.txt", b"late").unwrap();
        assert_eq!(state, DedupeState::New, "重开后继续 checkout");
        of2.mark_ready().unwrap();
        assert_eq!(
            fx.store.state(WorktreeId(1)).unwrap(),
            Some(crate::WorktreeState::Ready)
        );
        // M8 语义：崩溃丢缓冲——b.txt 的回收决策不落库（无 upper 文件，
        // 视图天然回落 lower）；late.txt 的 upper 是数据真相，重开即见。
        assert!(fx.upper_dir.join("late.txt").is_file(), "late.txt 物化已保留");
        assert_eq!(of2.read("b.txt").unwrap().as_deref(), Some(BEE), "一致内容零残留 → 回落 lower");
    }

    /// 终裁拒绝保留前缀名；写_from 与 finish_staged 互不干扰（计数器分段）。
    #[test]
    fn finish_staged_rejects_reserved_temp_prefix() {
        let fx = fixture(true);
        let of = make(&fx, true);
        let err = of
            .finish_staged(".wtfs-tmp-1-1", crate::dedupe::hash_bytes(b"x"), 1, StagedContent::Memory(b"x"))
            .unwrap_err();
        assert!(matches!(err, crate::Error::Overlay(_)));
        assert_eq!(of.io_stats().fsyncs(), 0);
    }

    /// P2-1 回归：内存物化经 TEMP_PREFIX 临时文件原子改名——成功后零临时
    /// 残留、最终内容/长度精确；rename 失败（目标被目录占用）时最终路径
    /// 不被部分覆写、临时文件被清理。最终路径只经原子 rename 替换。
    #[test]
    fn finish_staged_memory_materializes_via_temp_rename_without_residue() {
        let fx = fixture(true);
        let of = make(&fx, true);
        let (state, size) = of
            .finish_staged("new.txt", crate::dedupe::hash_bytes(b"payload!"), 8, StagedContent::Memory(b"payload!"))
            .unwrap();
        assert_eq!(state, DedupeState::New);
        assert_eq!(size, 8);
        let temp_residue = || -> Vec<String> {
            std::fs::read_dir(&fx.upper_dir)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with(TEMP_PREFIX))
                .collect()
        };
        assert!(temp_residue().is_empty(), "临时文件必须经 rename 消费");
        let md = std::fs::metadata(fx.upper_dir.join("new.txt")).unwrap();
        assert_eq!(md.len(), 8);
        assert_eq!(of.read("new.txt").unwrap().as_deref(), Some(b"payload!".as_slice()));
        assert_eq!(of.io_stats().fsyncs(), 1);
        assert_eq!(of.io_stats().upper_writes(), 1);

        // rename 失败（目标为目录）：终裁报错，目录不被替换，无临时残留；
        // 失败发生在 fsync 之后（物化恰好一次 fsync 已计数）。
        std::fs::create_dir(fx.upper_dir.join("dir.txt")).unwrap();
        let err = of
            .finish_staged("dir.txt", crate::dedupe::hash_bytes(b"x"), 1, StagedContent::Memory(b"x"))
            .unwrap_err();
        assert!(matches!(err, crate::Error::Io(_)));
        assert!(fx.upper_dir.join("dir.txt").is_dir(), "目录不被部分覆写");
        assert!(temp_residue().is_empty(), "失败路径清理临时文件");
        assert_eq!(of.io_stats().fsyncs(), 2);
        assert_eq!(of.io_stats().upper_writes(), 1, "失败的 rename 不计物化");
    }

    // ---- M8/B080100：批量元数据与 O(1) 事务 ----

    const N_FILES: usize = 48;

    /// N 文件 checkout（initializing 期全内存批量）→ 写事务 O(1)：
    /// register + mark_ready flush 共 2 个事务（M11 后 per-file 零 DB 读写）。
    #[test]
    fn batched_checkout_uses_constant_transactions_and_flushes_rows() {
        let fx = fixture(true);
        let base = fx.store.db().tx_stats();
        let of = make(&fx, true); // register：+1 事务

        for i in 0..N_FILES {
            let (path, content, expect_state): (String, &[u8], DedupeState) = match i % 3 {
                0 => ("a/1.txt".to_string(), ONE, DedupeState::Lower),
                1 => ("a/2.txt".to_string(), b"changed!", DedupeState::Upper),
                _ => (format!("new/f{i}.txt"), b"fresh", DedupeState::New),
            };
            let (state, _) = of.write(&path, content).unwrap();
            assert_eq!(state, expect_state);
            // checkout 的 stat：file_id（路径哈希）/ mtime（真实值）纯推导。
            of.stat(&path).unwrap();
        }
        // 批量窗口内：仅 register 的 1 个事务。
        let during = fx.store.db().tx_stats();
        assert_eq!(during.committed - base.committed, 1, "仅 register");
        assert_eq!(during.rolled_back - base.rolled_back, 0);

        of.mark_ready().unwrap();
        let after = fx.store.db().tx_stats();
        let total = (after.committed - base.committed) + (after.rolled_back - base.rolled_back);
        assert_eq!(total, 2, "O(1)：register + flush");

        // 状态落库；本批无删除 → 无 whiteout 行；ino = 路径哈希纯推导。
        assert_eq!(
            fx.store.state(WorktreeId(1)).unwrap(),
            Some(crate::WorktreeState::Ready)
        );
        assert!(fx.store.whiteouts(WorktreeId(1)).unwrap().is_empty());
        assert_eq!(
            of.file_id("a/1.txt").unwrap(),
            crate::overlay_store::derive_ino("a/1.txt") as u64,
            "ino 由路径稳定哈希派生"
        );
    }

    /// 崩溃于 mark_ready 前：worktree 保持 initializing、upper 文件完整
    /// （缓冲丢失可接受，spec §6.3.6）；重开后仍隐藏视图、可继续累积并 flush。
    #[test]
    fn crash_before_ready_keeps_initializing_and_upper_files() {
        let fx = fixture(true);
        let of = make(&fx, true);
        of.write("a/1.txt", b"changed!").unwrap(); // upper（改动）
        of.write("b.txt", BEE).unwrap(); // 与模板一致 → 无副本
        of.write("new.txt", b"fresh").unwrap(); // upper（新文件）
        drop(of); // 模拟崩溃：缓冲不 flush

        assert_eq!(
            fx.store.state(WorktreeId(1)).unwrap(),
            Some(crate::WorktreeState::Initializing)
        );
        assert!(fx.store.whiteouts(WorktreeId(1)).unwrap().is_empty());
        assert!(fx.upper_dir.join("new.txt").is_file(), "upper 文件是数据真相");
        assert!(fx.upper_dir.join("a/1.txt").is_file());

        let mut cfg = OverlayConfig::new(WorktreeId(1), &fx.upper_dir);
        cfg.lower_root = Some(fx.lower_dir.clone());
        cfg.template_id = Some(TemplateId(1));
        let of2 = Overlay::reopen(cfg, Some(lower_fixture(&fx.lower_dir)), fx.store.clone()).unwrap();
        assert!(of2.is_initializing().unwrap(), "崩溃后保持 initializing");
        assert_eq!(of2.list("").unwrap().len(), 0, "readdir 仍为空");
        let (state, _) = of2.write("late.txt", b"late").unwrap();
        assert_eq!(state, DedupeState::New, "重开后继续 checkout");
        of2.mark_ready().unwrap();
        assert_eq!(
            fx.store.state(WorktreeId(1)).unwrap(),
            Some(crate::WorktreeState::Ready)
        );
        assert!(fx.upper_dir.join("late.txt").is_file());
        assert_eq!(of2.read("new.txt").unwrap().as_deref(), Some(b"fresh".as_slice()));
    }

    /// upper mtime = 真实文件 mtime：同会话稳定、跨重开一致（M11 后无逻辑
    /// 时钟；mtime 随真实修改自然前进，lower 恒为 per-worktree epoch）。
    #[test]
    fn upper_mtime_is_real_file_mtime_stable_across_reopen() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("meta.db");
        let lower_dir = dir.path().join("lower");
        let upper_dir = dir.path().join("upper");
        std::fs::create_dir_all(&lower_dir).unwrap();
        let store = Arc::new(SqliteOverlayStore::new(Db::open(&db_path).unwrap()));
        let mut cfg = OverlayConfig::new(WorktreeId(5), &upper_dir);
        cfg.lower_root = Some(lower_dir.clone());
        cfg.dedupe_enabled = true;
        let seen: Vec<i64> = {
            let of = Overlay::new(cfg.clone(), Some(lower_fixture(&lower_dir)), store.clone()).unwrap();
            let mut seen = Vec::new();
            for i in 0..8 {
                let path = format!("n{i}.txt");
                of.write(&path, b"x").unwrap();
                let st = of.stat(&path).unwrap().unwrap();
                let real = std::fs::metadata(upper_dir.join(&path))
                    .unwrap()
                    .modified()
                    .unwrap()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as i64;
                assert_eq!(st.mtime_ns, real, "{path} 的 stat mtime 必须等于真实文件 mtime");
                assert!(st.mtime_ns > crate::FIXED_STAT_EPOCH_UNIX_NS);
                seen.push(st.mtime_ns);
            }
            of.mark_ready().unwrap();
            seen
        };
        let of2 = Overlay::reopen(cfg, Some(lower_fixture(&lower_dir)), store.clone()).unwrap();
        for (i, t) in seen.iter().enumerate() {
            let path = format!("n{i}.txt");
            assert_eq!(
                of2.stat(&path).unwrap().unwrap().mtime_ns,
                *t,
                "{path} 重开后 mtime 一致（真实文件 mtime，无重发放）"
            );
        }
        // 同一路径真实修改后 mtime 前进（由文件系统保证，无需时钟）。
        std::thread::sleep(std::time::Duration::from_millis(50));
        of2.write("n0.txt", b"xx").unwrap();
        assert!(
            of2.stat("n0.txt").unwrap().unwrap().mtime_ns > seen[0],
            "真实修改后 mtime 前进"
        );
    }

    /// 并发 VFS 回调线程安全：多线程同写 initializing 缓冲，flush 后视图完整。
    #[test]
    fn concurrent_init_writes_are_thread_safe_and_fully_flushed() {
        let fx = fixture(true);
        let of = Arc::new(make(&fx, true));
        let threads = 4u64;
        let per_thread = 16u64;
        let mut handles = Vec::new();
        for t in 0..threads {
            let of = of.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..per_thread {
                    let p = format!("t{t}/f{i}.txt");
                    let (state, _) = of.write(&p, b"payload").unwrap();
                    assert_eq!(state, DedupeState::New);
                    of.stat(&p).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        of.mark_ready().unwrap();
        let id = WorktreeId(1);
        let total = threads * per_thread;
        assert_eq!(fx.store.state(id).unwrap(), Some(crate::WorktreeState::Ready));
        assert!(fx.store.whiteouts(id).unwrap().is_empty());
        // flush 后全部路径可解析 upper、内容正确；ino 纯推导一致。
        for t in 0..threads {
            for i in 0..per_thread {
                let p = format!("t{t}/f{i}.txt");
                assert!(matches!(of.resolve(&p).unwrap(), Resolution::Upper { .. }));
                assert_eq!(of.read(&p).unwrap().as_deref(), Some(b"payload".as_slice()));
                assert_eq!(of.file_id(&p).unwrap(), crate::overlay_store::derive_ino(&p) as u64);
            }
        }
    }

    /// 幂等 mark_ready：已 ready 的重开实例二次 mark_ready 只切状态，
    /// 绝不重写/清空既有元数据行（daemon watcher 的 already-ready 路径）。
    #[test]
    fn mark_ready_without_batch_never_touches_existing_rows() {
        let fx = fixture(true);
        let of = make(&fx, true);
        of.write("a/2.txt", b"changed!").unwrap();
        assert!(of.remove("b.txt").unwrap(), "制造一条 whiteout");
        of.mark_ready().unwrap();
        let before = fx.store.whiteouts(WorktreeId(1)).unwrap();
        assert_eq!(before.len(), 1);
        // 模拟 watcher 二次 mark_ready（无缓冲的实例走仅状态分支）
        of.mark_ready().unwrap();
        assert_eq!(
            fx.store.state(WorktreeId(1)).unwrap(),
            Some(crate::WorktreeState::Ready)
        );
        assert_eq!(fx.store.whiteouts(WorktreeId(1)).unwrap(), before);
        // 结构性保证：无缓冲分支只 UPDATE worktrees，不触碰任何子表行。
    }

    // ---- M11/B110100 回归：派生元数据（哈希 ino / 内存索引 / 真实 mtime）----

    /// 同一路径重开 Overlay 后 file_id 稳定（路径哈希派生，无持久映射依赖）、
    /// list 内存合并正确（lower 索引 ∪ upper − whiteout）、whiteout 删除的
    /// 文件重开后不可见。
    #[test]
    fn derived_metadata_survives_reopen_file_id_list_whiteout() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("meta.db");
        let lower_dir = dir.path().join("lower");
        let upper_dir = dir.path().join("upper");
        let db = Db::open(&db_path).unwrap();
        let store = Arc::new(SqliteOverlayStore::new(db));
        let mut cfg = OverlayConfig::new(WorktreeId(6), &upper_dir);
        cfg.lower_root = Some(lower_dir.clone());
        cfg.dedupe_enabled = true;

        let ino_map: std::collections::HashMap<String, u64> = {
            let of = Overlay::new(cfg.clone(), Some(lower_fixture(&lower_dir)), store.clone()).unwrap();
            of.write("a/1.txt", b"ONE!").unwrap(); // upper 覆盖
            of.write("fresh.txt", b"fresh").unwrap(); // upper 新增
            of.remove("b.txt").unwrap(); // whiteout
            of.remove("a").unwrap(); // 目录 whiteout
            of.write("a/9.txt", b"nine").unwrap(); // whiteout 目录内重建 upper
            of.mark_ready().unwrap();
            ["a/1.txt", "a/9.txt", "fresh.txt", "b.txt", "exec.sh", ""]
                .into_iter()
                .map(|p| (p.to_string(), of.file_id(p).unwrap_or(ROOT_FILE_ID)))
                .collect()
        };

        // 重开（新实例 + 新 lower 索引 + 从 DB 装载 whiteouts）。
        let of2 = Overlay::reopen(cfg, Some(lower_fixture(&lower_dir)), store.clone()).unwrap();
        assert!(!of2.is_initializing().unwrap());

        // 1) file_id 稳定：路径哈希派生与实例/映射表无关。
        for (p, ino) in &ino_map {
            assert_eq!(
                of2.file_id(p).unwrap_or(ROOT_FILE_ID),
                *ino,
                "file_id must be stable across reopen for {p:?}"
            );
            if p.is_empty() {
                assert_eq!(*ino, ROOT_FILE_ID, "根目录固定 ino");
            } else {
                assert_eq!(*ino, crate::overlay_store::derive_ino(p) as u64);
            }
        }

        // 2) whiteout 删除后不可见（含目录子树）；重建的 upper 例外可见。
        assert_eq!(of2.resolve("b.txt").unwrap(), Resolution::Whiteout);
        assert!(!names(&of2, "").contains(&"b.txt".to_string()));
        assert!(matches!(of2.resolve("a").unwrap(), Resolution::Directory { has_lower: false, has_upper: true }));
        assert_eq!(names(&of2, "a"), vec!["9.txt"], "whiteout 子树隐藏，upper 重建可见");

        // 3) list 合并正确：lower（内存索引）∪ upper − whiteout，kind 正确。
        let root = of2.list("").unwrap();
        let get = |name: &str| root.iter().find(|e| e.name == name).map(|e| e.kind);
        assert_eq!(get("a"), Some(EntryKind::Directory));
        assert_eq!(get("exec.sh"), Some(EntryKind::File));
        assert_eq!(get("fresh.txt"), Some(EntryKind::File));
        assert_eq!(get("b.txt"), None, "whiteout 必须从合并视图剔除");
        let sub = of2.list("a").unwrap();
        assert_eq!(
            sub.iter().map(|e| (e.name.as_str(), e.kind)).collect::<Vec<_>>(),
            vec![("9.txt", EntryKind::File)]
        );

        // mark_ready 幂等（无缓冲分支）：whiteout 行不被清空。
        of2.mark_ready().unwrap();
        assert!(
            !store.whiteouts(WorktreeId(6)).unwrap().is_empty(),
            "幂等 mark_ready 不得清空既有 whiteout 行"
        );
        assert_eq!(of2.resolve("b.txt").unwrap(), Resolution::Whiteout);
    }
}
