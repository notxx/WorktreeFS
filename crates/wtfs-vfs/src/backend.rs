//! VFS 后端抽象（M18/B180100）：B1 [`Overlay`] 与 C [`CasView`] 共用
//! WinFSP 回调层（[`crate::context`]）。
//!
//! [`ViewBackend`] 是回调层看到的唯一视图接口（enum 分发，无 dyn/vtable；
//! stat/枚举是 status 的热路径）。两条实现：
//!
//! * [`ViewBackend::Overlay`]：B1 模板 overlay（wtfs-core，零适配直通）。
//!   B1 行为与测试不回退：M9 零 fsync / M10 零字节 delete-on-close /
//!   M12 标记日志的状态机逐字保留。
//! * [`ViewBackend::Cas`]：C 的 CAS 视图（wtfs-cas）。[`CasBackend`] 把
//!   `&mut self` 写方法收进 per-view `Mutex`（WinFSP 回调并发 `&self` 进
//!   入），stat/枚举/读走同一把锁（`parking_lot` 快速路径）。
//!
//! 后端差异映射：
//!
//! | 语义 | B1 Overlay | C CasView |
//! | ---- | ---------- | --------- |
//! | initializing 暂存流 | checkout 期 staged 写 | 无（注册即 ready，恒走就地写） |
//! | lower 命中 | 模板树物理文件 | manifest（读时 hydrate） |
//! | 写终裁 dedupe | blake3 对模板 hash | git blob sha1 对 manifest oid |
//! | 目录 rename | lower 有内容 EXDEV | manifest 有内容 EXDEV（同语义） |
//!
//! [`register_cas_worktree`]（在 [`crate::registry`]）是 M18 显式流程的
//! 挂载侧入口：`git worktree add --no-checkout` + 索引种子由调用方经
//! `wtfs_cas` 完成后，把真实 worktree 目录（`.git` 所在）作为 upper、
//! manifest 作为 lower 组装视图并注册路由。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use wtfs_cas::{CasView, CasViewConfig, CacheStatsSnapshot, CatFile, HydrationCache, Manifest, StagedSource as CasStagedSource};
use wtfs_core::{
    DedupeState, EntryKind, FileHash, Overlay, OverlayDirEntry, OverlayStore, Stat, TemplateFile,
};

/// CAS 视图共享句柄：per-挂载的 cat-file 常驻进程与 hydration 缓存
/// （`<store_root>/cas-cache` 按 oid 全局分片——同 blob 跨 worktree 只存
/// 一份）。统计快照（命中/未命中/灌入）是"status 干净零 hydrate"断言的
/// 依据。
#[derive(Clone)]
pub struct CasShared {
    pub cat: Arc<CatFile>,
    pub cache: Arc<HydrationCache>,
}

/// C 后端：一个 CAS worktree 的视图句柄。
#[derive(Clone)]
pub struct CasBackend {
    /// `&mut self` 写方法（whiteout 镜像/标记日志单写者）收进锁；WinFSP
    /// 回调并发 `&self` 进入。
    view: Arc<Mutex<CasView>>,
    volume_serial: u32,
    epoch_ns: i64,
    /// 真实 worktree 目录（`.git` 指针所在；refresh 解析 HEAD 的基准）。
    /// 同时是视图的 upper 根——`.git` 经 upper 自然可见，git 无需特判。
    worktree: PathBuf,
    shared: Arc<CasShared>,
    /// checkout 吸收窗口：true 时 `is_initializing` 返回 true，VFS 走
    /// 内存暂存 + `finish_staged` 的 manifest oid 比对（匹配零 upper/
    /// 零 fsync），而不是 copy-up 就地写。由调用方在 `git worktree add`
    /// 的 checkout 前后置位/清除。
    absorbing: Arc<AtomicBool>,
}

impl CasBackend {
    /// 组装并打开视图（manifest/cat/cache 由调用方构建；markers 按 config
    /// 打开重放）。
    pub fn open(
        config: CasViewConfig,
        manifest: Arc<Manifest>,
        worktree: impl Into<PathBuf>,
        shared: Arc<CasShared>,
    ) -> wtfs_cas::Result<Self> {
        let view = CasView::open(
            config.clone(),
            manifest,
            shared.cat.clone(),
            shared.cache.clone(),
        )?;
        Ok(Self {
            view: Arc::new(Mutex::new(view)),
            volume_serial: config.volume_serial,
            epoch_ns: config.fixed_stat_epoch_ns,
            worktree: worktree.into(),
            shared,
            absorbing: Arc::new(AtomicBool::new(false)),
        })
    }

    /// 进入/退出 checkout 吸收窗口。窗口内写路径走内存暂存，close 时
    /// 按 manifest oid 比对：一致丢弃（零 upper/零 fsync），不一致落 upper。
    pub fn set_absorbing(&self, value: bool) {
        self.absorbing.store(value, Ordering::Relaxed);
    }

    pub fn absorbing(&self) -> bool {
        self.absorbing.load(Ordering::Relaxed)
    }

    /// upper 变更后失效负存在缓存（M14 口径）。
    pub fn invalidate_upper(&self) {
        self.view.lock().invalidate_upper();
    }

    /// upper 直接变更后修正三态索引（VFS create/truncate 等路径）。
    pub fn note_upper_mutation(&self, rel: &str) {
        self.view.lock().note_upper_mutation(rel);
    }

    pub fn volume_serial(&self) -> u32 {
        self.volume_serial
    }

    pub fn fixed_stat_epoch_ns(&self) -> i64 {
        self.epoch_ns
    }

    /// 真实 worktree 目录（upper 根）。
    pub fn worktree_dir(&self) -> &Path {
        &self.worktree
    }

    /// hydration 缓存统计（E2E/基准断言用）。
    pub fn cache_stats(&self) -> CacheStatsSnapshot {
        self.shared.cache.stats()
    }

    /// 缓存树当前字节量（基准"缓存大小"口径）。
    pub fn cached_bytes(&self) -> u64 {
        self.shared.cache.cached_bytes()
    }

    /// manifest 当前对应 commit。
    pub fn manifest_commit(&self) -> String {
        self.view.lock().manifest().commit().to_string()
    }

    /// HEAD 前进后刷新 manifest（[`CasView::refresh_head`]）；返回是否
    /// 重载。B1 无此语义（模板刷新归 daemon）。
    pub fn refresh_head(&self) -> wtfs_cas::Result<bool> {
        self.view.lock().refresh_head(&self.worktree)
    }

    /// 存活 whiteout 数（测试断言用）。
    pub fn whites_len(&self) -> usize {
        self.view.lock().whites().len()
    }

    fn view(&self) -> &Mutex<CasView> {
        &self.view
    }
}

/// 后端无关的解析结果（VFS 写路径状态机的输入）。
#[derive(Debug, Clone)]
pub enum ViewResolution {
    NotFound,
    Whiteout,
    /// upper 命中；`path` 为 upper 物理路径。
    Upper { path: PathBuf },
    /// lower 命中（B1 模板 / C manifest）；`copy_up` 可物化。
    Lower,
    Directory,
}

/// 后端无关的写终裁决策（core `DedupeState` 的镜像）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewWriteState {
    Lower,
    Upper,
    New,
}

/// 后端无关的写终裁结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewWriteOutcome {
    pub state: ViewWriteState,
    pub bytes: u64,
}

/// 暂存内容来源（core `StagedContent` 的后端无关镜像）。
pub enum ViewStaged<'a> {
    /// 完整内容已在内存。
    Memory(&'a [u8]),
    /// 溢写文件（upper 目录内 `.wtfs-tmp-w-*`；句柄已由调用方关闭）。
    Spill(&'a Path),
}

/// lower 条目信息（暂存句柄 stat 投影用）：B1 = 模板条目（blake3 hash
/// 可比），C = manifest 条目（blake3 无可比 hash → `None`；C 无暂存流，
/// 仅为完整性提供）。
#[derive(Debug, Clone, Copy)]
pub struct ViewLowerEntry {
    pub mode: u32,
    pub size: u64,
    pub hash: Option<FileHash>,
}

/// 挂载回调层看到的视图后端（enum 分发）。
#[derive(Clone)]
pub enum ViewBackend {
    Overlay(Arc<Overlay>),
    Cas(CasBackend),
}

/// [`wtfs_cas::Error`] → [`wtfs_core::Error`]（挂载层统一错误面；
/// `CrossDevice` 保形映射——fsp_err 据此回 `STATUS_NOT_SAME_DEVICE`）。
pub fn map_cas_error(e: wtfs_cas::Error) -> wtfs_core::Error {
    match e {
        wtfs_cas::Error::Cas(msg) => wtfs_core::Error::Overlay(format!("cas: {msg}")),
        wtfs_cas::Error::Git(msg) => wtfs_core::Error::Overlay(format!("git: {msg}")),
        wtfs_cas::Error::InvalidPath { path, reason } => {
            wtfs_core::Error::InvalidPath { path, reason }
        }
        wtfs_cas::Error::NotFound(msg) => wtfs_core::Error::NotFound(msg),
        wtfs_cas::Error::CrossDevice(msg) => wtfs_core::Error::CrossDevice(msg),
        wtfs_cas::Error::Io(io) => wtfs_core::Error::Io(io),
    }
}

fn dedupe_state_to_view(state: DedupeState) -> ViewWriteState {
    match state {
        DedupeState::Lower => ViewWriteState::Lower,
        DedupeState::Upper => ViewWriteState::Upper,
        DedupeState::New => ViewWriteState::New,
    }
}

fn cas_stat(st: wtfs_cas::CasStat) -> Stat {
    Stat {
        dev: st.dev,
        ino: st.ino,
        mode: st.mode,
        size: st.size,
        mtime_ns: st.mtime_ns,
        ctime_ns: st.ctime_ns,
    }
}

fn cas_entry_kind(kind: wtfs_cas::EntryKind) -> EntryKind {
    match kind {
        wtfs_cas::EntryKind::File => EntryKind::File,
        wtfs_cas::EntryKind::Directory => EntryKind::Directory,
    }
}

impl ViewBackend {
    /// 推导 stat（whiteout/not-found → `None`）。纯读，零 hydrate。
    pub fn stat(&self, rel: &str) -> wtfs_core::Result<Option<Stat>> {
        match self {
            ViewBackend::Overlay(o) => o.stat(rel),
            ViewBackend::Cas(c) => c
                .view()
                .lock()
                .stat(rel)
                .map(|s| s.map(cas_stat))
                .map_err(map_cas_error),
        }
    }

    /// 解析视图路径（写路径状态机/冲突检查用）。
    pub fn resolve(&self, rel: &str) -> wtfs_core::Result<ViewResolution> {
        match self {
            ViewBackend::Overlay(o) => Ok(match o.resolve(rel)? {
                wtfs_core::Resolution::NotFound => ViewResolution::NotFound,
                wtfs_core::Resolution::Whiteout => ViewResolution::Whiteout,
                wtfs_core::Resolution::Upper { path } => ViewResolution::Upper { path },
                wtfs_core::Resolution::Lower { .. } => ViewResolution::Lower,
                wtfs_core::Resolution::Directory { .. } => ViewResolution::Directory,
            }),
            ViewBackend::Cas(c) => Ok(match c.view().lock().resolve(rel).map_err(map_cas_error)? {
                wtfs_cas::CasResolution::NotFound => ViewResolution::NotFound,
                wtfs_cas::CasResolution::Whiteout => ViewResolution::Whiteout,
                wtfs_cas::CasResolution::Upper { path } => ViewResolution::Upper { path },
                wtfs_cas::CasResolution::CasFile => ViewResolution::Lower,
                wtfs_cas::CasResolution::Directory { .. } => ViewResolution::Directory,
            }),
        }
    }

    /// 目录合并枚举（lightweight；delete-on-close 的非空检查用）。
    pub fn list(&self, dir_rel: &str) -> wtfs_core::Result<Vec<OverlayDirEntry>> {
        match self {
            ViewBackend::Overlay(o) => o.list(dir_rel),
            ViewBackend::Cas(c) => Ok(c
                .view()
                .lock()
                .list(dir_rel)
                .map_err(map_cas_error)?
                .into_iter()
                .map(|e| OverlayDirEntry {
                    name: e.name,
                    rel: e.rel,
                    kind: cas_entry_kind(e.kind),
                })
                .collect()),
        }
    }

    /// 目录合并枚举 + 逐项 stat（read_directory 的口径）。
    pub fn list_stat(&self, dir_rel: &str) -> wtfs_core::Result<Vec<(String, Stat)>> {
        match self {
            ViewBackend::Overlay(o) => o.list_stat(dir_rel),
            ViewBackend::Cas(c) => c
                .view()
                .lock()
                .list_stat(dir_rel)
                .map_err(map_cas_error)
                .map(|v| v.into_iter().map(|(name, st)| (name, cas_stat(st))).collect()),
        }
    }

    /// 可读文件的本地物理路径（upper 真实文件 / hydration 缓存文件 /
    /// 模板树文件）。whiteout/not-found → `None`。
    pub fn source_path(&self, rel: &str) -> wtfs_core::Result<Option<PathBuf>> {
        match self {
            ViewBackend::Overlay(o) => o.source_path(rel),
            ViewBackend::Cas(c) => c
                .view()
                .lock()
                .source_path(rel)
                .map_err(map_cas_error),
        }
    }

    /// checkout 暂存流门控；C 在 absorbing 窗口内为 true（内存吸收 +
    /// manifest oid 比对），窗口外为 false（普通 copy-up/就地写）。
    pub fn is_initializing(&self) -> wtfs_core::Result<bool> {
        match self {
            ViewBackend::Overlay(o) => o.is_initializing(),
            ViewBackend::Cas(c) => Ok(c.absorbing()),
        }
    }

    /// copy-up：lower 内容物化到 upper；已在 upper 返回其路径；
    /// whiteout/not-found → `None`。
    pub fn copy_up(&self, rel: &str) -> wtfs_core::Result<Option<PathBuf>> {
        match self {
            ViewBackend::Overlay(o) => o.copy_up(rel),
            ViewBackend::Cas(c) => c.view().lock().copy_up(rel).map_err(map_cas_error),
        }
    }

    /// upper 物理路径（`""` = upper 根）。
    pub fn upper_abs(&self, rel: &str) -> PathBuf {
        match self {
            ViewBackend::Overlay(o) => o.upper_abs(rel),
            ViewBackend::Cas(c) => {
                if rel.is_empty() {
                    c.worktree_dir().to_path_buf()
                } else {
                    c.worktree_dir().join(rel)
                }
            }
        }
    }

    /// upper 物理变更通知（B1 失效负存在缓存；C 无缓存，no-op）。
    pub fn note_upper_mutation(&self, rel: &str) {
        match self {
            ViewBackend::Overlay(o) => o.note_upper_mutation(),
            ViewBackend::Cas(c) => c.note_upper_mutation(rel),
        }
    }

    /// upper 侧文件物化计数（M9 插桩；C 无对应插桩，no-op）。
    pub fn record_upper_write(&self) {
        if let ViewBackend::Overlay(o) = self {
            o.io_stats().record_upper_write();
        }
    }

    /// 暂存写终裁：B1 按 blake3 对模板 hash 判定；C 按 git blob sha1 对
    /// manifest oid 判定（`hash` 参数仅 B1 使用）。
    pub fn finish_staged(
        &self,
        rel: &str,
        hash: FileHash,
        size: u64,
        content: ViewStaged<'_>,
    ) -> wtfs_core::Result<ViewWriteOutcome> {
        match self {
            ViewBackend::Overlay(o) => {
                let content = match content {
                    ViewStaged::Memory(mem) => wtfs_core::StagedContent::Memory(mem),
                    ViewStaged::Spill(path) => wtfs_core::StagedContent::Spill(path),
                };
                let (state, bytes) = o.finish_staged(rel, hash, size, content)?;
                Ok(ViewWriteOutcome {
                    state: dedupe_state_to_view(state),
                    bytes,
                })
            }
            ViewBackend::Cas(c) => {
                let source = match content {
                    ViewStaged::Memory(mem) => CasStagedSource::Memory(mem),
                    ViewStaged::Spill(path) => CasStagedSource::Spill(path),
                };
                let out = c.view().lock().finish_staged(rel, size, source).map_err(map_cas_error)?;
                Ok(ViewWriteOutcome {
                    state: view_state_from_cas(out.state),
                    bytes: out.bytes,
                })
            }
        }
    }

    /// 就地写 post-hoc 终裁（cleanup/flush）：内容一致 → 回收 upper 回落
    /// lower/manifest。
    pub fn write_from(
        &self,
        rel: &str,
        reader: &mut impl std::io::Read,
    ) -> wtfs_core::Result<ViewWriteOutcome> {
        match self {
            ViewBackend::Overlay(o) => {
                let (state, bytes) = o.write_from(rel, reader)?;
                Ok(ViewWriteOutcome {
                    state: dedupe_state_to_view(state),
                    bytes,
                })
            }
            ViewBackend::Cas(c) => {
                let out = c
                    .view()
                    .lock()
                    .write_from(rel, reader)
                    .map_err(map_cas_error)?;
                Ok(ViewWriteOutcome {
                    state: view_state_from_cas(out.state),
                    bytes: out.bytes,
                })
            }
        }
    }

    /// create 回调后路径立即可解析（B100100）。
    pub fn prepare_created_file(&self, rel: &str) -> wtfs_core::Result<()> {
        match self {
            ViewBackend::Overlay(o) => o.prepare_created_file(rel),
            ViewBackend::Cas(c) => c
                .view()
                .lock()
                .prepare_created_file(rel)
                .map_err(map_cas_error),
        }
    }

    /// 删除：upper 删真实文件；lower 有内容记 whiteout（M12）。
    pub fn remove(&self, rel: &str) -> wtfs_core::Result<bool> {
        match self {
            ViewBackend::Overlay(o) => o.remove(rel),
            ViewBackend::Cas(c) => c.view().lock().remove(rel).map_err(map_cas_error),
        }
    }

    /// 重命名：文件级搬移 + M12 标记；lower 有内容的目录 EXDEV。
    pub fn rename(&self, from: &str, to: &str) -> wtfs_core::Result<()> {
        match self {
            ViewBackend::Overlay(o) => o.rename(from, to),
            ViewBackend::Cas(c) => c.view().lock().rename(from, to).map_err(map_cas_error),
        }
    }

    /// 稳定 file id（Windows FILE_ID 与 index ino 用）。
    pub fn file_id(&self, rel: &str) -> wtfs_core::Result<u64> {
        match self {
            ViewBackend::Overlay(o) => o.file_id(rel),
            ViewBackend::Cas(_) => Ok(wtfs_cas::file_id(rel)),
        }
    }

    /// 固定虚拟卷序列号（lower 固定 stat 的 dev）。
    pub fn volume_serial(&self) -> u32 {
        match self {
            ViewBackend::Overlay(o) => o.config().volume_serial,
            ViewBackend::Cas(c) => c.volume_serial(),
        }
    }

    /// 固定纪元（lower 文件的 mtime/ctime）。
    pub fn fixed_stat_epoch_ns(&self) -> i64 {
        match self {
            ViewBackend::Overlay(o) => o.config().fixed_stat_epoch_ns,
            ViewBackend::Cas(c) => c.fixed_stat_epoch_ns(),
        }
    }

    /// 写路径 dedupe 开关（C 恒开：manifest oid 回查）。
    pub fn dedupe_enabled(&self) -> bool {
        match self {
            ViewBackend::Overlay(o) => o.config().dedupe_enabled,
            ViewBackend::Cas(_) => true,
        }
    }

    /// lower 条目信息（暂存句柄 stat 投影用）。
    pub fn lower_entry(&self, rel: &str) -> Option<ViewLowerEntry> {
        match self {
            ViewBackend::Overlay(o) => o
                .lower()
                .and_then(|m| m.get(rel))
                .map(|t: &TemplateFile| ViewLowerEntry {
                    mode: t.mode,
                    size: t.size,
                    hash: Some(t.hash),
                }),
            ViewBackend::Cas(c) => c.view().lock().manifest().file(rel).map(|f| ViewLowerEntry {
                mode: f.mode,
                size: f.size,
                // blake3 与 git blob sha1 不可比；C 无暂存流（不可达路径）。
                hash: None,
            }),
        }
    }

    /// 暂存句柄的 stat 投影（M11）：内容与 lower 一致 → 固定 lower stat
    /// （index 对齐）；否则报当下时间。仅 B1 checkout 暂存流可达；C 注册
    /// 即 ready，此分支不可达。
    pub fn projected_staged_stat(
        &self,
        rel: &str,
        size: u64,
        hash: Option<FileHash>,
    ) -> wtfs_core::Result<Stat> {
        let o = match self {
            ViewBackend::Overlay(o) => o,
            // C 的 checkout 吸收窗口：暂存内容尚未物化，但句柄 stat 必须
            // 可解析。manifest 同尺寸 → 固定纪元（index 对齐）；否则报
            // 当前时间。C 的暂存哈希是 blake3，无法直接比 manifest 的
            // git blob sha1，这里按尺寸近似。
            ViewBackend::Cas(c) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as i64)
                    .unwrap_or(0);
                let (mode, mtime) = {
                    let view = c.view().lock();
                    match view.manifest().file(rel) {
                        Some(f) if size == f.size => {
                            (f.mode, wtfs_core::FIXED_STAT_EPOCH_UNIX_NS)
                        }
                        Some(f) => (f.mode, now),
                        None => (0o100644, now),
                    }
                };
                let ino = c.view().lock().file_id(rel).map_err(map_cas_error)?;
                return Ok(Stat {
                    dev: u64::from(c.volume_serial()),
                    ino,
                    mode,
                    size,
                    mtime_ns: mtime,
                    ctime_ns: mtime,
                });
            }
        };
        let vs = u64::from(o.config().volume_serial);
        let ino = o.file_id(rel)?;
        let template: Option<TemplateFile> = o.lower().and_then(|m| m.get(rel)).cloned();
        let matches_template = o.config().dedupe_enabled
            && hash.is_some()
            && size == template.as_ref().map_or(0, |t| t.size)
            && template.as_ref().zip(hash).is_some_and(|(t, h)| h == t.hash);
        if matches_template {
            let t = template.as_ref().expect("checked above");
            let epoch = o.config().fixed_stat_epoch_ns;
            return Ok(Stat {
                dev: vs,
                ino,
                mode: t.mode,
                size: t.size,
                mtime_ns: epoch,
                ctime_ns: epoch,
            });
        }
        // 与模板不一致：报当下时间（与 core 的真实 mtime 语义对齐）。
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as i64);
        Ok(Stat {
            dev: vs,
            ino,
            mode: template.map_or(0o100644, |t| t.mode),
            size,
            mtime_ns: now,
            ctime_ns: now,
        })
    }

    /// worktree 根 delete-on-close 的元数据收尾：B1 删 DB 行（级联清子表，
    /// upper 由 gc 回收）；C 无持久元数据行，no-op（标记日志随目录留存，
    /// 重开视图重放仍一致）。
    pub fn root_delete_cleanup(&self) -> wtfs_core::Result<()> {
        match self {
            ViewBackend::Overlay(o) => o.store().unregister(o.worktree_id()),
            ViewBackend::Cas(_) => Ok(()),
        }
    }

    /// B1 overlay 访问（daemon/watcher/测试的既有语义；CAS → `None`）。
    pub fn as_overlay(&self) -> Option<&Arc<Overlay>> {
        match self {
            ViewBackend::Overlay(o) => Some(o),
            ViewBackend::Cas(_) => None,
        }
    }

    /// C 后端访问。
    pub fn as_cas(&self) -> Option<&CasBackend> {
        match self {
            ViewBackend::Overlay(_) => None,
            ViewBackend::Cas(c) => Some(c),
        }
    }
}

fn view_state_from_cas(state: wtfs_cas::WriteState) -> ViewWriteState {
    match state {
        wtfs_cas::WriteState::Lower => ViewWriteState::Lower,
        wtfs_cas::WriteState::Upper => ViewWriteState::Upper,
        wtfs_cas::WriteState::New => ViewWriteState::New,
    }
}
