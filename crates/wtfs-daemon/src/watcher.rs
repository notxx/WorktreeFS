//! index watcher（spec §5.3/§6.3）：唯一允许监听 index 的组件是 daemon。
//!
//! 轮询 `<common_git_dir>/worktrees/*`（M0 用轮询：稳态开销可忽略，且免于
//! ReadDirectoryChangesW 的缓冲竞争）。`git worktree add` 的收尾动作是把
//! `index.lock` 原子改名成 `index`，因此“`index` 存在 && 无 `index.lock` &&
//! 连续两轮 size+mtime 稳定”即可判定写入已关闭：
//!
//! 1. worktree 未注册 → 兜底 [`VfsRegistry::auto_register`]（mkdir 钩子未
//!    生效/daemon 晚启动的场景）；
//! 2. 写 `<gitdir>/config.worktree` 三项 stat 配置 + 确保
//!    `extensions.worktreeConfig = true`（[`crate::worktree_config`]）；
//! 3. [`VfsRegistry::mark_ready`] 完成 `initializing → ready` 切换；
//! 4. 准备连续失败 [`MAX_PREPARE_ATTEMPTS`] 轮 → [`VfsRegistry::mark_failed`]，
//!    不允许 worker 在半初始化的 worktree 上启动（spec §6.3.6）。
//!
//! worktree 元数据目录消失（`git worktree remove`）→ 注销 overlay 路由；
//! upper/DB 行残留由 `wtfs gc` 的孤儿清理回收（spec §6.6）。全流程幂等：
//! 重启/重扫对已 ready 的 worktree 无副作用。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use parking_lot::Mutex;
use wtfs_core::{OverlayStore, WorktreeState};
use wtfs_vfs::VfsRegistry;

use crate::worktree_config;

/// 单个 worktree 准备动作的最大重试轮数；超过后 `mark_failed`。
const MAX_PREPARE_ATTEMPTS: u32 = 3;

pub struct IndexWatcher {
    common_git_dir: PathBuf,
    registry: Arc<VfsRegistry>,
    state: Mutex<WatcherState>,
}

#[derive(Default)]
struct WatcherState {
    /// 已 settled（ready 或 failed）的 worktree → name；不再处理。
    settled: HashSet<String>,
    /// 准备失败的累计重试轮数。
    attempts: HashMap<String, u32>,
    /// 等待稳定确认的 index → 上一轮 (size, mtime_ns)。
    last_seen: HashMap<String, (u64, i64)>,
}

impl IndexWatcher {
    pub fn new(common_git_dir: impl Into<PathBuf>, registry: Arc<VfsRegistry>) -> Self {
        Self {
            common_git_dir: common_git_dir.into(),
            registry,
            state: Mutex::new(WatcherState::default()),
        }
    }

    /// 单轮扫描。整体 IO 失败（如 git 目录暂不可读）返回 `Err`，循环层记录后
    /// 下一轮重试；单 worktree 的失败在内部计数，不中断扫描。
    pub fn scan_once(&self) -> std::io::Result<()> {
        let dir = self.common_git_dir.join("worktrees");
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            // 元数据根目录整个没了（最后一个 worktree 被 remove）：
            // 全部 settled 名单按“已消失”处理。
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.process_removals(&HashSet::new());
                return Ok(());
            }
            Err(e) => return Err(e),
        };

        let mut seen = HashSet::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !entry.path().is_dir() {
                continue;
            }
            seen.insert(name.clone());
            if self.state.lock().settled.contains(&name) {
                continue;
            }
            if self.settle_if_already_ready(&name) {
                continue;
            }
            self.evaluate(&name, &entry.path());
        }

        // 元数据目录消失 → 注销 overlay（路由移除；upper 归 gc 回收）。
        self.process_removals(&seen);
        Ok(())
    }

    /// settled 名单里已不在 `seen` 中的 worktree：删 DB 行（级联清子表）+ 摘
    /// 路由；upper 目录随后变孤儿，由 `wtfs gc` 回收（spec §6.6）。
    fn process_removals(&self, seen: &HashSet<String>) {
        let vanished: Vec<String> = {
            let mut state = self.state.lock();
            let gone: Vec<String> = state
                .settled
                .iter()
                .filter(|n| !seen.contains(*n))
                .cloned()
                .collect();
            for name in &gone {
                state.settled.remove(name);
                state.attempts.remove(name);
                state.last_seen.remove(name);
            }
            gone
        };
        for name in &vanished {
            if let Some(overlay) = self.registry.get(name) {
                if let Err(e) = overlay.store().unregister(overlay.worktree_id()) {
                    tracing::warn!(worktree = %name, error = %e, "failed to unregister row");
                }
            }
            if self.registry.unregister(name) {
                tracing::info!(worktree = %name, "worktree metadata gone; overlay unregistered");
            }
        }
    }

    /// 轮询循环（daemon 后台线程入口）。
    pub fn run(&self, poll_interval: Duration, stop: &AtomicBool) {
        tracing::info!(
            common_git_dir = %self.common_git_dir.display(),
            "index watcher started"
        );
        while !stop.load(Ordering::Relaxed) {
            if let Err(e) = self.scan_once() {
                tracing::warn!(error = %e, "index watcher scan failed");
            }
            let mut waited = Duration::ZERO;
            while waited < poll_interval && !stop.load(Ordering::Relaxed) {
                let step = poll_interval.min(Duration::from_millis(50));
                std::thread::sleep(step);
                waited += step;
            }
        }
        tracing::info!("index watcher stopped");
    }

    /// 启动后台轮询线程；返回停止开关（join 由 daemon 关停路径负责）。
    pub fn spawn(
        self: &Arc<Self>,
        poll_interval: Duration,
    ) -> (Arc<AtomicBool>, std::thread::JoinHandle<()>) {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let this = self.clone();
        let join = std::thread::Builder::new()
            .name("wtfs-index-watcher".to_string())
            .spawn(move || this.run(poll_interval, &thread_stop))
            .expect("spawn index watcher thread");
        (stop, join)
    }

    /// DB/注册表里已是 ready（daemon 崩溃重启后的重扫）→ 直接 settled。
    fn settle_if_already_ready(&self, name: &str) -> bool {
        let ready = match self.registry.get(name) {
            Some(overlay) => overlay
                .store()
                .state(overlay.worktree_id())
                .ok()
                .flatten()
                .map(|s| s == WorktreeState::Ready)
                .unwrap_or(false),
            None => false,
        };
        if ready {
            self.state.lock().settled.insert(name.to_string());
        }
        ready
    }

    /// index 是否已出现且关闭；已稳定则执行 ready 切换。
    fn evaluate(&self, name: &str, gitdir: &Path) {
        let index = gitdir.join("index");
        let lock = gitdir.join("index.lock");
        if lock.exists() {
            // 写入中：等待关闭。
            self.state.lock().last_seen.remove(name);
            return;
        }
        let meta = match std::fs::metadata(&index) {
            Ok(meta) => meta,
            // checkout 尚未写到 index；等待。
            Err(_) => {
                self.state.lock().last_seen.remove(name);
                return;
            }
        };
        let size = meta.len();
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        let stamp = (size, mtime_ns);
        let stable = {
            let mut state = self.state.lock();
            match state.last_seen.get(name) {
                Some(prev) if *prev == stamp => true,
                _ => {
                    state.last_seen.insert(name.to_string(), stamp);
                    false
                }
            }
        };
        if stable {
            self.prepare_and_settle(name, gitdir);
        }
    }

    /// 配置写入 + ready 切换；连续失败达上限则 `mark_failed`。
    fn prepare_and_settle(&self, name: &str, gitdir: &Path) {
        match self.prepare(name, gitdir) {
            Ok(()) => {
                let mut state = self.state.lock();
                state.settled.insert(name.to_string());
                state.attempts.remove(name);
                state.last_seen.remove(name);
                tracing::info!(worktree = %name, "worktree ready (index closed)");
            }
            Err(e) => {
                let failed = {
                    let mut state = self.state.lock();
                    let attempts = state.attempts.entry(name.to_string()).or_insert(0);
                    *attempts += 1;
                    let give_up = *attempts >= MAX_PREPARE_ATTEMPTS;
                    if give_up {
                        state.settled.insert(name.to_string());
                        state.last_seen.remove(name);
                    }
                    give_up
                };
                tracing::warn!(
                    worktree = %name,
                    error = %e,
                    "worktree prepare failed",
                );
                if failed {
                    if let Some(overlay) = self.registry.get(name) {
                        if let Err(mark) = overlay.mark_failed() {
                            tracing::error!(worktree = %name, error = %mark, "mark_failed failed");
                        }
                    }
                    tracing::error!(worktree = %name, "giving up after repeated failures; marked failed");
                }
            }
        }
    }

    /// 一次准备动作：注册兜底 → config.worktree → 扩展开关 → mark_ready。
    fn prepare(&self, name: &str, gitdir: &Path) -> anyhow::Result<()> {
        if self.registry.get(name).is_none() {
            // mkdir 钩子未生效（daemon 晚于 git 启动等）：补注册。
            self.registry
                .auto_register(name)
                .map_err(|e| anyhow::anyhow!("auto_register {name}: {e}"))?;
        }
        worktree_config::write_worktree_config(gitdir)
            .map_err(|e| anyhow::anyhow!("write config.worktree: {e}"))?;
        worktree_config::ensure_worktree_config_extension(&self.common_git_dir.join("config"))
            .map_err(|e| anyhow::anyhow!("ensure extensions.worktreeConfig: {e}"))?;
        self.registry
            .mark_ready(name)
            .map_err(|e| anyhow::anyhow!("mark_ready {name}: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wtfs_vfs::VfsConfig;

    /// fake 布局：`<base>/git/worktrees/<name>/`（common git dir = <base>/git）。
    struct Fixture {
        dir: tempfile::TempDir,
        watcher: IndexWatcher,
        registry: Arc<VfsRegistry>,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path().join("git");
        std::fs::create_dir_all(git.join("worktrees")).unwrap();
        let registry = Arc::new(VfsRegistry::open(VfsConfig::new(
            dir.path().join("repo"),
            dir.path().join("store"),
        ))
        .unwrap());
        let watcher = IndexWatcher::new(git, registry.clone());
        Fixture { dir, watcher, registry }
    }

    fn write_index(f: &Fixture, name: &str, content: &str) -> PathBuf {
        let gitdir = f.dir.path().join("git").join("worktrees").join(name);
        std::fs::create_dir_all(&gitdir).unwrap();
        let index = gitdir.join("index");
        std::fs::write(&index, content).unwrap();
        index
    }

    #[test]
    fn stable_index_triggers_ready_and_config_writes() {
        let f = fixture();
        let index = write_index(&f, "wt-3", "GIT index bytes");
        // mtime 粒度依赖文件系统：显式回拨 mtime，保证两轮之间不变化。
        let older = std::time::SystemTime::now() - Duration::from_secs(2);
        set_mtime(&index, older);

        f.watcher.scan_once().unwrap();
        assert!(
            f.registry.get("wt-3").is_none() || {
                let overlay = f.registry.get("wt-3").unwrap();
                overlay.store().state(overlay.worktree_id()).unwrap()
                    != Some(WorktreeState::Ready)
            },
            "单轮采样不得切换 ready"
        );

        f.watcher.scan_once().unwrap();
        let overlay = f.registry.get("wt-3").expect("已注册");
        assert_eq!(
            overlay.store().state(overlay.worktree_id()).unwrap(),
            Some(WorktreeState::Ready)
        );

        // config.worktree + extensions.worktreeConfig 已写。
        let config = f
            .dir
            .path()
            .join("git")
            .join("worktrees")
            .join("wt-3")
            .join("config.worktree");
        let content = std::fs::read_to_string(config).unwrap();
        for (key, value) in worktree_config::REQUIRED_KEYS {
            assert!(content.contains(&format!("{key} = {value}")));
        }
        let common = std::fs::read_to_string(f.dir.path().join("git").join("config")).unwrap();
        assert!(common.contains("worktreeConfig = true"));
    }

    #[test]
    fn index_lock_and_single_sample_block_ready() {
        let f = fixture();
        let index = write_index(&f, "wt-1", "data");
        let older = std::time::SystemTime::now() - Duration::from_secs(2);
        set_mtime(&index, older);
        std::fs::write(
            f.dir
                .path()
                .join("git")
                .join("worktrees")
                .join("wt-1")
                .join("index.lock"),
            b"",
        )
        .unwrap();
        f.watcher.scan_once().unwrap();
        f.watcher.scan_once().unwrap();
        assert!(f.registry.get("wt-1").is_none(), "index.lock 在写：不得 ready");

        // 关闭后只扫一轮：采样不足两轮，仍不切换。
        std::fs::remove_file(
            f.dir
                .path()
                .join("git")
                .join("worktrees")
                .join("wt-1")
                .join("index.lock"),
        )
        .unwrap();
        f.watcher.scan_once().unwrap();
        assert!(f.registry.get("wt-1").is_none());
    }

    #[test]
    fn removal_unregisters_overlay() {
        let f = fixture();
        let index = write_index(&f, "wt-2", "data");
        let older = std::time::SystemTime::now() - Duration::from_secs(2);
        set_mtime(&index, older);
        f.watcher.scan_once().unwrap();
        f.watcher.scan_once().unwrap();
        assert!(f.registry.get("wt-2").is_some());

        std::fs::remove_dir_all(f.dir.path().join("git").join("worktrees").join("wt-2")).unwrap();
        f.watcher.scan_once().unwrap();
        assert!(f.registry.get("wt-2").is_none(), "元数据消失需注销");
    }

    #[test]
    fn already_ready_is_settled_without_rewrites() {
        let f = fixture();
        let index = write_index(&f, "wt-4", "data");
        let older = std::time::SystemTime::now() - Duration::from_secs(2);
        set_mtime(&index, older);
        // 手动走完 ready（等价崩溃前的状态）。
        let overlay = f.registry.auto_register("wt-4").unwrap();
        overlay.mark_ready().unwrap();
        std::fs::remove_file(
            f.dir
                .path()
                .join("git")
                .join("worktrees")
                .join("wt-4")
                .join("config.worktree"),
        )
        .ok();

        f.watcher.scan_once().unwrap();
        f.watcher.scan_once().unwrap();
        // 已 ready 的 worktree 不重写配置（幂等）。
        assert!(
            !f.dir
                .path()
                .join("git")
                .join("worktrees")
                .join("wt-4")
                .join("config.worktree")
                .exists()
        );
    }

    fn set_mtime(path: &Path, t: std::time::SystemTime) {
        let file = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap();
        file.set_modified(t).unwrap();
    }
}
