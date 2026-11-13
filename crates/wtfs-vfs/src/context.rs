//! WinFSP [`FileSystemContext`] 实现：M0 dedupe-only 的目录挂载
//! `<repo>/.tower/worktrees`（spec §4.1/§6.3/§6.5）。
//!
//! 路由：`\` = 挂载根（枚举已注册 worktree）；`\wt-N\...` = 对应
//! [`Overlay`] 内的 repo 相对路径。未注册首组件只有在"创建目录"
//! （`git worktree add` 先 `mkdir`）时经 [`VfsRegistry::auto_register`]
//! 接入，其余一律不存在。
//!
//! initializing 门控（spec §6.3）：worktree 处于 `Initializing` 时，
//! **除 worktree 根目录外的所有按名查找**（security/open/get_file_info/
//! read）都报不存在——`git worktree add` 的"目录必须为空"检查因此通过；
//! 写入/创建路径不受影响，照常进 overlay。
//!
//! 写路径（spec §6.3/§16.2 折衷）：
//!
//! * `Initializing`（checkout 流）：内容先暂存（内存，>32MiB 溢写
//!   `.wtfs-tmp-w-*` 隐藏文件），暂存期流式算好 blake3。Git 在 close 前
//!   `fstat`，[`FileContext`] 投影最终 stat——内容与模板一致时报固定 stat
//!   （与 ready 后 lower 一致，index 对齐），否则报逻辑 mtime。cleanup/flush
//!   时经 [`wtfs_core::Overlay::finish_staged`] 直接终裁（M9/B090100 零拷贝：
//!   一致 → 丢弃暂存回落 lower，零 upper 文件、零 fsync、零临时副本；不同 →
//!   溢写同卷改名 / 内存直写 upper，恰好一次 fsync）。判定逻辑全部复用
//!   M2 合并实现（含祖先 whiteout 守护）。
//! * `Ready`（worker 编辑流）：首次写 copy-up 后直接对 upper 物理 pwrite
//!   （POSIX 可见性）；cleanup 时同样 post-hoc 经 `write_from` 比对回收。
//! * 覆写打开语义（B060100）：`CREATE_ALWAYS`/`TRUNCATE_EXISTING` 有两条
//!   真实路径，都保证最终 EOF 恒等于新内容：
//!   ① 真覆写 disposition（`FILE_OVERWRITE_IF`/`FILE_OVERWRITE`/
//!   `FILE_SUPERSEDE`，git 等）：WinFSP 用户态命中已有文件时只调
//!   [`FileSystemContext::open`]（`CreateOptions` 高 8 位携带 disposition），
//!   [`FileSystemContext::overwrite`] 由内核 best-effort 补发、可能不达，
//!   因此 `open` 打 `truncate_open` 标记：首次写状态建立时 `set_len(0)`
//!   （Lower 目标跳过 copy-up），零写入句柄在 cleanup 兜底清空。
//!   ② Rust std ≥1.7x 对 `CREATE_ALWAYS` 的模拟（rust-lang/rust#115745：
//!   `OPEN_ALWAYS`（=`FILE_OPEN_IF`）+ `FileAllocationInfo(0)` 手动清空）：
//!   [`FileSystemContext::set_file_size`] 对分配大小收缩按 NT 语义截断
//!   EOF（增长/持平仍是预占提示，不改变 EOF）。
//! * 任何一步失败保留 upper 已写内容（正确性优先，spec §6.3.6）。
//!
//! 并发：回调以 `&self` 并发进入（FineGuard 多线程）；文件级状态在
//! [`FileContext`] 内用 `Mutex` 串行化；overlay 元数据由 core 的
//! SQLite/锁保护。同一路径多句柄并发写不在 M0 保证范围（checkout 是
//! 顺序逐文件写）。

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use wtfs_core::{
    FileAttributes, Overlay, OverlayStore, Resolution, StagedContent, Stat, TemplateFile,
    validate_relative_path,
};
use winfsp::FspError;
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileSecurity, FileInfo, FileSystemContext, OpenFileInfo,
    VolumeInfo, WideNameInfo,
};
use winfsp::U16CStr;

use crate::notify::NotifyHub;
use crate::registry::{VfsRegistry, worktree_ino};
use crate::security::{fixed_security_descriptor, fixed_security_descriptor_len};
use crate::win::{
    FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME,
    FILE_ACTION_RENAMED_OLD_NAME, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE,
    allocation_size, err_access_denied, err_collision, err_dir_not_empty,
    err_invalid_name, err_invalid_parameter, err_is_directory, err_no_more_files,
    err_not_a_directory, err_not_found, filetime_from_unix_ns, fsp_err, is_overwrite_disposition,
    mount_path, split_first_component,
};

/// 暂存溢写阈值：超过后暂存内容落到 upper 目录内的 `.wtfs-tmp-w-*`
/// 隐藏文件（core 对该前缀隐藏/拒绝写，视为临时文件）。
pub(crate) const STAGE_SPILL_BYTES: usize = 32 * 1024 * 1024;

/// 挂载根的稳定 ino。
const MOUNT_ROOT_INO: u64 = 1;

/// 暂存溢写文件序号（同进程唯一）。
static SPILL_SEQ: AtomicU64 = AtomicU64::new(0);

/// 路由结果。
enum Route {
    /// 挂载根 `\`。
    Root,
    /// 已注册 worktree + repo 相对路径（`""` = worktree 根）。
    Worktree {
        name: String,
        overlay: Arc<Overlay>,
        rel: String,
    },
    /// 未注册首组件。
    Unknown { name: String },
}

/// VFS 文件系统上下文（挂载一份）。
pub struct WtfsContext {
    registry: Arc<VfsRegistry>,
    notify: NotifyHub,
}

impl WtfsContext {
    pub fn new(registry: Arc<VfsRegistry>) -> Self {
        Self {
            registry,
            notify: NotifyHub::new(),
        }
    }

    pub fn registry(&self) -> &Arc<VfsRegistry> {
        &self.registry
    }

    pub fn notify_hub(&self) -> &NotifyHub {
        &self.notify
    }

    fn route(&self, file_name: &U16CStr) -> Result<Route, FspError> {
        let Some((name, rel)) = split_first_component(file_name) else {
            return Ok(Route::Root);
        };
        match self.registry.lookup(&name) {
            Some((canonical, overlay)) => {
                if !rel.is_empty() {
                    validate_relative_path(&rel).map_err(|_| err_invalid_name())?;
                }
                Ok(Route::Worktree {
                    name: canonical,
                    overlay,
                    rel,
                })
            }
            None => Ok(Route::Unknown { name }),
        }
    }

    /// 通知事件（挂载内绝对路径）；同时给父目录补一条内容变化。
    fn notify_change(&self, worktree: &str, rel: &str, action: u32) {
        self.notify.push(&mount_path(worktree, rel), action);
        if let Some((parent, _)) = rel.rsplit_once('/') {
            self.notify.push(&mount_path(worktree, parent), FILE_ACTION_MODIFIED);
        } else if !rel.is_empty() {
            self.notify.push(&mount_path(worktree, ""), FILE_ACTION_MODIFIED);
        }
    }
}

/// 打开句柄的写状态。
enum WriteState {
    /// initializing 暂存写：内容在内存/溢写文件，cleanup/flush 终裁。
    Staged(StagedWrite),
    /// ready 就地写：copy-up 后直接对 upper 物理 pwrite。
    InPlace { file: File, size: u64 },
}

struct StagedWrite {
    mem: Vec<u8>,
    spill: Option<Spill>,
    size: u64,
    hasher: blake3::Hasher,
    /// `true` = 增量哈希有效（全程 append）；乱序写后需整段重算。
    hasher_valid: bool,
}

struct Spill {
    path: std::path::PathBuf,
    file: File,
}

/// 打开的文件/目录句柄（WinFSP FileContext）。
pub struct FileContext {
    /// worktree 名；`None` = 挂载根。
    route_name: Option<String>,
    overlay: Option<Arc<Overlay>>,
    /// overlay 内相对路径（挂载根/worktree 根为 `""`）。
    rel: String,
    is_dir: bool,
    dir_buffer: DirBuffer,
    write: Mutex<Option<WriteState>>,
    delete_pending: AtomicBool,
    /// 覆写类 disposition 打开（CREATE_ALWAYS/TRUNCATE_EXISTING，B060100）：
    /// EOF 必须收缩到新内容——首次写状态建立时清空；零写入句柄 cleanup 兜底。
    truncate_open: AtomicBool,
}

impl Drop for FileContext {
    fn drop(&mut self) {
        // best-effort：句柄异常路径下清理溢写文件（正常流程 cleanup 已终裁）。
        if let Ok(mut slot) = self.write.try_lock() {
            if let Some(WriteState::Staged(staged)) = slot.take() {
                discard_staged(staged);
            }
        }
    }
}

fn discard_staged(staged: StagedWrite) {
    if let Some(spill) = staged.spill {
        drop(spill.file);
        let _ = std::fs::remove_file(&spill.path);
    }
}

impl FileContext {
    fn dir(route_name: Option<String>, overlay: Option<Arc<Overlay>>, rel: String) -> Self {
        Self {
            route_name,
            overlay,
            rel,
            is_dir: true,
            dir_buffer: DirBuffer::new(),
            write: Mutex::new(None),
            delete_pending: AtomicBool::new(false),
            truncate_open: AtomicBool::new(false),
        }
    }

    fn file(route_name: Option<String>, overlay: Arc<Overlay>, rel: String) -> Self {
        Self {
            route_name,
            overlay: Some(overlay),
            rel,
            is_dir: false,
            dir_buffer: DirBuffer::new(),
            write: Mutex::new(None),
            delete_pending: AtomicBool::new(false),
            truncate_open: AtomicBool::new(false),
        }
    }

    /// 在 `upper_abs(rel)` 建立全新空 upper 文件的写状态（新建/whiteout 重建/
    /// 覆写跳过 copy-up 共用；P1-1 祖先 whiteout 时同样按新文件落 upper）。
    fn open_fresh_upper(
        overlay: &Overlay,
        rel: &str,
    ) -> Result<WriteState, FspError> {
        let path = overlay.upper_abs(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(FspError::from)?;
        }
        let file = open_rw_create(&path)?;
        overlay.io_stats().record_upper_write();
        Ok(WriteState::InPlace { file, size: 0 })
    }

    /// 句柄视角的当前 EOF：写状态优先；覆写类打开（尚未写入）按 0 报告
    /// （CREATE_ALWAYS 打开即清空，物理收缩随首次写/cleanup 落地）。
    fn current_eof(&self) -> Result<u64, FspError> {
        {
            let slot = self.write.lock().map_err(|_| err_invalid_parameter())?;
            match slot.as_ref() {
                Some(WriteState::Staged(staged)) => return Ok(staged.size),
                Some(WriteState::InPlace { size, .. }) => return Ok(*size),
                None => {}
            }
        }
        if self.truncate_open.load(Ordering::SeqCst) {
            return Ok(0);
        }
        let overlay = self.overlay.as_ref().ok_or_else(err_access_denied)?;
        Ok(overlay
            .stat(&self.rel)
            .map_err(fsp_err)?
            .map(|st| st.size)
            .unwrap_or(0))
    }

    /// 首次写操作时建立写状态（spec §6.5 copy-up / §16.2 暂存策略）。
    fn ensure_write_state(&self) -> Result<std::sync::MutexGuard<'_, Option<WriteState>>, FspError> {
        let overlay = self.overlay.as_ref().ok_or_else(err_access_denied)?;
        let mut slot = self.write.lock().map_err(|_| err_invalid_parameter())?;
        if slot.is_none() {
            let rel = self.rel.as_str();
            let initializing = overlay.is_initializing().map_err(fsp_err)?;
            *slot = Some(if initializing {
                // 暂存写从空内容开始：覆写打开语义天然成立（终裁整段替换）。
                WriteState::Staged(StagedWrite {
                    mem: Vec::new(),
                    spill: None,
                    size: 0,
                    hasher: blake3::Hasher::new(),
                    hasher_valid: true,
                })
            } else {
                // 覆写类 disposition（CREATE_ALWAYS 等，B060100）：EOF 收缩到
                // 新内容——upper 直接清空；lower 跳过 copy-up（内容将被整体
                // 替换），终裁 write_from 的 dedupe 比对语义不变。
                let truncate = self.truncate_open.load(Ordering::SeqCst);
                match overlay.resolve(rel).map_err(fsp_err)? {
                    Resolution::Upper { path } => {
                        let mut size = std::fs::metadata(&path).map_err(FspError::from)?.len();
                        let mut file = open_rw(&path)?;
                        if truncate {
                            file.set_len(0).map_err(FspError::from)?;
                            file.seek(SeekFrom::Start(0)).map_err(FspError::from)?;
                            size = 0;
                        }
                        WriteState::InPlace { file, size }
                    }
                    Resolution::Lower { .. } if truncate => {
                        Self::open_fresh_upper(overlay, rel)?
                    }
                    Resolution::Lower { .. } => {
                        // copy-up；被祖先 whiteout 挡住（P1-1）时按新文件落 upper。
                        let copied = overlay.copy_up(rel).map_err(fsp_err)?;
                        let path = copied.unwrap_or_else(|| overlay.upper_abs(rel));
                        let size = std::fs::metadata(&path)
                            .map(|m| m.len())
                            .unwrap_or(0);
                        let file = open_rw_create(&path)?;
                        WriteState::InPlace { file, size }
                    }
                    Resolution::Whiteout | Resolution::NotFound => {
                        Self::open_fresh_upper(overlay, rel)?
                    }
                    Resolution::Directory { .. } => return Err(err_is_directory()),
                }
            });
        }
        Ok(slot)
    }

    /// 暂存内容哈希（增量有效用增量；否则整段重算）。溢写读取失败返回 IO 错误
    /// （终裁路径据此报错，投影 stat 路径按 `.ok()` 降级为不匹配）。
    fn staged_hash(staged: &StagedWrite) -> std::io::Result<wtfs_core::FileHash> {
        if staged.hasher_valid {
            return Ok(wtfs_core::FileHash(
                *staged.hasher.clone().finalize().as_bytes(),
            ));
        }
        match &staged.spill {
            Some(spill) => match wtfs_core::hash_file(&spill.path) {
                Ok((h, _)) => Ok(h),
                Err(wtfs_core::Error::Io(e)) => Err(e),
                Err(other) => Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    other.to_string(),
                )),
            },
            None => Ok(wtfs_core::hash_bytes(&staged.mem)),
        }
    }

    /// 投影 stat（暂存句柄的 get_file_info）：内容与模板一致 → 固定 lower
    /// stat（与 ready 后 core 报告一致，index 对齐）；否则逻辑 mtime。
    fn projected_stat(&self, overlay: &Overlay, staged: &StagedWrite) -> Result<Stat, FspError> {
        let rel = self.rel.as_str();
        let vs = u64::from(overlay.config().volume_serial);
        let ino = overlay.file_id(rel).map_err(fsp_err)?;
        let template: Option<TemplateFile> = overlay.lower().and_then(|m| m.get(rel)).cloned();
        let matches_template = overlay.config().dedupe_enabled
            && staged.hasher_valid
            && staged.size == template.as_ref().map_or(0, |t| t.size)
            && template
                .as_ref()
                .zip(Self::staged_hash(staged).ok())
                .is_some_and(|(t, h)| h == t.hash);
        if matches_template {
            let t = template.as_ref().expect("checked above");
            return Ok(Stat::fixed_lower(vs, ino, t.mode, t.size));
        }
        // 与模板不一致：报逻辑 mtime（与 core 的发放时钟对齐；只读投影不持久，
        // 不一致场景 git 在 status 时回退全量哈希，正确性不受影响）。
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as i64);
        let last = overlay
            .store()
            .max_stat_mtime(overlay.worktree_id())
            .map_err(fsp_err)?;
        let mtime = now.max(last.unwrap_or(0) + 1);
        Ok(Stat {
            dev: vs,
            ino,
            mode: template.map_or(0o100644, |t| t.mode),
            size: staged.size,
            mtime_ns: mtime,
            ctime_ns: mtime,
        })
    }

    /// cleanup/flush 终裁：暂存内容经 [`Overlay::finish_staged`] 判定
    /// dedupe（M9/B090100 零拷贝：一致 → 丢弃暂存回落 lower，零 upper/零
    /// fsync；不同/新 → 溢写改名 / 内存直写 upper + 恰好一次 fsync）；
    /// 就地写（ready 编辑流）保持 [`Overlay::write_from`] post-hoc 比对。
    /// 失败时保留 upper 已写内容（全量 copy 兜底）。
    fn finalize_write(&self) -> Result<(), FspError> {
        let overlay = self.overlay.clone().ok_or_else(err_access_denied)?;
        let rel = self.rel.clone();
        let mut slot = self.write.lock().map_err(|_| err_invalid_parameter())?;
        let Some(state) = slot.take() else {
            return Ok(());
        };
        match state {
            WriteState::Staged(staged) => Self::finalize_staged(&overlay, &rel, staged),
            WriteState::InPlace { file, size: _ } => {
                // 关句柄再重开：Windows 上 rename 覆盖打开中的文件会失败。
                drop(file);
                let path = overlay.upper_abs(&rel);
                match File::open(&path) {
                    // upper 已被旁路删除：视为无内容可终裁。
                    Err(_) => Ok(()),
                    Ok(mut f) => overlay.write_from(&rel, &mut f).map(|_| ()).map_err(fsp_err),
                }
            }
        }
    }

    /// 暂存终裁（M9/B090100）：暂存期算好的哈希/长度直接交
    /// [`Overlay::finish_staged`]，不再把内容重写进临时副本。溢写句柄先关
    /// （Windows rename 前置条件）；失败时 best-effort 清理溢写文件，
    /// upper 既有内容不受影响。
    fn finalize_staged(overlay: &Overlay, rel: &str, staged: StagedWrite) -> Result<(), FspError> {
        let hash = Self::staged_hash(&staged).map_err(FspError::from)?;
        let size = staged.size;
        let StagedWrite { mem, spill, .. } = staged;
        let spill_path = spill.as_ref().map(|s| s.path.clone());
        drop(spill);
        let content = match spill_path.as_deref() {
            Some(path) => StagedContent::Spill(path),
            None => StagedContent::Memory(mem.as_slice()),
        };
        let result = overlay
            .finish_staged(rel, hash, size, content)
            .map(|_| ())
            .map_err(fsp_err);
        if let Some(path) = &spill_path {
            let _ = std::fs::remove_file(path);
        }
        result
    }

    /// 读句柄当前内容（暂存 → 内存/溢写；就地/只读 → overlay 解析的源文件）。
    fn read_into(&self, buffer: &mut [u8], offset: u64) -> Result<u32, FspError> {
        if self.is_dir {
            return Err(err_access_denied());
        }
        {
            let mut slot = self.write.lock().map_err(|_| err_invalid_parameter())?;
            if let Some(WriteState::Staged(staged)) = slot.as_mut() {
                if offset >= staged.size {
                    return Ok(0);
                }
                let end = (offset + buffer.len() as u64).min(staged.size);
                let len = (end - offset) as usize;
                match &mut staged.spill {
                    Some(spill) => {
                        spill.file.seek(SeekFrom::Start(offset)).map_err(FspError::from)?;
                        spill.file.read_exact(&mut buffer[..len]).map_err(FspError::from)?;
                    }
                    None => {
                        buffer[..len].copy_from_slice(&staged.mem[offset as usize..end as usize]);
                    }
                }
                return Ok(len as u32);
            }
        }
        let overlay = self.overlay.as_ref().ok_or_else(err_access_denied)?;
        let Some(source) = overlay.source_path(&self.rel).map_err(fsp_err)? else {
            return Err(err_not_found());
        };
        let md = std::fs::metadata(&source).map_err(FspError::from)?;
        if offset >= md.len() {
            return Ok(0);
        }
        let end = (offset + buffer.len() as u64).min(md.len());
        let len = (end - offset) as usize;
        let mut file = File::open(&source).map_err(FspError::from)?;
        file.seek(SeekFrom::Start(offset)).map_err(FspError::from)?;
        file.read_exact(&mut buffer[..len]).map_err(FspError::from)?;
        Ok(len as u32)
    }

    /// 句柄 stat：暂存 → 投影；其余 → core 逻辑 stat。
    fn handle_stat(&self) -> Result<Stat, FspError> {
        let overlay = self.overlay.as_ref().ok_or_else(err_access_denied)?;
        {
            let slot = self.write.lock().map_err(|_| err_invalid_parameter())?;
            if let Some(WriteState::Staged(staged)) = slot.as_ref() {
                return self.projected_stat(overlay, staged);
            }
        }
        overlay.stat(&self.rel).map_err(fsp_err)?.ok_or_else(err_not_found)
    }
}

fn open_rw(path: &std::path::Path) -> Result<File, FspError> {
    OpenOptions::new().read(true).write(true).open(path).map_err(FspError::from)
}

fn open_rw_create(path: &std::path::Path) -> Result<File, FspError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(FspError::from)
}

/// [`Stat`] → WinFSP [`FileInfo`]（时间戳映射：mtime=写/访问，ctime=创建/变更）。
fn fill_file_info(info: &mut FileInfo, st: &Stat) {
    info.file_attributes = wtfs_core::attributes_for(st).bits();
    info.reparse_tag = 0;
    info.allocation_size = allocation_size(st.size);
    info.file_size = st.size;
    info.creation_time = filetime_from_unix_ns(st.ctime_ns);
    info.last_access_time = filetime_from_unix_ns(st.mtime_ns);
    info.last_write_time = filetime_from_unix_ns(st.mtime_ns);
    info.change_time = filetime_from_unix_ns(st.ctime_ns);
    info.index_number = st.ino;
    info.hard_links = 0;
    info.ea_size = 0;
}

/// 挂载根的 stat（固定目录值）。
fn mount_root_stat() -> Stat {
    Stat {
        dev: u64::from(wtfs_core::DEFAULT_VOLUME_SERIAL),
        ino: MOUNT_ROOT_INO,
        mode: 0o040755,
        size: 0,
        mtime_ns: wtfs_core::FIXED_STAT_EPOCH_UNIX_NS,
        ctime_ns: wtfs_core::FIXED_STAT_EPOCH_UNIX_NS,
    }
}

/// 大小写不敏感的 `*`/`?` 通配符匹配（read_directory pattern 兜底；
/// WinFSP 默认不传 pattern）。
pub(crate) fn wildcard_match(pattern: &str, name: &str) -> bool {
    let (p, n) = (pattern.to_ascii_lowercase(), name.to_ascii_lowercase());
    fn inner(p: &[u8], n: &[u8]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some(b'*'), _) => (0..=n.len()).any(|skip| inner(&p[1..], &n[skip..])),
            (Some(b'?'), Some(_)) => inner(&p[1..], &n[1..]),
            (Some(&c), Some(&d)) => c == d && inner(&p[1..], &n[1..]),
            _ => false,
        }
    }
    inner(p.as_bytes(), n.as_bytes())
}

/// FILETIME → Unix ns（set_basic_info 回写）。
fn unix_ns_from_filetime(ft: u64) -> i64 {
    (ft as i64 - crate::win::UNIX_TO_FILETIME_TICKS).saturating_mul(100)
}

fn fill_dir_stat(info: &mut FileInfo, base: &Stat) {
    info.file_attributes = FileAttributes::DIRECTORY.bits();
    info.reparse_tag = 0;
    info.file_size = 0;
    info.allocation_size = 0;
    let ft = filetime_from_unix_ns(base.mtime_ns);
    info.creation_time = ft;
    info.last_write_time = ft;
    info.change_time = ft;
    info.last_access_time = ft;
    info.index_number = base.ino;
    info.hard_links = 0;
    info.ea_size = 0;
}

impl FileSystemContext for WtfsContext {
    type FileContext = FileContext;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let sd = fixed_security_descriptor().ok_or_else(err_invalid_parameter)?;
        let sd_len = fixed_security_descriptor_len();
        let attributes = match self.route(file_name)? {
            Route::Root => FileAttributes::DIRECTORY,
            Route::Unknown { .. } => return Err(err_not_found()),
            Route::Worktree { overlay, rel, .. } => {
                match overlay.stat(&rel).map_err(fsp_err)? {
                    Some(st) => wtfs_core::attributes_for(&st),
                    None => return Err(err_not_found()),
                }
            }
        };
        if let Some(buf) = security_descriptor {
            copy_sd(sd, buf)?;
        }
        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: sd_len,
            attributes: attributes.bits(),
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> Result<Self::FileContext, FspError> {
        let _ = granted_access;
        let dir_requested = create_options & FILE_DIRECTORY_FILE != 0;
        let non_dir_requested = create_options & FILE_NON_DIRECTORY_FILE != 0;
        match self.route(file_name)? {
            Route::Root => {
                if non_dir_requested {
                    return Err(err_not_a_directory());
                }
                fill_file_info(file_info.as_mut(), &mount_root_stat());
                Ok(FileContext::dir(None, None, String::new()))
            }
            Route::Unknown { .. } => Err(err_not_found()),
            Route::Worktree { name, overlay, rel } => {
                if rel.is_empty() {
                    // worktree 根：initializing 期间也可开目录句柄（git 查空目录）。
                    if non_dir_requested {
                        return Err(err_not_a_directory());
                    }
                    let st = overlay.stat("").map_err(fsp_err)?.unwrap_or_else(|| Stat {
                        dev: u64::from(overlay.config().volume_serial),
                        ino: worktree_ino(&name),
                        mode: 0o040755,
                        size: 0,
                        mtime_ns: wtfs_core::FIXED_STAT_EPOCH_UNIX_NS,
                        ctime_ns: wtfs_core::FIXED_STAT_EPOCH_UNIX_NS,
                    });
                    fill_file_info(file_info.as_mut(), &st);
                    return Ok(FileContext::dir(Some(name), Some(overlay), rel));
                }
                let st = overlay.stat(&rel).map_err(fsp_err)?.ok_or_else(err_not_found)?;
                let is_dir = st.mode & 0o170000 == 0o040000;
                if dir_requested && !is_dir {
                    return Err(err_not_a_directory());
                }
                if non_dir_requested && is_dir {
                    return Err(err_is_directory());
                }
                fill_file_info(file_info.as_mut(), &st);
                if is_dir {
                    Ok(FileContext::dir(Some(name), Some(overlay), rel))
                } else {
                    let fctx = FileContext::file(Some(name), overlay, rel);
                    // 覆写类 disposition（B060100）：WinFSP 用户态对覆写打开
                    // 只调 Open（Overwrite op 由内核 best-effort 补发、可能
                    // 不达），EOF 收缩语义由标记在首次写/cleanup 落地。
                    if is_overwrite_disposition(create_options) {
                        fctx.truncate_open.store(true, Ordering::SeqCst);
                    }
                    Ok(fctx)
                }
            }
        }
    }

    fn close(&self, _context: Self::FileContext) {
        // Drop 负责 best-effort 清理；正常流程 cleanup 已终裁。
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: u32,
        _file_attributes: u32,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> Result<Self::FileContext, FspError> {
        let is_dir = create_options & FILE_DIRECTORY_FILE != 0;
        match self.route(file_name)? {
            Route::Root => return Err(err_access_denied()),
            Route::Unknown { name } => {
                // 挂载根只接受"创建 worktree 目录"（git worktree add 的 mkdir）。
                if !is_dir {
                    return Err(err_access_denied());
                }
                let overlay = self.registry.auto_register(&name).map_err(fsp_err)?;
                let st = overlay
                    .stat("")
                    .map_err(fsp_err)?
                    .unwrap_or_else(|| Stat {
                        dev: u64::from(overlay.config().volume_serial),
                        ino: worktree_ino(&name),
                        mode: 0o040755,
                        size: 0,
                        mtime_ns: wtfs_core::FIXED_STAT_EPOCH_UNIX_NS,
                        ctime_ns: wtfs_core::FIXED_STAT_EPOCH_UNIX_NS,
                    });
                fill_file_info(file_info.as_mut(), &st);
                self.notify.push(&mount_path(&name, ""), FILE_ACTION_ADDED);
                return Ok(FileContext::dir(Some(name), Some(overlay), String::new()));
            }
            Route::Worktree { name, overlay, rel } => {
                let initializing = overlay.is_initializing().map_err(fsp_err)?;
                if rel.is_empty() {
                    return Err(err_access_denied());
                }
                if is_dir {
                    if !initializing
                        && matches!(
                            overlay.resolve(&rel).map_err(fsp_err)?,
                            Resolution::Upper { .. }
                                | Resolution::Lower { .. }
                                | Resolution::Directory { .. }
                        )
                    {
                        return Err(err_collision());
                    }
                    std::fs::create_dir_all(overlay.upper_abs(&rel)).map_err(FspError::from)?;
                    let st = overlay
                        .stat(&rel)
                        .map_err(fsp_err)?
                        .unwrap_or_else(|| Stat {
                            dev: u64::from(overlay.config().volume_serial),
                            ino: overlay.file_id(&rel).map_err(fsp_err).unwrap_or(0),
                            mode: 0o040755,
                            size: 0,
                            mtime_ns: wtfs_core::FIXED_STAT_EPOCH_UNIX_NS,
                            ctime_ns: wtfs_core::FIXED_STAT_EPOCH_UNIX_NS,
                        });
                    fill_file_info(file_info.as_mut(), &st);
                    self.notify_change(&name, &rel, FILE_ACTION_ADDED);
                    return Ok(FileContext::dir(Some(name), Some(overlay), rel));
                }
                if !initializing
                    && matches!(
                        overlay.resolve(&rel).map_err(fsp_err)?,
                        Resolution::Upper { .. }
                            | Resolution::Lower { .. }
                            | Resolution::Directory { .. }
                    )
                {
                    // whiteout 目标可重建（POSIX 语义）；真实存在才冲突。
                    return Err(err_collision());
                }
                let st = Stat {
                    dev: u64::from(overlay.config().volume_serial),
                    ino: overlay.file_id(&rel).map_err(fsp_err)?,
                    mode: 0o100644,
                    size: 0,
                    mtime_ns: wtfs_core::FIXED_STAT_EPOCH_UNIX_NS,
                    ctime_ns: wtfs_core::FIXED_STAT_EPOCH_UNIX_NS,
                };
                fill_file_info(file_info.as_mut(), &st);
                self.notify_change(&name, &rel, FILE_ACTION_ADDED);
                return Ok(FileContext::file(Some(name), overlay, rel));
            }
        }
    }

    fn overwrite(
        &self,
        context: &Self::FileContext,
        _file_attributes: u32,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> Result<(), FspError> {
        if context.is_dir {
            return Err(err_is_directory());
        }
        let name = context.route_name.clone().unwrap_or_default();
        {
            let mut slot = context.ensure_write_state()?;
            match slot.as_mut().expect("ensure_write_state initializes") {
                WriteState::Staged(staged) => {
                    staged.mem.clear();
                    staged.size = 0;
                    staged.hasher = blake3::Hasher::new();
                    staged.hasher_valid = true;
                    if let Some(spill) = staged.spill.take() {
                        drop(spill.file);
                        let _ = std::fs::remove_file(&spill.path);
                    }
                }
                WriteState::InPlace { file, size } => {
                    file.set_len(0).map_err(FspError::from)?;
                    file.seek(SeekFrom::Start(0)).map_err(FspError::from)?;
                    *size = 0;
                }
            }
        }
        let st = context.handle_stat()?;
        fill_file_info(file_info, &st);
        self.notify_change(&name, &context.rel, FILE_ACTION_MODIFIED);
        Ok(())
    }

    fn cleanup(&self, context: &Self::FileContext, _file_name: Option<&U16CStr>, flags: u32) {
        let delete = winfsp::constants::FspCleanupFlags::FspCleanupDelete.is_flagged(flags)
            || context.delete_pending.load(Ordering::SeqCst);
        // 挂载根：删除由 daemon/gc 负责。
        let overlay = match context.overlay.clone() {
            Some(overlay) => overlay,
            None => return,
        };
        let name = context.route_name.clone().unwrap_or_default();
        if context.rel.is_empty() {
            // worktree 根 delete-on-close（`git worktree remove` 的最后一步
            // rmdir，B060100）：与 daemon watcher 的元数据消失路径完全一致——
            // 删 DB 行（级联清子表）+ 摘路由；upper 目录随后变孤儿，由
            // `wtfs gc` 回收（spec §6.6）。
            if delete {
                if let Err(e) = overlay.store().unregister(overlay.worktree_id()) {
                    tracing::warn!(worktree = %name, error = %e, "failed to unregister row");
                }
                self.registry.unregister(&name);
                self.notify.push(&mount_path(&name, ""), FILE_ACTION_REMOVED);
            }
            return;
        }
        let rel = context.rel.clone();

        let mut slot = match context.write.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        let state = slot.take();
        drop(slot);
        match state {
            Some(WriteState::Staged(staged)) => {
                if delete {
                    // 从未可见：直接丢弃暂存。
                    discard_staged(staged);
                    return;
                }
                match FileContext::finalize_staged(&overlay, &rel, staged) {
                    Ok(()) => self.notify_change(&name, &rel, FILE_ACTION_MODIFIED),
                    Err(e) => tracing::warn!(
                        rel = %rel,
                        error = %e,
                        "staged finalize failed; upper content preserved"
                    ),
                }
            }
            Some(WriteState::InPlace { file, size: _ }) => {
                drop(file);
                if delete {
                    match overlay.remove(&rel) {
                        Ok(_) => self.notify_change(&name, &rel, FILE_ACTION_REMOVED),
                        Err(e) => tracing::warn!(rel = %rel, error = %e, "delete failed"),
                    }
                    return;
                }
                // post-hoc dedupe：内容一致 → 回收 upper 回落 lower。
                let path = overlay.upper_abs(&rel);
                let result = match File::open(&path) {
                    Err(_) => Ok(()), // upper 已被旁路删除
                    Ok(mut f) => overlay.write_from(&rel, &mut f).map(|_| ()).map_err(fsp_err),
                };
                match result {
                    Ok(()) => self.notify_change(&name, &rel, FILE_ACTION_MODIFIED),
                    Err(e) => tracing::warn!(
                        rel = %rel,
                        error = %e,
                        "finalize failed; upper content preserved"
                    ),
                }
            }
            None => {
                if delete {
                    match overlay.remove(&rel) {
                        Ok(_) => self.notify_change(&name, &rel, FILE_ACTION_REMOVED),
                        Err(e) => tracing::warn!(rel = %rel, error = %e, "delete failed"),
                    }
                } else if context.truncate_open.load(Ordering::SeqCst) {
                    // 覆写类打开后零写入（B060100 兜底）：Windows CREATE_ALWAYS
                    // 语义 = 打开即清空。走 finish_staged 终裁：与模板一致的空
                    // 内容回落 lower，否则落空 upper（git 空文件 blob 路径）。
                    let result = overlay
                        .finish_staged(
                            &rel,
                            wtfs_core::hash_bytes(&[]),
                            0,
                            StagedContent::Memory(&[]),
                        )
                        .map(|_| ())
                        .map_err(fsp_err);
                    match result {
                        Ok(()) => self.notify_change(&name, &rel, FILE_ACTION_MODIFIED),
                        Err(e) => tracing::warn!(
                            rel = %rel,
                            error = %e,
                            "zero-write overwrite finalize failed; content preserved"
                        ),
                    }
                }
            }
        }
    }

    fn flush(
        &self,
        context: Option<&Self::FileContext>,
        file_info: &mut FileInfo,
    ) -> Result<(), FspError> {
        let Some(context) = context else {
            // 卷级 flush：M0 无全局脏状态（SQLite WAL 自管理）。
            return Ok(());
        };
        if context.overlay.is_none() {
            fill_file_info(file_info, &mount_root_stat());
            return Ok(());
        }
        if !context.is_dir {
            context.finalize_write()?;
        }
        let st = context.handle_stat()?;
        fill_file_info(file_info, &st);
        Ok(())
    }

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FileInfo,
    ) -> Result<(), FspError> {
        let st = if context.overlay.is_none() {
            mount_root_stat()
        } else {
            context.handle_stat()?
        };
        fill_file_info(file_info, &st);
        Ok(())
    }

    fn get_security(
        &self,
        _context: &Self::FileContext,
        security_descriptor: Option<&mut [c_void]>,
    ) -> Result<u64, FspError> {
        let sd = fixed_security_descriptor().ok_or_else(err_invalid_parameter)?;
        if let Some(buf) = security_descriptor {
            copy_sd(sd, buf)?;
        }
        Ok(fixed_security_descriptor_len())
    }

    fn read(
        &self,
        context: &Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> Result<u32, FspError> {
        context.read_into(buffer, offset)
    }

    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> Result<u32, FspError> {
        if context.is_dir {
            return Err(err_access_denied());
        }
        let name = context.route_name.clone().unwrap_or_default();
        if constrained_io {
            // 受限写：不得越过当前 EOF（cache manager 语义）。
            let size_now = context.current_eof()?;
            if offset >= size_now {
                return Ok(0);
            }
            let clip = size_now.saturating_sub(offset).min(buffer.len() as u64) as usize;
            return self.write_chunk(context, &name, &buffer[..clip], offset, file_info);
        }
        let offset = if write_to_eof {
            context.current_eof()?
        } else {
            offset
        };
        self.write_chunk(context, &name, buffer, offset, file_info)
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        pattern: Option<&U16CStr>,
        marker: DirMarker<'_>,
        buffer: &mut [u8],
    ) -> Result<u32, FspError> {
        let pattern = pattern
            .and_then(|p| p.to_string().ok())
            .map(|s| s.trim_end_matches('\\').to_string());
        let reset = marker.is_none();
        let mut entries: Vec<(String, Stat)> = Vec::new();
        let self_stat;
        if context.overlay.is_none() {
            // 挂载根：枚举已注册 worktree。
            self_stat = mount_root_stat();
            for name in self.registry.names() {
                entries.push((
                    name.clone(),
                    Stat {
                        dev: u64::from(self.registry.config().volume_serial),
                        ino: worktree_ino(&name),
                        mode: 0o040755,
                        size: 0,
                        mtime_ns: wtfs_core::FIXED_STAT_EPOCH_UNIX_NS,
                        ctime_ns: wtfs_core::FIXED_STAT_EPOCH_UNIX_NS,
                    },
                ));
            }
        } else {
            let overlay = context.overlay.as_ref().ok_or_else(err_access_denied)?;
            let rel = context.rel.as_str();
            self_stat = overlay.stat(rel).map_err(fsp_err)?.ok_or_else(err_not_found)?;
            for entry in overlay.list(rel).map_err(fsp_err)? {
                let Some(st) = overlay.stat(&entry.rel).map_err(fsp_err)? else {
                    continue;
                };
                entries.push((entry.name, st));
            }
        }
        if let Some(pattern) = &pattern {
            entries.retain(|(name, _)| wildcard_match(pattern, name));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        if reset {
            let estimate = entries.len() as u32 * 160 + 1024;
            let lock = context.dir_buffer.acquire(true, Some(estimate))?;
                for dot in [".", ".."] {
                    let mut di = DirInfo::<255>::new();
                    fill_dir_stat(di.file_info_mut(), &self_stat);
                    di.set_name(dot)?;
                    lock.write(&mut di)?;
                }
                for (name, st) in entries {
                    let mut di = DirInfo::<255>::new();
                    fill_file_info(di.file_info_mut(), &st);
                    di.set_name(&name)?;
                    lock.write(&mut di)?;
                }
            drop(lock);
        }
        let read = context.dir_buffer.read(marker, buffer);
        if read == 0 {
            return Err(err_no_more_files());
        }
        Ok(read)
    }

    fn rename(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> Result<(), FspError> {
        if context.overlay.is_none() {
            return Err(err_access_denied());
        }
        let overlay = context.overlay.clone().expect("checked above");
        let rel = context.rel.clone();
        let name = context.route_name.clone().ok_or_else(err_invalid_parameter)?;
        let Route::Worktree {
            name: new_name,
            overlay: _target_overlay,
            rel: new_rel,
        } = self.route(new_file_name)?
        else {
            return Err(err_access_denied());
        };
        if new_name != name {
            return Err(err_invalid_parameter());
        }
        if new_rel.is_empty() {
            return Err(err_access_denied());
        }
        if !replace_if_exists {
            let exists = !matches!(
                overlay.resolve(&new_rel).map_err(fsp_err)?,
                Resolution::NotFound | Resolution::Whiteout
            );
            if exists {
                return Err(err_collision());
            }
        }
        // 源句柄可能有暂存内容：先终裁再重命名。
        context.finalize_write()?;
        overlay.rename(&rel, &new_rel).map_err(fsp_err)?;
        self.notify_change(&name, &rel, FILE_ACTION_RENAMED_OLD_NAME);
        self.notify_change(&name, &new_rel, FILE_ACTION_RENAMED_NEW_NAME);
        Ok(())
    }

    fn set_basic_info(
        &self,
        context: &Self::FileContext,
        file_attributes: u32,
        creation_time: u64,
        last_access_time: u64,
        last_write_time: u64,
        last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> Result<(), FspError> {
        let _ = (creation_time, last_access_time, last_change_time);
        let _ = file_attributes; // M0：READONLY 等属性位不落 overlay
        let overlay = context.overlay.as_ref().ok_or_else(err_access_denied)?;
        // 句柄级操作，不做 initializing 门控（git checkout 的 utime 走这里）。
        // upper 文件的逻辑 mtime 更新（git 回写 index mtime 走这里；
        // lower 文件固定 stat 不变）。
        if last_write_time != 0 && !context.is_dir {
            let mtime_ns = unix_ns_from_filetime(last_write_time);
            let existing = overlay.store().stat_entry(overlay.worktree_id(), &context.rel);
            if let Ok(Some(entry)) = existing {
                let _ = overlay.store().set_stat_entry(
                    overlay.worktree_id(),
                    &context.rel,
                    wtfs_core::StatCacheEntry { mtime_ns, ..entry },
                );
            }
        }
        let st = context.handle_stat()?;
        fill_file_info(file_info, &st);
        Ok(())
    }

    fn set_delete(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> Result<(), FspError> {
        let overlay = context.overlay.as_ref().ok_or_else(err_access_denied)?;
        if context.rel.is_empty() {
            // worktree 根目录删除（B060100，`git worktree remove` 的收尾
            // rmdir）：initializing（checkout 进行中）拒绝；视图仍有可见条目
            // 拒绝（与普通目录一致）。挂载根已在上面（overlay=None）拒绝。
            if delete_file {
                if overlay.is_initializing().map_err(fsp_err)? {
                    return Err(err_access_denied());
                }
                let non_empty = !overlay.list("").map_err(fsp_err)?.is_empty();
                if non_empty {
                    return Err(err_dir_not_empty());
                }
            }
            context.delete_pending.store(delete_file, Ordering::SeqCst);
            return Ok(());
        }
        // 非根路径：句柄级操作，不做 initializing 门控（暂存句柄可
        // delete-on-close）。
        if delete_file && context.is_dir {
            let non_empty = !overlay.list(&context.rel).map_err(fsp_err)?.is_empty();
            if non_empty {
                return Err(err_dir_not_empty());
            }
        }
        context.delete_pending.store(delete_file, Ordering::SeqCst);
        Ok(())
    }

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> Result<(), FspError> {
        if context.is_dir {
            return Err(err_is_directory());
        }
        // B060100：分配大小收缩即截断 EOF（NT 语义：EOF 不得超过分配大小）。
        // Rust std 对 CREATE_ALWAYS 的模拟（rust-lang/rust#115745：OPEN_ALWAYS
        // + FileAllocationInfo(0)）依赖此收缩截断；增长/持平仍只是预占提示，
        // 不改变 EOF。
        let shrink = set_allocation_size && new_size < context.current_eof()?;
        if set_allocation_size && !shrink {
            let st = context.handle_stat()?;
            fill_file_info(file_info, &st);
            return Ok(());
        }
        {
            let mut slot = context.ensure_write_state()?;
            match slot.as_mut().expect("ensure_write_state initializes") {
                WriteState::Staged(staged) => {
                    staged.mem.resize(new_size as usize, 0);
                    staged.size = new_size;
                    staged.hasher_valid = false;
                }
                WriteState::InPlace { file, size } => {
                    let cur = file.metadata().map_err(FspError::from)?.len();
                    file.set_len(new_size).map_err(FspError::from)?;
                    if new_size > cur {
                        file.seek(SeekFrom::Start(new_size)).map_err(FspError::from)?;
                    }
                    *size = new_size;
                }
            }
        }
        let st = context.handle_stat()?;
        fill_file_info(file_info, &st);
        Ok(())
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> Result<(), FspError> {
        use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let root = crate::win::to_path(&self.registry.config().store_root);
        let (mut total, mut free) = (0u64, 0u64);
        // SAFETY: HSTRING 以 NUL 结尾；输出均为合法指针。
        let ok = unsafe { GetDiskFreeSpaceExW(&root, None, Some(&mut total), Some(&mut free)) };
        match ok {
            Ok(()) => {
                out_volume_info.total_size = total;
                out_volume_info.free_size = free;
            }
            Err(e) => return Err(FspError::HRESULT(e.code().0)),
        }
        out_volume_info.set_volume_label(&self.registry.config().volume_label);
        Ok(())
    }
}

fn copy_sd(sd: &[u8], buf: &mut [c_void]) -> Result<(), FspError> {
    let bytes: &mut [u8] = unsafe {
        std::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), buf.len())
    };
    if bytes.len() < sd.len() {
        return Err(err_invalid_parameter());
    }
    bytes[..sd.len()].copy_from_slice(sd);
    Ok(())
}

impl WtfsContext {
    /// 单个 chunk 的写入（暂存或就地），返回写入字节数并回填 file_info。
    fn write_chunk(
        &self,
        context: &FileContext,
        worktree: &str,
        buffer: &[u8],
        offset: u64,
        file_info: &mut FileInfo,
    ) -> Result<u32, FspError> {
        let overlay = context.overlay.as_ref().ok_or_else(err_access_denied)?;
        {
            let mut slot = context.ensure_write_state()?;
            match slot.as_mut().expect("ensure_write_state initializes") {
                WriteState::Staged(staged) => {
                    let end = offset + buffer.len() as u64;
                    let append = offset == staged.size && staged.hasher_valid;
                    if append {
                        staged.hasher.update(buffer);
                    } else {
                        staged.hasher_valid = false;
                    }
                    if end > staged.size {
                        staged.size = end;
                    }
                    if staged.spill.is_some() {
                        let spill = staged.spill.as_mut().expect("checked above");
                        spill.file.seek(SeekFrom::Start(offset)).map_err(FspError::from)?;
                        spill.file.write_all(buffer).map_err(FspError::from)?;
                        spill.file.flush().map_err(FspError::from)?;
                    } else if append
                        && staged.mem.len() + buffer.len() > STAGE_SPILL_BYTES
                    {
                        // 溢写：把内存内容落到 upper 内隐藏临时文件。
                        let dir = overlay.upper_abs(&context.rel);
                        let parent = dir
                            .parent()
                            .map(|p| p.to_path_buf())
                            .unwrap_or_else(|| overlay.upper_abs(""));
                        let path = parent.join(format!(
                            ".wtfs-tmp-w-{}-{}",
                            std::process::id(),
                            SPILL_SEQ.fetch_add(1, Ordering::Relaxed)
                        ));
                        let mut file = File::create(&path).map_err(FspError::from)?;
                        file.write_all(&staged.mem).map_err(FspError::from)?;
                        file.write_all(buffer).map_err(FspError::from)?;
                        staged.mem = Vec::new();
                        staged.spill = Some(Spill { path, file });
                    } else {
                        let off = offset as usize;
                        if staged.mem.len() < off + buffer.len() {
                            staged.mem.resize(off + buffer.len(), 0);
                        }
                        staged.mem[off..off + buffer.len()].copy_from_slice(buffer);
                    }
                }
                WriteState::InPlace { file, size } => {
                    file.seek(SeekFrom::Start(offset)).map_err(FspError::from)?;
                    file.write_all(buffer).map_err(FspError::from)?;
                    let end = offset + buffer.len() as u64;
                    if end > *size {
                        *size = end;
                    }
                }
            }
        }
        let st = context.handle_stat()?;
        fill_file_info(file_info, &st);
        let _ = worktree;
        Ok(buffer.len() as u32)
    }
}


#[cfg(test)]
pub(crate) fn blank_open_file_info_for_test() -> OpenFileInfo {
    // 测试用：零值 OpenFileInfo（normalized_name 为 null；本实现不调用
    // set_normalized_name，安全）。
    unsafe { std::mem::zeroed() }
}

#[cfg(test)]
/// 测试用：从裸引用构造 DirMarker（字段 pub(crate)，对外无法构造）。
/// DirMarker 是单字段（Option<&U16CStr>）新类型，布局一致。
pub(crate) unsafe fn marker_for_test<'a>(name: Option<&'a U16CStr>) -> DirMarker<'a> {
    unsafe { std::mem::transmute::<Option<&'a U16CStr>, DirMarker<'a>>(name) }
}
