//! wtfsd 编排（spec §5.3/§6.1）：挂载生命周期 → 注册表 → index watcher →
//! 控制管道，统一在一个 `Daemon` 里。
//!
//! 生命周期：
//!
//! 1. [`Daemon::start`]：校验仓库 → 打开 [`VfsRegistry`]（模板/overlay 存储
//!    就绪）→ `mount_directory(<repo>/.tower/worktrees)` → 启动 index watcher。
//!    挂载点目录由 VFS 移除/重建（WinFSP 目录挂载语义），daemon 不预建。
//! 2. [`Daemon::serve_until_stopped`]：控制管道阻塞服务，`Stop` 请求或外部
//!    [`Daemon::request_stop`] 后返回。
//! 3. [`Daemon::shutdown`]/drop：watcher、管道线程 join；[`MountedVolume`]
//!    卸载并恢复空挂载点目录。
//!
//! 崩溃恢复：注册表与状态机元数据持久在 SQLite；重启后 auto_register 走
//! reopen 路径，index watcher 对已 ready 的 worktree 幂等跳过。
//! `Daemon` 经 `Arc` 与管道 handler 共享，全部可变状态用锁内化。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use wtfs_cas::{CatFile, HydrationCache, Manifest};
use wtfs_core::OverlayStore;
use wtfs_vfs::{
    CasShared, MountedVolume, VfsConfig, VfsRegistry, register_cas_worktree,
};

use crate::config::DaemonConfig;
use crate::gc::{self, GcContext};
use crate::logging;
use crate::pipe::{self, PipeControl, PipeRequest, PipeServer};
use crate::watcher::IndexWatcher;

pub struct Daemon {
    cfg: DaemonConfig,
    registry: Arc<VfsRegistry>,
    volume: Mutex<Option<MountedVolume>>,
    watcher_stop: Mutex<Option<Arc<AtomicBool>>>,
    watcher_join: Mutex<Option<std::thread::JoinHandle<()>>>,
    pipe: Mutex<Option<PipeControl>>,
    pipe_join: Mutex<Option<std::thread::JoinHandle<()>>>,
    started_at: Instant,
    stop: Arc<AtomicBool>,
}

/// `wtfs status` / pipe `status` 的报告。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusReport {
    pub pid: u32,
    pub uptime_ms: u64,
    pub mounted: bool,
    pub mount_point: String,
    pub repo_root: String,
    pub store_root: String,
    pub pipe_name: String,
    pub dedupe_enabled: bool,
    pub common_git_dir: String,
    pub worktrees: Vec<WorktreeStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeStatus {
    pub name: String,
    pub worktree_id: i64,
    pub state: String,
}

impl Daemon {
    /// 启动：挂载 + watcher（测试可用 [`Daemon::start_without_mount`] 只起
    /// watcher/管道，无需 WinFSP）。
    pub fn start(cfg: DaemonConfig) -> anyhow::Result<Arc<Self>> {
        Self::start_inner(cfg, true)
    }

    /// 无 WinFSP 环境（单元/无挂载集成测试）用：只起 watcher，不挂载。
    pub fn start_without_mount(cfg: DaemonConfig) -> anyhow::Result<Arc<Self>> {
        Self::start_inner(cfg, false)
    }

    fn start_inner(cfg: DaemonConfig, mount: bool) -> anyhow::Result<Arc<Self>> {
        if !cfg.repo_root.is_dir() {
            anyhow::bail!("repo root not found: {}", cfg.repo_root.display());
        }
        let mut vfs_cfg = VfsConfig::new(&cfg.repo_root, &cfg.store_root);
        vfs_cfg.dedupe_enabled = cfg.dedupe_enabled;
        let registry = Arc::new(VfsRegistry::open(vfs_cfg)?);

        let volume = if mount {
            // 挂载点父目录（.tower/）由 Tower 预建；缺则补建。挂载点目录本身
            // 交给 mount_directory 处理（移除空目录 → winfsp 重建 → 卸载恢复）。
            if let Some(parent) = cfg.mount_point.parent() {
                std::fs::create_dir_all(parent)?;
            }
            Some(
                wtfs_vfs::mount_directory(&cfg.mount_point, registry.clone()).map_err(|e| {
                    anyhow::anyhow!("mount {}: {e}", cfg.mount_point.display())
                })?,
            )
        } else {
            None
        };

        let common_git_dir = probe_common_git_dir(&cfg);
        let watcher = Arc::new(IndexWatcher::new(common_git_dir, registry.clone()));
        let (watcher_stop, watcher_join) = watcher.spawn(cfg.poll_interval);

        Ok(Arc::new(Self {
            cfg,
            registry,
            volume: Mutex::new(volume),
            watcher_stop: Mutex::new(Some(watcher_stop)),
            watcher_join: Mutex::new(Some(watcher_join)),
            pipe: Mutex::new(None),
            pipe_join: Mutex::new(None),
            started_at: Instant::now(),
            stop: Arc::new(AtomicBool::new(false)),
        }))
    }

    /// 阻塞服务控制管道直到停机请求；返回即进入卸载收尾。
    pub fn serve_until_stopped(self: &Arc<Self>) -> anyhow::Result<()> {
        let daemon = self.clone();
        let handler: pipe::Handler = Arc::new(move |request| daemon.handle_request(request));
        let (control, join) = PipeServer::spawn(self.cfg.pipe_name.clone(), handler)
            .map_err(|e| anyhow::anyhow!("open pipe {}: {e}", self.cfg.pipe_name))?;
        *self.pipe.lock() = Some(control);
        *self.pipe_join.lock() = Some(join);
        tracing::info!(
            mount_point = %self.cfg.mount_point.display(),
            pipe = %self.cfg.pipe_name,
            "wtfsd serving"
        );
        while !self.stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
        tracing::info!("stop requested; shutting down");
        if let Some(control) = self.pipe.lock().take() {
            control.stop();
        }
        if let Some(join) = self.pipe_join.lock().take() {
            let _ = join.join();
        }
        Ok(())
    }

    /// 外部（测试等）触发停机。
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(control) = &*self.pipe.lock() {
            control.stop();
        }
    }

    /// 停 watcher、卸载卷。幂等；drop 兜底再调一次。
    pub fn shutdown(&self) {
        if let Some(stop) = self.watcher_stop.lock().take() {
            stop.store(true, Ordering::Relaxed);
        }
        if let Some(join) = self.watcher_join.lock().take() {
            let _ = join.join();
        }
        if let Some(volume) = self.volume.lock().take() {
            volume.unmount();
        }
        self.dump_profile_report();
    }

    /// profiler 启用时（`WTFS_VFS_PROFILE=1`）把累计报告 best-effort 落到
    /// `log_dir/vfs-profile.txt` 并入日志（M13/B130100）。
    fn dump_profile_report(&self) {
        if !wtfs_core::profiling::enabled() {
            return;
        }
        let report = wtfs_core::profiling::report();
        tracing::info!(target: "vfs_profile", "\n{}", report);
        let path = self.cfg.log_dir.join("vfs-profile.txt");
        match (std::fs::create_dir_all(&self.cfg.log_dir), std::fs::write(&path, &report)) {
            (Ok(()), Ok(())) => {
                tracing::info!(target: "vfs_profile", path = %path.display(), "profile report written");
            }
            (Err(e), _) | (_, Err(e)) => {
                tracing::warn!(target: "vfs_profile", path = %path.display(), error = %e, "failed to write profile report");
            }
        }
    }

    pub fn config(&self) -> &DaemonConfig {
        &self.cfg
    }

    pub fn registry(&self) -> &Arc<VfsRegistry> {
        &self.registry
    }

    pub fn is_mounted(&self) -> bool {
        self.volume.lock().is_some()
    }

    pub fn status(&self) -> StatusReport {
        let mut worktrees = Vec::new();
        let mut names = self.registry.names();
        names.sort();
        for name in names {
            // M18：同一注册表可同时挂 B1 overlay 与 C CAS 视图。B1 报 DB
            // 状态机；CAS 视图注册即 ready（无状态机）。
            let state = match self.registry.get_backend(&name) {
                Some(backend) => match backend.as_overlay() {
                    Some(overlay) => overlay
                        .store()
                        .state(overlay.worktree_id())
                        .ok()
                        .flatten()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "unknown".to_string()),
                    None => "ready (cas)".to_string(),
                },
                None => "unknown".to_string(),
            };
            let worktree_id = match self.registry.get_backend(&name) {
                Some(backend) => match backend.as_overlay() {
                    Some(overlay) => overlay.worktree_id().0,
                    // CAS 视图无 DB 行；id 与 FNV 派生路由无关联，报 0。
                    None => 0,
                },
                None => 0,
            };
            worktrees.push(WorktreeStatus {
                name,
                worktree_id,
                state,
            });
        }
        StatusReport {
            pid: std::process::id(),
            uptime_ms: self.started_at.elapsed().as_millis() as u64,
            mounted: self.volume.lock().is_some(),
            mount_point: self.cfg.mount_point.to_string_lossy().into_owned(),
            repo_root: self.cfg.repo_root.to_string_lossy().into_owned(),
            store_root: self.cfg.store_root.to_string_lossy().into_owned(),
            pipe_name: self.cfg.pipe_name.clone(),
            dedupe_enabled: self.cfg.dedupe_enabled,
            common_git_dir: probe_common_git_dir(&self.cfg).to_string_lossy().into_owned(),
            worktrees,
        }
    }

    /// 管道请求分发（CLI `wtfs status/gc/unmount` 的数据来源）。
    fn handle_request(self: &Arc<Self>, request: &PipeRequest) -> serde_json::Value {
        match request {
            PipeRequest::Status => {
                let report = self.status();
                match serde_json::to_value(report) {
                    Ok(value) => pipe::ok_value(serde_json::json!({ "status": value })),
                    Err(e) => pipe::err_value(format!("serialize status: {e}")),
                }
            }
            PipeRequest::RegisterCas {
                name,
                real_worktree,
                head,
                absorbing,
            } => self.handle_register_cas(name, Path::new(real_worktree), head, *absorbing),
            PipeRequest::FinishCas { name } => {
                match self.registry.get_backend(name).and_then(|b| b.as_cas().cloned()) {
                    Some(cas) => {
                        cas.set_absorbing(false);
                        pipe::ok_value(serde_json::json!({}))
                    }
                    None => pipe::err_value(format!("cas view not registered: {name}")),
                }
            }
            PipeRequest::RemoveCas { name } => {
                self.registry.unregister(name);
                pipe::ok_value(serde_json::json!({}))
            }
            PipeRequest::Gc { dry_run, max_age_days } => self.handle_gc(*dry_run, *max_age_days),
            PipeRequest::Stop => {
                self.stop.store(true, Ordering::Relaxed);
                // 唤醒 accept/serve 循环，尽快进入卸载收尾。
                if let Some(control) = &*self.pipe.lock() {
                    control.stop();
                }
                pipe::ok_value(serde_json::json!({}))
            }
        }
    }

    fn handle_register_cas(
        &self,
        name: &str,
        real_worktree: &Path,
        head: &str,
        absorbing: bool,
    ) -> serde_json::Value {
        let repo = &self.cfg.repo_root;
        let manifest = match Manifest::load(repo, head) {
            Ok(m) => m,
            Err(e) => return pipe::err_value(format!("manifest load: {e}")),
        };
        let shared = Arc::new(CasShared {
            cat: Arc::new(CatFile::new(repo)),
            cache: Arc::new(HydrationCache::open(&self.cfg.store_root)),
        });
        match register_cas_worktree(
            self.registry.as_ref(),
            name,
            real_worktree,
            Arc::new(manifest),
            shared,
        ) {
            Ok(cas) => {
                cas.set_absorbing(absorbing);
                pipe::ok_value(serde_json::json!({}))
            }
            Err(e) => pipe::err_value(format!("register cas: {e}")),
        }
    }

    fn handle_gc(&self, dry_run: bool, max_age_days: Option<u64>) -> serde_json::Value {
        let max_age = max_age_days
            .map(|days| Duration::from_secs(days.saturating_mul(24 * 3600)))
            .unwrap_or(self.cfg.template_max_age);
        let ctx = GcContext {
            mount_root: &self.cfg.mount_point,
            store_root: &self.cfg.store_root,
            registry: Some(&self.registry),
            mounted: self.is_mounted(),
            max_age,
        };
        let gc_plan = match gc::plan(&ctx) {
            Ok(gc_plan) => gc_plan,
            Err(e) => return pipe::err_value(format!("gc plan: {e}")),
        };
        let plan_value = match serde_json::to_value(&gc_plan) {
            Ok(v) => v,
            Err(e) => return pipe::err_value(format!("serialize gc plan: {e}")),
        };
        if dry_run {
            return pipe::ok_value(serde_json::json!({
                "gc": { "dry_run": true, "summary": gc_plan.summary(), "plan": plan_value }
            }));
        }
        match gc::apply(&ctx, &gc_plan) {
            Ok(report) => {
                let report_value = serde_json::to_value(&report).unwrap_or_default();
                pipe::ok_value(serde_json::json!({
                    "gc": {
                        "dry_run": false,
                        "summary": gc_plan.summary(),
                        "plan": plan_value,
                        "report": report_value,
                        "report_summary": report.summary(),
                    }
                }))
            }
            Err(e) => pipe::err_value(format!("gc apply: {e}")),
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// 探测主仓库 common git dir；git 不可用时回退 `<repo>/.git`。
pub fn probe_common_git_dir(cfg: &DaemonConfig) -> std::path::PathBuf {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&cfg.repo_root)
        .args(["rev-parse", "--git-common-dir"])
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if s.is_empty() {
                cfg.common_git_dir()
            } else {
                let p = std::path::PathBuf::from(&s);
                if p.is_absolute() {
                    p
                } else {
                    cfg.repo_root.join(p)
                }
            }
        }
        _ => cfg.common_git_dir(),
    }
}

/// wtfsd 进程主路径：start + serve；返回即已完全停机卸载。
pub fn run_foreground(cfg: DaemonConfig) -> anyhow::Result<()> {
    logging::init(&cfg);
    let daemon = Daemon::start(cfg)?;
    let report = daemon.status();
    tracing::info!(
        pid = report.pid,
        mount_point = %report.mount_point,
        store_root = %report.store_root,
        pipe = %report.pipe_name,
        "wtfsd started"
    );
    let result = daemon.serve_until_stopped();
    daemon.shutdown();
    match result {
        Ok(()) => {
            tracing::info!("wtfsd stopped cleanly (unmounted)");
            Ok(())
        }
        Err(e) => {
            tracing::error!(error = %e, "wtfsd exited with error");
            Err(e)
        }
    }
}
