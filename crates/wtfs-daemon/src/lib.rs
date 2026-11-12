//! wtfs-daemon —— wtfsd：挂载生命周期 / 控制管道（`\\.\pipe\wtfs`）/
//! 模板与 overlay 注册表 / index 监听与 ready 切换 / gc（spec §5.3/§6.3）。
//!
//! M0（dedupe-only，无 hook/shim）职责：
//!
//! 1. 挂载 `<repo>/.tower/worktrees`（主仓库保持原生），持有
//!    [`MountedVolume`]；drop/停机即卸载并恢复空挂载点目录。
//! 2. 监听 `<main>/.git/worktrees/<name>/index`（轮询）：写入关闭后写
//!    `config.worktree` 三项 stat 配置 + 确保 `extensions.worktreeConfig=true`，
//!    调 [`VfsRegistry::mark_ready`] 完成 `initializing → ready` 切换；
//!    失败重试若干轮后 [`VfsRegistry::mark_failed`]。
//! 3. 控制管道一行 JSON 协议：`status` / `gc`(dry-run/apply) / `stop`；
//!    CLI（wtfs-cli）经同一协议远控，也可离线直连存储。
//! 4. gc：dead 目录（无 `.git`/未注册）、孤儿 upper、无引用旧模板；
//!    只认 Tower `wt-N` 命名，绝不碰 `dashboard-fork-*` 等目录。
//!
//! daemon 不读写 `.tower/comms` 状态（该目录只读透传给 Tower）。
//!
//! 模块：
//!
//! * [`config`]   —— 路径布局/管道名/轮询与 gc 参数。
//! * [`logging`]  —— tracing 落盘 `%LOCALAPPDATA%\wtfs\logs`。
//! * [`worktree_config`] —— config.worktree 与 extensions 开关的幂等写入。
//! * [`watcher`]  —— index watcher：ready/failed 切换与注销。
//! * [`gc`]       —— 清理计划与执行。
//! * [`pipe`]     —— 管道服务端/客户端与协议。
//! * [`daemon`]   —— 编排：`Daemon` 生命周期与 `run_foreground`。

pub mod config;
pub mod daemon;
pub mod gc;
pub mod logging;
pub mod pipe;
pub mod watcher;
pub mod worktree_config;

pub use config::{DaemonConfig, DEFAULT_PIPE_NAME, DEFAULT_POLL_INTERVAL_MS, DEFAULT_TEMPLATE_MAX_AGE_DAYS};
pub use daemon::{run_foreground, Daemon, StatusReport, WorktreeStatus};
pub use gc::{GcContext, GcPlan, GcReport};
pub use pipe::{PipeControl, PipeRequest, PipeServer};
pub use watcher::IndexWatcher;
pub use worktree_config::{ensure_worktree_config_extension, write_worktree_config, REQUIRED_KEYS};

#[cfg(test)]
mod tests {
    #[test]
    fn public_surface_is_wired() {
        // 冒烟：公共类型/函数可达（编译期即验证，此处防误删 re-export）。
        let _ = crate::DEFAULT_PIPE_NAME;
        let _ = crate::write_worktree_config;
        let _ = crate::ensure_worktree_config_extension;
        assert_eq!(crate::DEFAULT_PIPE_NAME, r"\\.\pipe\wtfs");
    }
}
