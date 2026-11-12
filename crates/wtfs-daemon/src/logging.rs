//! wtfsd 日志（spec §8.1：`%LOCALAPPDATA%\wtfs\logs\wtfsd.log`）。
//!
//! daemon 以 detached 进程运行，stdout 无人消费，因此写追加式日志文件；
//! 级别取 `RUST_LOG`，未设置时用 [`DaemonConfig::log_level`]。

use std::fs::OpenOptions;
use std::sync::Mutex;

use tracing_subscriber::EnvFilter;

use crate::config::DaemonConfig;

/// 初始化全局 tracing；失败只向 stderr 报告，不阻断 daemon 启动。
pub fn init(cfg: &DaemonConfig) {
    if let Err(e) = init_inner(cfg) {
        eprintln!("wtfsd: logging init failed: {e:#}");
    }
}

fn init_inner(cfg: &DaemonConfig) -> anyhow::Result<()> {
    std::fs::create_dir_all(&cfg.log_dir)?;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(cfg.log_dir.join("wtfsd.log"))?;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&cfg.log_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(Mutex::new(file))
        .with_ansi(false)
        .try_init()
        .map_err(|e| anyhow::anyhow!("tracing init: {e}"))?;
    Ok(())
}
