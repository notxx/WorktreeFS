//! wtfs-vfs —— WinFSP 文件系统层（M0：dedupe-only）。
//!
//! 挂载形态（spec §4.1/§6.1）：目录挂载 `<repo>/.tower/worktrees`，主仓库
//! 保持原生；挂载根下的 `wt-*` 走 COW overlay：
//!
//! * lower = 模板 checkout（只读），upper = worktree 私有目录，只存改动。
//! * 初始化期（[`wtfs_core::WorktreeState::Initializing`]）除 worktree 根
//!   外的按名查找一律不存在，`git worktree add` 的"目录必须为空"检查因此
//!   通过；Git 写完 worktree index 后由 daemon 调
//!   [`registry::VfsRegistry::mark_ready`]（M3 暴露钩子，index 监听归 M4）。
//! * 写入路径 dedupe：暂存/就地两段式。暂存流（checkout）cleanup/flush 时经
//!   [`wtfs_core::Overlay::finish_staged`] 直接终裁（M9/B090100 零拷贝：
//!   一致则丢弃暂存回落 lower——零 upper 文件、零 fsync、零临时副本；不同才
//!   溢写改名 / 内存经临时文件原子改名 upper，恰好一次 fsync）；就地流（worker
//!   编辑）仍经 [`wtfs_core::Overlay::write_from`] post-hoc 比对回收。失败保留
//!   upper 全量内容（正确性优先）。I/O 计数器见
//!   [`wtfs_core::Overlay::io_stats`]。
//! * stat：lower 报 [`wtfs_core::Stat::fixed_lower`] 固定值（与 index 对
//!   齐），upper 报真实 mtime（M11 起无逻辑时钟）；稳定 file id；卷序列号固定。
//! * 通知：变更事件经 notify timer 发 `FspFileSystemNotify`。
//!
//! 模块：
//!
//! * [`registry`] —— worktree 注册表 + mark_ready 公共钩子 + 自动注册。
//! * [`context`]  —— WinFSP 回调实现（get_volume_info/security/open/create/
//!   overwrite/read/write/read_directory/rename/cleanup/flush/set_*）。
//! * [`mount`]    —— 目录挂载/卸载（[`mount::mount_directory`]）。
//! * [`notify`]   —— `FspFileSystemNotify` 事件队列。
//! * [`security`] —— 固定安全描述符。
//! * [`win`]      —— FILETIME/NTSTATUS/路径转换与常量。

pub mod context;
pub mod mount;
pub mod notify;
pub mod registry;
pub mod security;
pub mod win;

#[cfg(test)]
mod tests;

pub use context::{FileContext, WtfsContext};
pub use mount::{MountedVolume, mount_directory};
pub use notify::{NotifyEvent, NotifyHub};
pub use registry::{VfsConfig, VfsRegistry};
