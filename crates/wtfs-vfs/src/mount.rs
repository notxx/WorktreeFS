//! 挂载生命周期：目录挂载 `<repo>/.tower/worktrees`（spec §6.1）。
//!
//! [`mount_directory`] 创建 [`WtfsContext`](crate::context::WtfsContext)
//! （含 notify timer），挂到给定目录并启动 dispatcher；返回的
//! [`MountedVolume`] 持有宿主——drop 即卸载（`FspFileSystemRemoveMountPoint`
//! + 停 dispatcher）。daemon（M4）将其纳入 named-pipe/service 编排。
//!
//! **WinFSP 目录挂载语义**（实测 + winfsp `FspMountSet_Directory`/`util.c`
//! 证实）：挂载点目录必须**不存在**——winfsp 以 `FILE_CREATE` 自行创建，
//! 打上 `IO_REPARSE_TAG_MOUNT_POINT` junction 且 `FILE_DELETE_ON_CLOSE`，
//! 卸载时连目录一并删除。因此 [`mount_directory`] 约定：
//!
//! * 挂载点已存在且为**空**目录（TowerInit 预建的 `.tower/worktrees`）→
//!   先移除，挂载成功后由 winfsp 重建；[`MountedVolume`] 卸载时恢复调用方
//!   的空目录（Tower 契约不变）。
//! * 已存在且**非空** → 拒绝挂载（防误伤真实数据，spec §6.2"行为不稳定
//!   退回盘符"的同类预案，M2 再验证 workspace 根场景）。
//! * 不存在 → 直接挂载，卸载后由 [`MountedVolume`] 补建空目录。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use winfsp::FspError;
use winfsp::host::{FileSystemHost, FileSystemParams, FineGuard, VolumeParams};

use crate::context::WtfsContext;
use crate::notify::NotifyEvent;
use crate::registry::VfsRegistry;
use crate::win::filetime_from_unix_ns;

/// notify timer 轮询周期（毫秒）：变更事件的批量发布节奏。
pub const NOTIFY_INTERVAL_MS: u32 = 500;

pub struct MountedVolume {
    /// 持有 WinFSP 宿主；显式 drop 触发卸载（RemoveMountPoint + 停 dispatcher）。
    host: std::mem::ManuallyDrop<FileSystemHost<WtfsContext, FineGuard>>,
    registry: Arc<VfsRegistry>,
    /// 卸载后需要恢复的空挂载根（`mount_directory` 挂载前移除的目录）。
    restore: Option<PathBuf>,
}

impl MountedVolume {
    pub fn registry(&self) -> &Arc<VfsRegistry> {
        &self.registry
    }

    /// 显式卸载（等价 drop；方便调用处命名）。
    pub fn unmount(self) {}
}

impl Drop for MountedVolume {
    fn drop(&mut self) {
        // SAFETY: host 只在此处 drop 一次（Drop 之后字段不再被使用）；
        // 先卸载 → WinFSP 删除 junction 目录（FILE_DELETE_ON_CLOSE）。
        unsafe { std::mem::ManuallyDrop::drop(&mut self.host) };
        if let Some(dir) = &self.restore {
            if let Err(e) = std::fs::create_dir_all(dir) {
                tracing::warn!(
                    dir = %dir.display(),
                    error = %e,
                    "failed to restore pre-mount worktrees directory"
                );
            }
        }
    }
}

/// 目录挂载到 `mount_point`。见模块文档的 WinFSP 目录挂载语义。
///
/// 卷参数：512B 扇区 / 8 扇区块、255 组件长度、保留大小写 + Unicode、
/// 卷序列号来自 [`VfsRegistry`] 配置、`post_cleanup_when_modified_only`
/// 减少冗余 Cleanup。`file_info_timeout` 1000ms（WinFSP 默认节拍），配合
/// 固定 stat/逻辑 mtime 足够 git 正确性。
pub fn mount_directory(
    mount_point: &Path,
    registry: Arc<VfsRegistry>,
) -> winfsp::Result<MountedVolume> {
    // 加载 WinFSP DLL（PATH 或注册表安装目录；delayload 需要模块已驻留）。
    crate::win::ensure_winfsp_runtime()?;

    // WinFSP 要求挂载点目录不存在（FILE_CREATE）：移除调用方预建的空目录，
    // 卸载时恢复。
    let restore = if mount_point.exists() {
        let is_empty = std::fs::read_dir(mount_point)
            .map(|mut rd| rd.next().is_none())
            .unwrap_or(false);
        if !is_empty {
            tracing::error!(
                mount_point = %mount_point.display(),
                "refusing to mount: pre-existing mount directory is not empty"
            );
            return Err(winfsp::FspError::NTSTATUS(
                windows::Win32::Foundation::STATUS_DIRECTORY_NOT_EMPTY.0,
            ));
        }
        std::fs::remove_dir(mount_point).map_err(FspError::from)?;
        Some(mount_point.to_path_buf())
    } else {
        None
    };

    let needs_restore = restore.is_some();
    match mount_inner(mount_point, registry, restore) {
        Ok(volume) => Ok(volume),
        Err(e) => {
            tracing::error!(mount_point = %mount_point.display(), error = %e, "mount failed");
            // 挂载失败：恢复调用方目录。
            if needs_restore {
                let _ = std::fs::create_dir_all(mount_point);
            }
            Err(e)
        }
    }
}

fn mount_inner(
    mount_point: &Path,
    registry: Arc<VfsRegistry>,
    restore: Option<PathBuf>,
) -> winfsp::Result<MountedVolume> {
    let mut params = VolumeParams::new();
    params
        .sector_size(512)
        .sectors_per_allocation_unit(8)
        .max_component_length(255)
        .volume_serial_number(registry.config().volume_serial)
        .volume_creation_time(filetime_from_unix_ns(
            wtfs_core::FIXED_STAT_EPOCH_UNIX_NS,
        ))
        .file_info_timeout(1000)
        .case_sensitive_search(false)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .post_cleanup_when_modified_only(true)
        .filesystem_name("WorktreeFS");
    let volume_params = FileSystemParams::default_params(params);

    let context = WtfsContext::new(registry.clone());
    let mut host: FileSystemHost<WtfsContext, FineGuard> =
        FileSystemHost::new_with_timer::<NotifyEvent, NOTIFY_INTERVAL_MS>(volume_params, context)?;
    host.mount(mount_point)?;
    host.start_with_threads(0)?;
    Ok(MountedVolume {
        host: std::mem::ManuallyDrop::new(host),
        registry,
        restore,
    })
}

// 类型约束自检：WtfsContext 必须实现 notify 框架（R = NotifyEvent），
// 保证 FileSystemHost::new_with_timer 的泛型约束在编译期成立。
const _: () = {
    const fn assert_notify<T>()
    where
        T: winfsp::notify::NotifyingFileSystemContext<NotifyEvent>,
    {
    }
    let _ = assert_notify::<WtfsContext>;
};
