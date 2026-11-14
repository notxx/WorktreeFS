//! WinFSP/Win32 值转换与小工具：FILETIME ↔ Unix ns、NTSTATUS 构造、
//! WinFSP 路径 ↔ repo 相对路径、写访问位掩码。
//!
//! winfsp-rs 的 [`winfsp::FspError`] 只接受裸 `i32` NTSTATUS（其 `From<NTSTATUS>`
//! 实现绑定在它自己的 windows 版本上），因此这里统一手工构造。

use std::path::Path;

use winfsp::{U16CStr, U16CString};
use windows::Win32::Foundation::{
    STATUS_ACCESS_DENIED, STATUS_DIRECTORY_NOT_EMPTY, STATUS_FILE_IS_A_DIRECTORY,
    STATUS_INVALID_PARAMETER, STATUS_NOT_A_DIRECTORY, STATUS_NOT_SAME_DEVICE,
    STATUS_NO_MORE_FILES, STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_INVALID,
    STATUS_OBJECT_NAME_NOT_FOUND,
};
use winfsp::FspError;

/// Unix 纪元 → Win32 FILETIME（1601-01-01 起，100ns 单位）的偏移。
pub const UNIX_TO_FILETIME_TICKS: i64 = 116_444_736_000_000_000;

/// WinFSP Create/CreateDirectory 的选项位（winfsp.h `FILE_*_FILE`）。
pub const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
pub const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
/// `FILE_FLAG_DELETE_ON_CLOSE` 对应的 CreateOptions 位。
pub const FILE_DELETE_ON_CLOSE: u32 = 0x0000_1000;

/// NT 创建 disposition（fsctl.h `Req.Create.CreateOptions`：高 8 位 =
/// disposition，低 24 位 = 选项；WinFSP 对 Open/CreateEx 原样传递该字段）。
pub const FILE_SUPERSEDE: u32 = 0;
pub const FILE_OPEN: u32 = 1;
pub const FILE_CREATE: u32 = 2;
pub const FILE_OPEN_IF: u32 = 3;
pub const FILE_OVERWRITE: u32 = 4;
pub const FILE_OVERWRITE_IF: u32 = 5;

/// `CreateOptions` 高 8 位 → disposition。
pub fn disposition_of(create_options: u32) -> u32 {
    (create_options >> 24) & 0xff
}

/// 覆写类 disposition（`CREATE_ALWAYS`/`TRUNCATE_EXISTING`）：打开即清空
/// 既有内容（B060100 的截断语义来源）。
pub fn is_overwrite_disposition(create_options: u32) -> bool {
    matches!(
        disposition_of(create_options),
        FILE_SUPERSEDE | FILE_OVERWRITE | FILE_OVERWRITE_IF
    )
}

/// 写访问位（`FILE_ACCESS_RIGHTS`/`GENERIC_*`）：open 回调判断"写意图"用。
pub const WRITE_ACCESS_MASK: u32 = 0x4000_0000 // GENERIC_WRITE
    | 0x1000_0000 // GENERIC_ALL
    | 0x0000_0002 // FILE_WRITE_DATA
    | 0x0000_0004 // FILE_APPEND_DATA
    | 0x0000_0100 // FILE_WRITE_ATTRIBUTES
    | 0x0001_0000; // DELETE

/// 目录通知过滤器：名字/内容/大小/属性变化（winfsp.h `FILE_NOTIFY_CHANGE_*`）。
pub const NOTIFY_FILTER: u32 = 0x0000_0001 // FILE_NOTIFY_CHANGE_FILE_NAME
    | 0x0000_0002 // FILE_NOTIFY_CHANGE_DIR_NAME
    | 0x0000_0004 // FILE_NOTIFY_CHANGE_ATTRIBUTES
    | 0x0000_0008 // FILE_NOTIFY_CHANGE_SIZE
    | 0x0000_0010; // FILE_NOTIFY_CHANGE_LAST_WRITE

/// 目录通知动作（winfsp.h `FILE_ACTION_*`）。
pub const FILE_ACTION_ADDED: u32 = 0x0000_0001;
pub const FILE_ACTION_REMOVED: u32 = 0x0000_0002;
pub const FILE_ACTION_MODIFIED: u32 = 0x0000_0003;
pub const FILE_ACTION_RENAMED_OLD_NAME: u32 = 0x0000_0004;
pub const FILE_ACTION_RENAMED_NEW_NAME: u32 = 0x0000_0005;

/// NTSTATUS → [`FspError`]（回调返回值通道）。
pub fn nt_err(nt: windows::Win32::Foundation::NTSTATUS) -> FspError {
    FspError::NTSTATUS(nt.0)
}

pub fn err_not_found() -> FspError {
    nt_err(STATUS_OBJECT_NAME_NOT_FOUND)
}

pub fn err_collision() -> FspError {
    nt_err(STATUS_OBJECT_NAME_COLLISION)
}

pub fn err_access_denied() -> FspError {
    nt_err(STATUS_ACCESS_DENIED)
}

pub fn err_is_directory() -> FspError {
    nt_err(STATUS_FILE_IS_A_DIRECTORY)
}

pub fn err_not_a_directory() -> FspError {
    nt_err(STATUS_NOT_A_DIRECTORY)
}

pub fn err_dir_not_empty() -> FspError {
    nt_err(STATUS_DIRECTORY_NOT_EMPTY)
}

pub fn err_invalid_parameter() -> FspError {
    nt_err(STATUS_INVALID_PARAMETER)
}

pub fn err_no_more_files() -> FspError {
    nt_err(STATUS_NO_MORE_FILES)
}

pub fn err_invalid_name() -> FspError {
    nt_err(STATUS_OBJECT_NAME_INVALID)
}

/// 跨层目录 rename（M12：core `Error::CrossDevice`）：OverlayFS 默认 EXDEV
/// 语义的 NT 状态（Win32 `ERROR_NOT_SAME_DEVICE`），调用方按跨设备退化为
/// 递归复制+删除。
pub fn err_not_same_device() -> FspError {
    nt_err(STATUS_NOT_SAME_DEVICE)
}

/// Unix 纳秒 → FILETIME（100ns tick）。
pub fn filetime_from_unix_ns(ns: i64) -> u64 {
    // 负值（1970 前）按 0 截断；本项目时间戳均为正。
    let ticks = ns / 100 + UNIX_TO_FILETIME_TICKS;
    u64::from_le_bytes(ticks.to_le_bytes()).max(0)
}

/// 把核心层的错误映射为 WinFSP 错误。
pub fn fsp_err(e: wtfs_core::Error) -> FspError {
    match e {
        wtfs_core::Error::NotFound(_) => err_not_found(),
        wtfs_core::Error::InvalidPath { .. } => err_invalid_name(),
        wtfs_core::Error::CrossDevice(_) => err_not_same_device(),
        wtfs_core::Error::Io(io) => FspError::from(io),
        wtfs_core::Error::Overlay(_) | wtfs_core::Error::Template(_) => err_invalid_parameter(),
    }
}

/// WinFSP 传入的 UTF-16 路径（`\wt-1\src\a.rs` 形式，根为 `\`）。
/// 返回 `(首组件, 其后相对路径)`；根路径返回 `None`。
/// 正反斜杠同义（做归一）；拒绝 NUL 与空组件。
pub fn split_first_component(file_name: &U16CStr) -> Option<(String, String)> {
    let normalized = file_name.to_string().ok()?.replace('/', "\\");
    let normalized = normalized.trim_start_matches('\\').to_string();
    if normalized.is_empty() {
        return None;
    }
    let mut parts = normalized.splitn(2, '\\');
    let first = parts.next().unwrap_or_default().to_string();
    let rest = parts.next().unwrap_or_default().replace('\\', "/");
    if first.is_empty() {
        return None;
    }
    Some((first, rest))
}

/// repo 相对路径 → WinFSP 挂载内路径（`wt-1\src\a.rs` → `\wt-1\src\a.rs`）。
pub fn mount_path(worktree: &str, rel: &str) -> String {
    if rel.is_empty() {
        format!("\\{worktree}")
    } else {
        format!("\\{worktree}\\{}", rel.replace('/', "\\"))
    }
}

/// `U16CStr` 从 `&str`（测试与通知名构造用）；失败报 invalid parameter。
pub fn u16c(s: &str) -> winfsp::Result<U16CString> {
    U16CString::from_str(s).map_err(|_| err_invalid_parameter())
}

/// 文件大小 → 分配大小（向上取整到 512B 扇区）。
pub fn allocation_size(size: u64) -> u64 {
    size.div_ceil(512) * 512
}

/// 挂载点的真实路径（供卷容量查询等）。
pub fn to_path(p: impl AsRef<Path>) -> windows::core::HSTRING {
    windows::core::HSTRING::from(p.as_ref().as_os_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_epoch_matches_fixed_stat() {
        // wtfs-core 固定纪元 2000-01-01Z → 946684800000000_0 ns / 100 = 9.466848e15 tick。
        let ft = filetime_from_unix_ns(wtfs_core::FIXED_STAT_EPOCH_UNIX_NS);
        assert_eq!(
            ft,
            9_466_848_000_000_000u64 + UNIX_TO_FILETIME_TICKS as u64
        );
    }

    #[test]
    fn split_first_component_parses_winfsp_paths() {
        let root = u16c("\\").unwrap();
        assert_eq!(split_first_component(&root), None);
        let p = u16c("\\wt-1\\src\\lib.rs").unwrap();
        assert_eq!(
            split_first_component(&p),
            Some(("wt-1".into(), "src/lib.rs".into()))
        );
        let p = u16c("\\wt-1").unwrap();
        assert_eq!(split_first_component(&p), Some(("wt-1".into(), String::new())));
        // 正斜杠归一
        let p = u16c("/wt-1/src/lib.rs").unwrap();
        assert_eq!(
            split_first_component(&p),
            Some(("wt-1".into(), "src/lib.rs".into()))
        );
        let p = u16c("\\\\wt-1\\a.txt").unwrap();
        assert_eq!(split_first_component(&p), Some(("wt-1".into(), "a.txt".into())));
    }

    #[test]
    fn mount_path_roundtrip_shape() {
        assert_eq!(mount_path("wt-2", "a/b.txt"), "\\wt-2\\a\\b.txt");
        assert_eq!(mount_path("wt-2", ""), "\\wt-2");
    }

    #[test]
    fn allocation_rounds_up() {
        assert_eq!(allocation_size(0), 0);
        assert_eq!(allocation_size(1), 512);
        assert_eq!(allocation_size(512), 512);
        assert_eq!(allocation_size(513), 1024);
    }
}

/// WinFSP 运行时加载：先走标准 DLL 搜索（PATH），失败则从注册表
/// `HKLM\SOFTWARE\WOW6432Node\WinFsp\InstallDir` 解析安装目录并显式
/// `LoadLibraryW`（WinFSP 2.x 不把 bin 加进 PATH）。模块句柄进程内常驻
/// （OnceLock），delayload 之后按模块名解析即可命中。
///
/// mount 与回调级测试都必须先调用：WinFSP 的 `FspFileSystem*` 目录缓冲/
/// 通知函数 delayload 自 `winfsp-x64.dll`，未加载即 0xC06D007E 崩溃。
pub fn ensure_winfsp_runtime() -> winfsp::Result<()> {
    use std::sync::OnceLock;
    static LOADED: OnceLock<Result<(), u32>> = OnceLock::new();

    if let Some(Ok(())) = LOADED.get() {
        return Ok(());
    }
    let result = winfsp::winfsp_init().map(|_| ()).or_else(|_| {
        use windows::Win32::System::LibraryLoader::LoadLibraryW;
        use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RegGetValueW, RRF_RT_REG_SZ};

        let install_dir = || -> Option<String> {
            let mut buf = [0u16; 512];
            let mut size = (buf.len() * 2) as u32;
            // SAFETY: 固定键名/值名；输出缓冲与长度合法。
            let status = unsafe {
                RegGetValueW(
                    HKEY_LOCAL_MACHINE,
                    windows::core::w!("SOFTWARE\\WOW6432Node\\WinFsp"),
                    windows::core::w!("InstallDir"),
                    RRF_RT_REG_SZ,
                    None,
                    Some(buf.as_mut_ptr().cast()),
                    Some(&mut size),
                )
            };
            if status.is_err() {
                return None;
            }
            let units = &buf[..(size as usize / 2).min(buf.len())];
            let units: &[u16] = match units.iter().position(|&u| u == 0) {
                Some(pos) => &units[..pos],
                None => units,
            };
            Some(String::from_utf16_lossy(units))
        }()
        .ok_or(0x8007_0002u32)?; // ERROR_FILE_NOT_FOUND

        let arch = if cfg!(target_arch = "x86_64") {
            "winfsp-x64.dll"
        } else if cfg!(target_arch = "x86") {
            "winfsp-x86.dll"
        } else if cfg!(target_arch = "aarch64") {
            "winfsp-a64.dll"
        } else {
            return Err(0x8007_0002u32);
        };
        let path = windows::core::HSTRING::from(format!(
            "{}\\bin\\{arch}",
            install_dir.trim_end_matches(['\\', '/'])
        ));
        // SAFETY: 路径为注册表安装目录拼接的 NUL 结尾 HSTRING。
        let _handle = unsafe { LoadLibraryW(&path) }.map_err(|_| 0x8007_0002u32)?;
        // HMODULE 无需保存：LoadLibraryW 的引用计数使模块进程内常驻，
        // delayload 之后按模块名解析即可命中。
        Ok(())
    });
    let stored = result.as_ref().map(|_| ()).map_err(|e| *e);
    let _ = LOADED.set(stored);
    result.map_err(|code| FspError::NTSTATUS(code as i32))
}

#[cfg(test)]
mod runtime_tests {
    #[test]
    fn winfsp_runtime_loads_from_registry_or_path() {
        // 本机要求安装 WinFSP；未安装时该测试给出明确的延迟加载错误。
        crate::win::ensure_winfsp_runtime().expect("WinFSP runtime must load");
    }
}
