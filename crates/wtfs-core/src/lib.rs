//! wtfs-core —— WorktreeFS 的共享核心类型、存储契约与 M0 核心实现。
//!
//! M1（B010100）交付 API 骨架，M2（B020100）在本 crate 内完成实现；M3+ 消费：
//!
//! * 值类型：[`TemplateId`]、[`WorktreeId`]、[`TemplateSpec`]、[`Template`]、
//!   [`TemplateFile`]、[`Stat`]、[`DedupeDecision`]、[`WorktreeState`]、[`OverlayConfig`]。
//! * 存储契约：[`TemplateStore`] 与 [`OverlayStore`]，由 wtfs-vfs（读路径、dedupe）
//!   与 wtfs-daemon（模板管理、注册表、gc）共同使用。
//! * 路径校验：[`validate_relative_path`]（Windows 保留名/保留尾字符，spec §11.2）。
//! * M2 实现：[`Db`]（schema/迁移）、[`SqliteTemplateStore`]（checkout + index
//!   快照 + blake3 清单）、[`SqliteOverlayStore`]（worktrees/whiteouts/renames/
//!   file_ids/stat_cache）、[`Overlay`]（upper>whiteout>lower 解析、目录合并、
//!   rename map、写路径 dedupe、固定 stat/逻辑 mtime）、[`dedupe`]（三态决策）、
//!   [`tower_state`]（state.json → 模板 base，缺失则全量 copy 兜底）。
//!
//! 稳定性承诺：类型与 trait 只做 additive 变更；方法语义不变。
//! 两个 trait 均为对象安全（`Send + Sync`），可放入 `Arc<dyn TemplateStore>`。
//!
//! 背景约束（spec §6.3）：M0 是 dedupe-only——不拦截 `git worktree add`，
//! 不依赖 hook/shim；FS 挂载在 `<repo>/.tower/worktrees`，主仓库保持原生。

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub mod db;
pub mod dedupe;
mod gitx;
pub mod overlay;
pub mod overlay_store;
pub mod template_store;
pub mod tower_state;

pub use db::Db;
pub use dedupe::{DedupeState, decide_for_path, hash_bytes, hash_file, hash_reader};
pub use overlay::{EntryKind, Overlay, OverlayDirEntry, Resolution, attributes_for};
pub use overlay_store::{SqliteOverlayStore, StatCacheEntry, WorktreeRegistration};
pub use template_store::SqliteTemplateStore;
pub use tower_state::{
    TowerMissionEntry, TowerStateFile, read_state_file, template_spec_for_worktree,
};

/// lower（模板）文件统一报告的固定 stat 纪元：2000-01-01T00:00:00Z（Unix 纳秒）。
/// 固定 mtime/ctime 使 Git 写入的 index 与 ready 后 FS 报告的 stat 一致（spec §7），
/// 并避开 racy-git。
pub const FIXED_STAT_EPOCH_UNIX_NS: i64 = 946_684_800_000_000_000;

/// M0 虚拟卷序列号默认值（ASCII "WTFS"）。真实卷号分配在后续里程碑由 daemon 决定。
pub const DEFAULT_VOLUME_SERIAL: u32 = 0x5754_4653;

/// blake3 哈希宽度；[`FileHash`] 的固定长度。
pub const FILE_HASH_LEN: usize = 32;

/// wtfs-core 统一 [`Result`]。
pub type Result<T> = std::result::Result<T, Error>;

/// wtfs-core 统一错误类型。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("template: {0}")]
    Template(String),
    #[error("overlay: {0}")]
    Overlay(String),
    #[error("invalid path {path:?}: {reason}")]
    InvalidPath { path: String, reason: String },
    #[error("not found: {0}")]
    NotFound(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// 模板主键（SQLite `templates.id`）。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
pub struct TemplateId(pub i64);

impl fmt::Display for TemplateId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "template-{}", self.0)
    }
}

/// worktree 主键（SQLite `worktrees.id`）。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
pub struct WorktreeId(pub i64);

impl fmt::Display for WorktreeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "worktree-{}", self.0)
    }
}

/// 模板创建/解析请求：repo + base ref（branch / tag / sha，spec §6.2）。
/// 实现负责把 `base_ref` 解析为 commit；模板身份 = repo + base commit。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateSpec {
    /// 仓库工作区根（真实路径，非 FS 视图）。
    pub repo_root: PathBuf,
    /// 模板基线 ref（如 `master`、`origin/main`、裸 sha）。
    pub base_ref: String,
}

impl TemplateSpec {
    pub fn new(repo_root: impl Into<PathBuf>, base_ref: impl Into<String>) -> Self {
        Self {
            repo_root: repo_root.into(),
            base_ref: base_ref.into(),
        }
    }
}

/// 模板生命周期状态（SQLite `templates.state`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TemplateState {
    /// 正在创建/更新（加锁，clone 不得读到半更新状态）。
    #[default]
    Building,
    /// 可用。
    Ready,
    /// 创建/更新失败；resolve 不得返回该模板。
    Failed,
}

impl fmt::Display for TemplateState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            TemplateState::Building => "building",
            TemplateState::Ready => "ready",
            TemplateState::Failed => "failed",
        };
        f.write_str(name)
    }
}

/// 已解析的模板（spec §6.2）：一次正常 checkout + index 快照，
/// 复用 Git 的 filter/CRLF/LFS 结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Template {
    pub id: TemplateId,
    /// 模板所属仓库根。
    pub repo_root: PathBuf,
    /// 解析后的 base commit（完整 sha 十六进制）。
    pub base_commit: String,
    /// 模板 checkout 树根（`%LOCALAPPDATA%\wtfs\repos\<repo-id>\templates\<commit>\tree`）。
    pub tree_path: PathBuf,
    /// 模板 index 快照路径（同目录 `index`）。
    pub index_path: PathBuf,
    pub state: TemplateState,
}

/// 内容哈希（blake3，32 字节）：M0 dedupe 对模板文件与写入内容的统一标识。
/// 与 Git 对象哈希（sha1）无关；sha1 仅在 index/对象操作时另行使用。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileHash(pub [u8; FILE_HASH_LEN]);

impl FileHash {
    /// 对一段完整内容计算 blake3。
    pub fn of_bytes(data: &[u8]) -> Self {
        Self(*blake3::hash(data).as_bytes())
    }

    /// 64 字符小写十六进制。
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(FILE_HASH_LEN * 2);
        for b in self.0 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    /// 解析 64 字符十六进制（大小写不限）。
    pub fn from_hex(hex: &str) -> Result<Self> {
        let bytes = hex.as_bytes();
        if bytes.len() != FILE_HASH_LEN * 2 {
            return Err(Error::Template(format!(
                "hash hex length {len} != {expected}",
                len = bytes.len(),
                expected = FILE_HASH_LEN * 2
            )));
        }
        let mut out = [0u8; FILE_HASH_LEN];
        for (i, slot) in out.iter_mut().enumerate() {
            let hi = hex_val(bytes[i * 2]).ok_or_else(|| {
                Error::Template(format!("invalid hex at offset {}", i * 2))
            })?;
            let lo = hex_val(bytes[i * 2 + 1]).ok_or_else(|| {
                Error::Template(format!("invalid hex at offset {}", i * 2 + 1))
            })?;
            *slot = (hi << 4) | lo;
        }
        Ok(Self(out))
    }
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

impl fmt::Debug for FileHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FileHash({})", self.to_hex())
    }
}

impl fmt::Display for FileHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// 模板内单个文件（SQLite `template_files` 行）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateFile {
    /// repo 相对路径：UTF-8，正斜杠 `/` 分隔，无 `./` 前缀（SQLite 存储键）。
    pub path: String,
    pub hash: FileHash,
    pub size: u64,
    /// Git 模式：`0o100644` / `0o100755`（symlink 在 v1 按 `core.symlinks=false` 不出现）。
    pub mode: u32,
}

/// 逻辑 stat（spec §6.3/§7）：FS 报告给 Git 与构建工具的稳定值；
/// 也是模板 index 改写（M1+）时写入 index entry 的同一组值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stat {
    /// 固定虚拟卷序列号（lower 文件恒定；index `dev` 截断为 32 位）。
    pub dev: u64,
    /// 稳定路径 ID（路径不变则不变；index `ino` 截断为 32 位）。
    pub ino: u64,
    /// Git 模式：`0o100644` / `0o100755`。
    pub mode: u32,
    pub size: u64,
    /// 固定纪元纳秒（lower）；upper 为真实/逻辑 mtime，单调前进。
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

impl Stat {
    /// lower（模板）文件：固定 dev/mtime/ctime，size/mode 来自模板 index，
    /// ino 为稳定路径 ID（`uid`/`gid` 在 Windows 上恒为 0，不建模）。
    pub fn fixed_lower(volume_serial: u64, ino: u64, mode: u32, size: u64) -> Self {
        Self {
            dev: volume_serial,
            ino,
            mode,
            size,
            mtime_ns: FIXED_STAT_EPOCH_UNIX_NS,
            ctime_ns: FIXED_STAT_EPOCH_UNIX_NS,
        }
    }
}

/// M0 写入路径 dedupe 决策（spec §6.3）：
/// 写入内容与模板完全一致 → [`DedupeDecision::ToLower`]（丢弃刚写的内容，视图回落 lower）；
/// 否则 → [`DedupeDecision::ToUpper`]（内容留在 upper）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DedupeDecision {
    ToUpper,
    ToLower,
}

impl DedupeDecision {
    /// 模板缺失（`None`）或内容不同 → upper；内容一致 → lower。
    pub fn decide(template_hash: Option<FileHash>, written: FileHash) -> Self {
        match template_hash {
            Some(h) if h == written => DedupeDecision::ToLower,
            _ => DedupeDecision::ToUpper,
        }
    }

    /// `true` 表示内容需要留在 upper（M0 中即：不能删除刚写的副本）。
    pub fn writes_upper(self) -> bool {
        matches!(self, DedupeDecision::ToUpper)
    }
}

/// worktree 生命周期状态（spec §6.3）：
/// FS 对 `wt-*` 维护的状态机，`readdir` 为空 → [`WorktreeState::Initializing`]；
/// Git 写完 worktree index（daemon 监听 `<main>/.git/worktrees/<name>/index`）→
/// [`WorktreeState::Ready`]，视图切成“模板 + upper”。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorktreeState {
    /// 初始化中：`readdir` 返回空，让 Git 通过“目录必须为空”检查；写入全量进 upper。
    #[default]
    Initializing,
    /// 视图 = lower + upper（应用 whiteout/rename）。
    Ready,
    /// 初始化失败：不允许 worker 在此 worktree 上启动；标记待清理。
    Failed,
    /// 已注销（merge/abandon/teardown 后 gc 清理完成）。
    Removed,
}

impl WorktreeState {
    /// `true` = FS 必须隐藏全部条目（`initializing` 语义）。
    pub fn hides_entries(self) -> bool {
        matches!(self, WorktreeState::Initializing)
    }
}

impl fmt::Display for WorktreeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            WorktreeState::Initializing => "initializing",
            WorktreeState::Ready => "ready",
            WorktreeState::Failed => "failed",
            WorktreeState::Removed => "removed",
        };
        f.write_str(name)
    }
}

bitflags::bitflags! {
    /// Win32 文件属性位（与 `FILE_ATTRIBUTE_*` 常量同值），供 VFS 回填 `get_file_info`。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct FileAttributes: u32 {
        const READONLY = 0x0000_0001;
        const HIDDEN = 0x0000_0002;
        const SYSTEM = 0x0000_0004;
        const DIRECTORY = 0x0000_0010;
        const ARCHIVE = 0x0000_0020;
        const NORMAL = 0x0000_0080;
        const TEMPORARY = 0x0000_0100;
    }
}

/// 单个 worktree 的 overlay 后端配置（spec §4.1/§8.1）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayConfig {
    pub worktree_id: WorktreeId,
    /// lower 来源模板；`None` = 无模板，全部写 upper（spec §6.3 兜底：全量 copy）。
    pub template_id: Option<TemplateId>,
    /// 模板 checkout 树（lower 根）；与 `template_id` 同为 `None` 表示无模板。
    pub lower_root: Option<PathBuf>,
    /// worktree 私有 upper 根（`%LOCALAPPDATA%\wtfs\...\worktrees\<wt-id>\upper`）。
    pub upper_root: PathBuf,
    /// overlay 元数据 SQLite（WAL）；`None` = 纯内存元数据（测试用）。
    pub meta_db: Option<PathBuf>,
    /// lower 文件固定 `dev`。
    pub volume_serial: u32,
    /// lower 文件固定 mtime/ctime（Unix 纳秒），默认 [`FIXED_STAT_EPOCH_UNIX_NS`]。
    pub fixed_stat_epoch_ns: i64,
    /// M0 dedupe 开关；关闭 = 全量 copy（兜底/性能对照路径）。
    pub dedupe_enabled: bool,
}

impl OverlayConfig {
    /// 以默认值构造（无模板、dedupe 开启、固定纪元、默认卷序列号）；
    /// `template_id`/`lower_root`/`meta_db` 由调用方按需填充。
    pub fn new(worktree_id: WorktreeId, upper_root: impl Into<PathBuf>) -> Self {
        Self {
            worktree_id,
            template_id: None,
            lower_root: None,
            upper_root: upper_root.into(),
            meta_db: None,
            volume_serial: DEFAULT_VOLUME_SERIAL,
            fixed_stat_epoch_ns: FIXED_STAT_EPOCH_UNIX_NS,
            dedupe_enabled: true,
        }
    }

    /// lower 文件的固定 stat。
    pub fn lower_stat(&self, ino: u64, mode: u32, size: u64) -> Stat {
        Stat {
            dev: self.volume_serial as u64,
            ino,
            mode,
            size,
            mtime_ns: self.fixed_stat_epoch_ns,
            ctime_ns: self.fixed_stat_epoch_ns,
        }
    }
}

/// 模板注册表契约（spec §6.2；SQLite `templates` / `template_files`）。
///
/// 实现要求：`Send + Sync`（跨线程共享）；模板更新期间加锁，
/// `resolve` 不得返回 [`TemplateState::Building`] 或 [`TemplateState::Failed`] 的模板。
pub trait TemplateStore: Send + Sync {
    /// 按 repo + base ref 精确解析已有模板；只查不建、不加模板更新锁。
    fn resolve(&self, spec: &TemplateSpec) -> Result<Option<Template>>;

    /// 创建或更新模板（正常 checkout + `update-index --refresh` + 快照 index +
    /// 计算文件 hash 入 `template_files`）；`spec.base_ref` 先解析为 commit。
    fn create_or_update(&self, spec: &TemplateSpec) -> Result<Template>;

    fn get(&self, id: TemplateId) -> Result<Option<Template>>;

    /// 全量文件清单：dedupe 与 lower stat 的基础数据（VFS 建内存索引用）。
    fn files(&self, id: TemplateId) -> Result<Vec<TemplateFile>>;

    /// 单文件查询（写入路径 dedupe 的热路径）。
    fn file(&self, id: TemplateId, path: &str) -> Result<Option<TemplateFile>>;
}

/// 模板文件清单的内存索引：repo 相对路径 → [`TemplateFile`]。
/// VFS 在挂载/ready 切换时由 [`TemplateStore::files`] 构建。
pub type FileHashMap = HashMap<String, TemplateFile>;

/// overlay 元数据契约（spec §8.2）：
/// `worktrees` / `dedupe` / `whiteouts` / `renames` / `file_ids` / `stat_cache`。
///
/// 实现要求：`Send + Sync`；SQLite WAL、写操作事务化；upper 文件是数据真相，
/// 元数据丢失只允许“被删 lower 文件重新出现”，不允许静默丢数据。
pub trait OverlayStore: Send + Sync {
    /// 注册 worktree（写 `worktrees` 行，状态 = [`WorktreeState::Initializing`]）。
    /// 同 id 重复注册返回 [`Error::Overlay`]。
    fn register(&self, config: &OverlayConfig) -> Result<()>;

    /// 读取注册配置（崩溃恢复用；无模板时 `template_id`/`lower_root` 为 `None`）。
    fn config(&self, id: WorktreeId) -> Result<Option<OverlayConfig>>;

    fn state(&self, id: WorktreeId) -> Result<Option<WorktreeState>>;

    /// 状态切换（`initializing → ready` 等）。
    fn set_state(&self, id: WorktreeId, state: WorktreeState) -> Result<()>;

    /// 记录一次写入路径 dedupe 决策（SQLite `dedupe` 表）。
    fn record_dedupe(&self, id: WorktreeId, path: &str, decision: DedupeDecision)
        -> Result<()>;

    /// lower 存在而 upper 删除 → whiteout（否则被删文件会在合并视图中重现）。
    fn add_whiteout(&self, id: WorktreeId, path: &str) -> Result<()>;
    /// upper 重建了被 whiteout 的路径 → 撤销 whiteout。
    fn remove_whiteout(&self, id: WorktreeId, path: &str) -> Result<()>;

    /// 目录级重命名映射（SQLite `renames` 表；文件级重命名 = whiteout + copy-up）。
    fn record_rename(&self, id: WorktreeId, from: &str, to: &str) -> Result<()>;

    /// 稳定 file id（Windows FILE_ID 与 index `ino` 用；持久映射，路径不变则不变）。
    fn file_id(&self, id: WorktreeId, path: &str) -> Result<u64>;

    /// 注销并清空该 worktree 的全部元数据（merge/abandon/gc）；
    /// upper 文件目录由调用方删除。
    fn unregister(&self, id: WorktreeId) -> Result<()>;
}

/// 校验 repo 相对路径（spec §5.2 路径校验；模板创建与写入路径均调用）。
///
/// 规则（v1，spec §11.2：大小写冲突/保留名/非 UTF-8 在模板创建时检测并报错）：
/// 非 UTF-8 输入在调用方已拒绝；这里拒绝空路径、绝对路径/盘符、
/// 空或 `.` / `..` 组件、Windows 保留名（含 `CON.txt` 形式）、
/// 尾随 `.` 或空格的组件（Win32 会剥离导致歧义）、超过 255 UTF-16 单元的组件。
pub fn validate_relative_path(path: &str) -> Result<()> {
    let reject = |reason: &str| -> Error {
        Error::InvalidPath {
            path: path.to_string(),
            reason: reason.to_string(),
        }
    };

    if path.is_empty() {
        return Err(reject("empty"));
    }
    if path.contains('\0') {
        return Err(reject("NUL byte"));
    }
    if path.starts_with('/') || path.starts_with('\\') || path.contains(':') {
        return Err(reject("absolute or drive-qualified"));
    }

    for component in path.split('/') {
        if component.is_empty() {
            return Err(reject("empty component"));
        }
        if component == "." || component == ".." {
            return Err(reject("dot component"));
        }
        if component.chars().count() > 255 {
            return Err(reject("component longer than 255 UTF-16 units"));
        }
        let base = component.split_once('.').map_or(component, |(b, _)| b);
        if is_reserved_name(base) {
            return Err(reject("reserved device name"));
        }
        if component.ends_with('.') || component.ends_with(' ') {
            return Err(reject("trailing dot or space"));
        }
    }
    Ok(())
}

fn is_reserved_name(base: &str) -> bool {
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6",
        "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7",
        "LPT8", "LPT9",
    ];
    let upper = base.to_ascii_uppercase();
    RESERVED.contains(&upper.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_hash_roundtrip_and_determinism() {
        let a = FileHash::of_bytes(b"hello");
        let b = FileHash::of_bytes(b"hello");
        let c = FileHash::of_bytes(b"world");
        assert_eq!(a, b);
        assert_ne!(a, c);
        let hex = a.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(FileHash::from_hex(&hex).unwrap(), a);
        assert_eq!(FileHash::from_hex(&hex.to_uppercase()).unwrap(), a);
        assert!(FileHash::from_hex("zz").is_err());
        assert!(FileHash::from_hex("abcd").is_err());
    }

    #[test]
    fn dedupe_decision_matches_template() {
        let template = FileHash::of_bytes(b"template");
        let same = template;
        let diff = FileHash::of_bytes(b"modified");
        assert_eq!(
            DedupeDecision::decide(Some(template), same),
            DedupeDecision::ToLower
        );
        assert!(!DedupeDecision::decide(Some(template), same).writes_upper());
        assert_eq!(
            DedupeDecision::decide(Some(template), diff),
            DedupeDecision::ToUpper
        );
        // 模板缺失 → 全量 copy 兜底（spec §6.3.4/6.3.6）。
        assert_eq!(
            DedupeDecision::decide(None, same),
            DedupeDecision::ToUpper
        );
        assert!(DedupeDecision::decide(None, same).writes_upper());
    }

    #[test]
    fn fixed_lower_stat_uses_fixed_epoch() {
        let st = Stat::fixed_lower(DEFAULT_VOLUME_SERIAL as u64, 42, 0o100644, 123);
        assert_eq!(st.dev, DEFAULT_VOLUME_SERIAL as u64);
        assert_eq!(st.mtime_ns, FIXED_STAT_EPOCH_UNIX_NS);
        assert_eq!(st.ctime_ns, FIXED_STAT_EPOCH_UNIX_NS);
        assert_eq!(st.size, 123);
    }

    #[test]
    fn overlay_config_defaults_match_spec() {
        let cfg = OverlayConfig::new(WorktreeId(1), r"%LOCALAPPDATA%\wtfs\upper");
        assert_eq!(cfg.template_id, None);
        assert_eq!(cfg.lower_root, None);
        assert!(cfg.dedupe_enabled);
        assert_eq!(cfg.volume_serial, DEFAULT_VOLUME_SERIAL);
        assert_eq!(cfg.fixed_stat_epoch_ns, FIXED_STAT_EPOCH_UNIX_NS);
        let st = cfg.lower_stat(7, 0o100755, 9);
        assert_eq!(st.ino, 7);
    }

    #[test]
    fn worktree_state_lifecycle() {
        assert!(WorktreeState::default().hides_entries());
        assert!(!WorktreeState::Ready.hides_entries());
        assert_eq!(WorktreeState::Ready.to_string(), "ready");
        assert_eq!(WorktreeState::Initializing.to_string(), "initializing");
        assert_eq!(TemplateState::default().to_string(), "building");
    }

    #[test]
    fn id_display_is_stable() {
        assert_eq!(TemplateId(3).to_string(), "template-3");
        assert_eq!(WorktreeId(5).to_string(), "worktree-5");
    }

    #[test]
    fn relative_path_accepts_normal_repo_paths() {
        for ok in [
            "src",
            "src/lib.rs",
            "crates/wtfs-core/src/lib.rs",
            "a.b/c..d/name with space.txt",
            "COM10.md", // COM0/COM10+ 不是保留名
            "convention.md",
        ] {
            validate_relative_path(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
    }

    #[test]
    fn relative_path_rejects_dangerous_paths() {
        for (bad, expected) in [
            ("", "empty"),
            ("a\0b", "NUL byte"),
            ("/abs", "absolute or drive-qualified"),
            ("C:/x", "absolute or drive-qualified"),
            ("a//b", "empty component"),
            ("./x", "dot component"),
            ("a/../b", "dot component"),
            ("..", "dot component"),
            ("CON", "reserved device name"),
            ("con.txt", "reserved device name"),
            ("lpt9.log", "reserved device name"),
            ("aux ", "trailing dot or space"),
            ("x.", "trailing dot or space"),
        ] {
            let err = validate_relative_path(bad)
                .err()
                .unwrap_or_else(|| panic!("{bad:?} should be rejected"));
            let Error::InvalidPath { path, reason } = err else {
                panic!("expected InvalidPath for {bad:?}");
            };
            assert_eq!(path, bad);
            assert_eq!(reason, expected, "reason mismatch for {bad:?}");
        }
    }
}
