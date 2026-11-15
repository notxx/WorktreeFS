//! wtfs-cas —— C 方案核心：Git 对象库作 CAS + manifest 虚拟命名空间 + 按需
//! hydration（M17/B170100，库层；WinFSP 挂载在 M18/B180100 接入）。
//!
//! MVP 边界（spec：与 B1 公平对比）：
//!
//! * **CAS 复用 Git 对象库**：blob 已内容寻址且全局去重，不另建独立 CAS；
//!   读取走常驻 `git cat-file --batch` 子进程（[`CatFile`]，Mutex 串行化
//!   请求/响应，按 oid 取 blob）。
//! * **虚拟命名空间 = 每 worktree 一份 manifest**：`git ls-tree -r -l -z
//!   <commit>` 一次解析为 path → (oid, mode, size)（[`Manifest`]），派生目录
//!   集合与每目录子项索引；打开 O(文件数)，之后 stat/list 纯内存查询。
//! * **视图模型**（[`CasView`]）：解析顺序 upper > whiteout > manifest；
//!   lower 读 = manifest 命中 → hydration 缓存（[`HydrationCache`]，按 oid
//!   分片、temp+rename 原子落盘、命中/未命中统计）→ 读缓存文件；upper 是
//!   真实文件；whiteout 复用 M12 日志语义（append-only `markers.log`，镜像
//!   即真相，撕裂尾忽略，[`markers::MarkerLog`]）。写路径 MVP 恒落 upper
//!   （不做写路径内容 dedupe——那是 B1 的差异化路径，M18 再定）；目录级
//!   rename/redirect 不在 MVP（M12 起 B1 也默认 EXDEV）。
//! * **stat 规则与 wtfs-core 逐字对齐**（公平对比口径）：manifest 文件固定
//!   dev/ino/mtime/ctime（size/mode 来自 manifest，mtime = 固定纪元
//!   [`FIXED_STAT_EPOCH_UNIX_NS`]）；upper 用真实 mtime；ino = blake3(小写
//!   路径) 62 位（[`file_id`]，根目录固定 1）；目录 `0o040755` size 0。
//! * **索引种子**（[`index_seed`]）：`git worktree add --no-checkout` 后
//!   `git read-tree <commit>` + 批量 `update-index --skip-worktree --stdin`
//!   ——M17 实测未挂载时这是让 `git status` 干净且零 hydrate 的唯一途径
//!   （复制 template index 的 route B 未挂载必然报全量删除）；同时把每条
//!   entry 的 stat 改写为 manifest 值（gix-index），M18 挂载后 FS 报告
//!   manifest stat 即逐位对齐，清除 skip-worktree 位即可切换挂载态。
//!
//! 不依赖 wtfs-core（C 独立实现，便于与 B1 的 overlay 实现分离评测）；常量
//! 与派生算法刻意镜像 wtfs-core（epoch/ino/路径校验），保证对比口径与后续
//! 统一挂载层的语义一致。挂载点约定仍为 `<repo>/.tower/worktrees`，主仓库
//! 保持原生。

pub mod cache;
pub mod cat_file;
pub mod gitx;
pub mod index_seed;
pub mod manifest;
pub mod markers;
pub mod view;

pub use cache::{CacheStatsSnapshot, GcReport, HydrationCache};
pub use cat_file::{CatFile, CatFileStats};
pub use index_seed::{
    SeedReport, add_worktree_no_checkout, clear_skip_worktree, seed_skip_worktree,
    seed_stat_aligned,
};
pub use manifest::{EntryKind, Manifest, ManifestFile};
pub use markers::MarkerLog;
pub use view::{CasDirEntry, CasView, CasViewConfig, WriteOutcome, WriteState};

/// lower（manifest）文件统一报告的固定 stat 纪元：2000-01-01T00:00:00Z
/// （Unix 纳秒）。与 wtfs-core [`FIXED_STAT_EPOCH`] 一致——同一枚举值是
/// A/B 方案 index 对齐的共同前提（spec §7）。
///
/// [`FIXED_STAT_EPOCH`]: wtfs_core::FIXED_STAT_EPOCH_UNIX_NS
pub const FIXED_STAT_EPOCH_UNIX_NS: i64 = 946_684_800_000_000_000;

/// M0 虚拟卷序列号默认值（ASCII "WTFS"），与 wtfs-core 默认值一致。
pub const DEFAULT_VOLUME_SERIAL: u32 = 0x5754_4653;

/// upper 内写入临时文件的名字前缀（对视图隐藏，崩溃残留由 gc 清扫）。
/// 与 wtfs-core 的 `TEMP_PREFIX` 同名同义。
pub const TEMP_PREFIX: &str = ".wtfs-tmp";

/// 视图根目录的固定 ino（路径哈希前的特例值），与 wtfs-core 一致。
pub const ROOT_FILE_ID: u64 = 1;

/// Git 树模式：常规文件。
pub const MODE_REGULAR: u32 = 0o100644;
/// Git 树模式：可执行文件。
pub const MODE_EXEC: u32 = 0o100755;
/// Git 树模式：符号链接（blob 内容 = 链接目标路径字节）。
pub const MODE_SYMLINK: u32 = 0o120000;
/// Git 树模式：子模块 gitlink（对象库无对应 blob，[`CasView::read`] 报错）。
pub const MODE_GITLINK: u32 = 0o160000;
/// 目录在 stat 中报告的模式（size 恒 0）。
pub const MODE_DIR: u32 = 0o040755;

/// wtfs-cas 统一 [`Result`]。
pub type Result<T> = std::result::Result<T, Error>;

/// wtfs-cas 统一错误类型。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cas: {0}")]
    Cas(String),
    #[error("git: {0}")]
    Git(String),
    #[error("invalid path {path:?}: {reason}")]
    InvalidPath { path: String, reason: String },
    #[error("not found: {0}")]
    NotFound(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// 逻辑 stat（与 wtfs-core `Stat` 同构）：FS 报告给 Git 与构建工具的稳定值；
/// 也是 [`index_seed`] 写入 index entry 的同一组值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CasStat {
    /// 固定虚拟卷序列号（manifest 文件恒定；index `dev` 截断为 32 位）。
    pub dev: u64,
    /// 稳定路径 ID（路径不变则不变；index `ino` 截断为 32 位）。
    pub ino: u64,
    /// Git 模式：[`MODE_REGULAR`] / [`MODE_EXEC`] / [`MODE_SYMLINK`] /
    /// [`MODE_GITLINK`]；目录恒 [`MODE_DIR`]。
    pub mode: u32,
    pub size: u64,
    /// 固定纪元纳秒（manifest）；upper 为真实文件 mtime（随修改自然前进）。
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

/// 路径 → 稳定 ino：blake3(小写路径) 前 8 字节 BE 截 62 位，0 → 1。
/// 与 wtfs-core `derive_ino` 同一算法——同一路径在 A/B 两方案下得到同一
/// ino，是 index 对齐与公平对比的前提。根目录（空路径）用 [`ROOT_FILE_ID`]。
pub fn file_id(rel: &str) -> u64 {
    if rel.is_empty() {
        return ROOT_FILE_ID;
    }
    let hash = blake3::hash(rel.to_ascii_lowercase().as_bytes());
    let mut v: u64 = 0;
    for b in &hash.as_bytes()[..8] {
        v = (v << 8) | u64::from(*b);
    }
    let v = v & 0x3FFF_FFFF_FFFF_FFFF;
    if v == 0 { 1 } else { v }
}

/// 校验 repo 相对路径（镜像 wtfs-core `validate_relative_path`，spec §11.2）：
/// 拒绝空路径、绝对路径/盘符、空或 `.` / `..` 组件、Windows 保留名（含
/// `CON.txt` 形式）、尾随 `.` 或空格的组件、超过 255 UTF-16 单元的组件。
/// manifest 装载与视图入口共用，保证两方案对危险路径同一拒绝。
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
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let upper = base.to_ascii_uppercase();
    RESERVED.contains(&upper.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_id_matches_wtfs_core_derivation() {
        // 与 wtfs-core derive_ino 的公开测试同一断言口径：62 位正数、
        // 大小写稳定、离散分布。
        const MAX_INO: u64 = 0x3FFF_FFFF_FFFF_FFFF;
        let a = file_id("src/lib.rs");
        assert!(a > 0 && a <= MAX_INO);
        assert_eq!(a, file_id("SRC/LIB.RS"), "case-insensitive path stability");
        for i in 0..1000 {
            let v = file_id(&format!("p{i}"));
            assert!(v > 0);
        }
        assert_eq!(file_id(""), ROOT_FILE_ID);
    }

    #[test]
    fn relative_path_rules_match_wtfs_core() {
        for ok in ["src/lib.rs", "a.b/c..d/name with space.txt", "COM10.md"] {
            validate_relative_path(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in [
            "", "/abs", "C:/x", "a//b", "./x", "a/../b", "con.txt", "x.", "aux ",
        ] {
            assert!(
                validate_relative_path(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }
}
