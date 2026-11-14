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
//!   的空目录（Tower 契约不变）。恢复前先等旧 junction 真正消失——
//!   `RemoveMountPoint` 返回 ≠ 目录句柄全部关闭，删除在 FS 侧异步生效，
//!   delete-pending 窗口内补建必失败（B070100 竞态）；补建重试，最终失败
//!   升格为 error 日志，不再静默 warn。
//! * 已存在且**非空** → 拒绝挂载（防误伤真实数据，spec §6.2"行为不稳定
//!   退回盘符"的同类预案，M2 再验证 workspace 根场景）。
//! * 不存在 → 直接挂载，卸载后由 [`MountedVolume`] 补建空目录。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use winfsp::FspError;
use winfsp::host::{FileSystemHost, FileSystemParams, FineGuard, VolumeParams};

use crate::context::WtfsContext;
use crate::notify::NotifyEvent;
use crate::registry::VfsRegistry;
use crate::win::filetime_from_unix_ns;

/// notify timer 轮询周期（毫秒）：变更事件的批量发布节奏。
pub const NOTIFY_INTERVAL_MS: u32 = 500;

/// 等待旧 junction 目录消失的上限（B070100）：`RemoveMountPoint` 返回后
/// WinFSP 侧的 `FILE_DELETE_ON_CLOSE` 删除仍可能未生效，delete-pending
/// 窗口内补建必然失败。
const RESTORE_DELETE_WAIT: Duration = Duration::from_secs(2);
/// junction 消失轮询间隔。
const RESTORE_POLL_INTERVAL: Duration = Duration::from_millis(25);
/// 补建空目录的重试次数与间隔（覆盖句柄刚释放的短暂窗口）。
const RESTORE_CREATE_RETRIES: u32 = 5;
const RESTORE_CREATE_RETRY_DELAY: Duration = Duration::from_millis(100);

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
            if let Err(e) = restore_mount_point_dir(dir) {
                // 卸载路径无法向上传播错误：升格为 error 日志并写明后果
                // （Tower 目录契约落空），不再静默（B070100）。
                tracing::error!(
                    dir = %dir.display(),
                    error = %e,
                    "failed to restore the empty worktrees directory after unmount; \
                     the Tower directory contract is broken until it is recreated"
                );
            }
        }
    }
}

/// 卸载/挂载失败后恢复空挂载点目录（Tower 契约：`<repo>/.tower/worktrees`
/// 必须以真实空目录存在）。
///
/// 先等旧 junction 真正消失（B070100 竞态：delete-pending 期间目录元数据
/// 仍可见但句柄未释放，此时 create 必失败；也绝不能把空目录建在活 junction
/// 上），再补建；补建遇 `AlreadyExists` 时复核是真实目录才算恢复成功。
fn restore_mount_point_dir(dir: &Path) -> std::io::Result<()> {
    let deadline = std::time::Instant::now() + RESTORE_DELETE_WAIT;
    loop {
        match std::fs::symlink_metadata(dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Ok(meta) if !meta.file_type().is_symlink() => break,
            _ if std::time::Instant::now() >= deadline => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "mount point {} still a junction after {}ms; refusing to \
                         recreate the directory over a lingering mount",
                        dir.display(),
                        RESTORE_DELETE_WAIT.as_millis()
                    ),
                ));
            }
            _ => std::thread::sleep(RESTORE_POLL_INTERVAL),
        }
    }

    // 叶子目录用 create_dir 而非 create_dir_all：后者对"路径已是 junction"
    // 会静默成功（is_dir 穿透 reparse point），正是 B070100 要防的落空形态。
    let mut last_err = None;
    for _ in 0..RESTORE_CREATE_RETRIES {
        match std::fs::create_dir(dir) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // 等待与补建之间被并发建好且是真实目录 → 等价已恢复。
                let real_dir = std::fs::symlink_metadata(dir)
                    .map(|meta| !meta.file_type().is_symlink())
                    .unwrap_or(false);
                if real_dir {
                    return Ok(());
                }
                last_err = Some(e);
            }
            Err(e) => last_err = Some(e),
        }
        std::thread::sleep(RESTORE_CREATE_RETRY_DELAY);
    }
    Err(last_err.expect("at least one restore attempt"))
}

/// 目录挂载到 `mount_point`。见模块文档的 WinFSP 目录挂载语义。
///
/// 卷参数：512B 扇区 / 8 扇区块、255 组件长度、保留大小写 + Unicode、
/// 卷序列号来自 [`VfsRegistry`] 配置、`post_cleanup_when_modified_only`
/// 减少冗余 Cleanup。`file_info_timeout` 1000ms（WinFSP 默认节拍），配合
/// 固定 stat/真实 mtime 足够 git 正确性。
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
            // 挂载失败：等 winfsp 侧 junction 消失（若已创建）后恢复调用方目录。
            if needs_restore {
                if let Err(e) = restore_mount_point_dir(mount_point) {
                    tracing::error!(
                        mount_point = %mount_point.display(),
                        error = %e,
                        "failed to restore the mount directory after a failed mount"
                    );
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_junction(link: &Path, target: &Path) {
        let out = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .expect("run mklink");
        assert!(
            out.status.success(),
            "mklink /J failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn is_junction(path: &Path) -> bool {
        std::fs::symlink_metadata(path)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false)
    }

    #[test]
    fn restore_creates_missing_mount_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("worktrees");
        restore_mount_point_dir(&mount).unwrap();
        assert!(mount.is_dir());
        assert!(!is_junction(&mount), "restored dir must be a real directory");
        assert!(
            std::fs::read_dir(&mount).unwrap().next().is_none(),
            "restored dir must be empty"
        );
    }

    #[test]
    fn restore_is_idempotent_for_existing_real_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("worktrees");
        std::fs::create_dir(&mount).unwrap();
        std::fs::write(mount.join("keep.txt"), b"x").unwrap();
        restore_mount_point_dir(&mount).unwrap();
        assert!(
            mount.join("keep.txt").is_file(),
            "an existing real directory must not be touched"
        );
    }

    #[test]
    fn restore_waits_for_lingering_junction_to_vanish() {
        // B070100 回归：junction 仍在（WinFSP 的 FILE_DELETE_ON_CLOSE 删除
        // 异步未完成）时立即补建必失败；restore 必须等它消失再建真实空目录。
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real-target");
        std::fs::create_dir(&target).unwrap();
        let mount = dir.path().join("worktrees");
        make_junction(&mount, &target);
        assert!(is_junction(&mount));

        // 模拟 WinFSP 侧迟到的删除：250ms 后 junction 才消失。
        let late = mount.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            std::fs::remove_dir(&late).unwrap();
        });

        restore_mount_point_dir(&mount).unwrap();
        assert!(!is_junction(&mount), "lingering junction must be gone");
        assert!(mount.is_dir(), "empty mount dir must be restored");
        assert!(
            std::fs::read_dir(&mount).unwrap().next().is_none(),
            "restored dir must be empty"
        );
    }

    #[test]
    fn restore_times_out_and_reports_when_junction_never_goes_away() {
        // 恢复失败不得静默：junction 一直不消失时必须报错，且不碰活 junction。
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real-target");
        std::fs::create_dir(&target).unwrap();
        let mount = dir.path().join("worktrees");
        make_junction(&mount, &target);

        let err = restore_mount_point_dir(&mount)
            .expect_err("restore must not silently build over a live junction");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(err.to_string().contains("still a junction"), "error: {err}");
        assert!(is_junction(&mount), "junction must stay untouched");
        std::fs::remove_dir(&mount).expect("remove junction for cleanup");
    }
}
