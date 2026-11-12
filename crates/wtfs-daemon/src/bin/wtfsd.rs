//! `wtfsd` —— WorktreeFS daemon 入口（spec §5.3）。
//!
//! 生命周期：读取参数 → 初始化日志 → 挂载 + index watcher → 管道服务；
//! `Stop` 请求（`wtfs unmount`）后卸载退出。以 detached 进程被
//! `wtfs mount` 拉起，也可前台运行。

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

use wtfs_daemon::config::{
    DaemonConfig, DEFAULT_PIPE_NAME, DEFAULT_POLL_INTERVAL_MS, DEFAULT_TEMPLATE_MAX_AGE_DAYS,
};
use wtfs_daemon::daemon::run_foreground;

#[derive(Parser)]
#[command(
    name = "wtfsd",
    version,
    about = "WorktreeFS daemon —— 挂载 <repo>/.tower/worktrees 并监听 worktree index"
)]
struct Args {
    /// 主仓库根。
    #[arg(long)]
    repo: PathBuf,

    /// overlay/模板存储根（默认 %LOCALAPPDATA%\wtfs\repos\<repo-id>）。
    #[arg(long)]
    store_root: Option<PathBuf>,

    /// 挂载点（默认 <repo>/.tower/worktrees）。
    #[arg(long)]
    mount_point: Option<PathBuf>,

    /// 控制管道名。
    #[arg(long, default_value = DEFAULT_PIPE_NAME)]
    pipe: String,

    /// index watcher 轮询周期（毫秒）。
    #[arg(long, default_value_t = DEFAULT_POLL_INTERVAL_MS)]
    poll_ms: u64,

    /// gc 旧模板年龄阈值（天）。
    #[arg(long, default_value_t = DEFAULT_TEMPLATE_MAX_AGE_DAYS)]
    template_max_age_days: u64,

    /// 关闭 dedupe（全量 copy 兜底/对照）。
    #[arg(long)]
    no_dedupe: bool,

    /// RUST_LOG 未设置时的日志级别。
    #[arg(long, default_value = "info")]
    log_level: String,
}

fn main() {
    let args = Args::parse();
    let mut cfg = DaemonConfig::for_repo(&args.repo);
    if let Some(store_root) = args.store_root {
        cfg.store_root = store_root;
    }
    if let Some(mount_point) = args.mount_point {
        cfg.mount_point = wtfs_daemon::config::to_absolute(mount_point);
    }
    cfg.pipe_name = args.pipe;
    cfg.poll_interval = Duration::from_millis(args.poll_ms.max(50));
    cfg.template_max_age = Duration::from_secs(args.template_max_age_days * 24 * 3600);
    cfg.dedupe_enabled = !args.no_dedupe;
    cfg.log_level = args.log_level;

    if let Err(e) = run_foreground(cfg) {
        eprintln!("wtfsd: {e:#}");
        std::process::exit(1);
    }
}
