//! `wt-*` worktree 的 overlay 注册表：VFS 路由的核心状态（spec §4.1/§6.3）。
//!
//! * [`VfsRegistry`] 持有 `worktree 名 → Arc<Overlay>` 映射（[`DashMap`]），
//!   并提供 daemon（M4）消费的公共钩子：[`VfsRegistry::mark_ready`]
//!   （index 关闭后切换 `initializing → ready`）、[`VfsRegistry::mark_failed`]。
//! * [`VfsRegistry::auto_register`]：挂载根下 `mkdir wt-N`（`git worktree add`
//!   的第一步）时自动注册——解析模板（state.json → base ref，缺失回退
//!   HEAD；resolve 失败 → 无模板全量 copy 兜底），upper/meta 落
//!   `store_root`。状态机起点 = [`wtfs_core::WorktreeState::Initializing`]。
//! * 已注册的同名 worktree 幂等返回（崩溃恢复/重开路径）。
//!
//! 注意：M0 的 VFS 不实现 index 监听（spec §5.3 归 daemon）；`mark_ready`
//! 只是把钩子暴露出来。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dashmap::DashMap;
use wtfs_cas::{CasViewConfig, Manifest};
use wtfs_core::{
    Db, FileHashMap, Overlay, OverlayConfig, SqliteOverlayStore, SqliteTemplateStore,
    TemplateId, TemplateSpec, TemplateStore, WorktreeId, template_spec_for_worktree,
    validate_relative_path,
};

use crate::backend::{CasBackend, CasShared, ViewBackend};

/// 单个挂载的配置（daemon/CLI 构造，测试可手工填充）。
#[derive(Debug, Clone)]
pub struct VfsConfig {
    /// 主仓库根（模板 resolve 与 state.json 的基准）。
    pub repo_root: PathBuf,
    /// overlay 存储根（upper/meta.db/template store 同在此树下，spec §8.1 的
    /// `<repo-id>/` 目录）。挂载点本身不落盘任何数据。
    pub store_root: PathBuf,
    /// dedupe 开关（spec §6.3：失败兜底可整体关闭 → 全量 copy）。
    pub dedupe_enabled: bool,
    /// 虚拟卷序列号（lower 固定 stat 的 dev；WinFSP volume serial）。
    pub volume_serial: u32,
    /// 卷标签（`get_volume_info`）。
    pub volume_label: String,
}

impl VfsConfig {
    pub fn new(repo_root: impl Into<PathBuf>, store_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
            store_root: store_root.into(),
            dedupe_enabled: true,
            volume_serial: wtfs_core::DEFAULT_VOLUME_SERIAL,
            volume_label: "WorktreeFS".to_string(),
        }
    }
}

pub struct VfsRegistry {
    cfg: VfsConfig,
    /// `worktree 名（小写键）→ 条目`。WinFSP FSD 的访问检查会传大写路径
    /// （如 `\WT-1\README.md`），挂载卷按 NTFS 惯例大小写不敏感，因此
    /// 路由键统一小写；原始名保留在条目里供通知/枚举使用。
    /// M18 起条目按 [`ViewBackend`] 分派（B1 Overlay / C CasView）。
    backends: DashMap<String, RegistryEntry>,
    /// 模板 resolve（只查不建；模板创建/更新归 daemon/CLI）。
    template_store: SqliteTemplateStore,
    /// overlay 元数据（一个挂载一份；全部 worktree 共享同一 Db）。
    overlay_store: Arc<SqliteOverlayStore>,
}

struct RegistryEntry {
    name: String,
    backend: ViewBackend,
}

impl VfsRegistry {
    pub fn open(cfg: VfsConfig) -> wtfs_core::Result<Self> {
        std::fs::create_dir_all(&cfg.store_root)?;
        let template_store = SqliteTemplateStore::open(&cfg.store_root)?;
        let db = Db::open(cfg.store_root.join("meta.db"))?;
        let overlay_store = Arc::new(SqliteOverlayStore::new(db));
        Ok(Self {
            cfg,
            backends: DashMap::new(),
            template_store,
            overlay_store,
        })
    }

    pub fn config(&self) -> &VfsConfig {
        &self.cfg
    }

    pub fn overlay_store(&self) -> &Arc<SqliteOverlayStore> {
        &self.overlay_store
    }

    /// 注册（或重开）一个已构造好的 overlay。daemon（M4）恢复既有注册与
    /// 测试注入都用它；同名（大小写不敏感）重复注册幂等（返回既有实例）。
    pub fn register(&self, name: impl Into<String>, overlay: Arc<Overlay>) -> Arc<Overlay> {
        let name = name.into();
        let key = name.to_ascii_lowercase();
        match self.get(&name) {
            // 同名 B1 overlay 已在（崩溃恢复/重开路径）：幂等返回既有实例。
            Some(existing) => existing,
            None => {
                let entry = RegistryEntry {
                    name: name.clone(),
                    backend: ViewBackend::Overlay(overlay.clone()),
                };
                self.backends.insert(key, entry);
                overlay
            }
        }
    }

    /// 注册一个 CAS 视图后端（M18 显式流程；[`register_cas_worktree`] 的
    /// 底座）。同名（大小写不敏感）重复注册幂等（返回既有实例）。
    pub fn register_cas(&self, name: impl Into<String>, backend: CasBackend) -> CasBackend {
        let name = name.into();
        let key = name.to_ascii_lowercase();
        match self.get_backend(&name) {
            // 同名 CAS 视图已在（崩溃恢复/重挂）：幂等返回既有实例。
            Some(ViewBackend::Cas(existing)) => existing,
            Some(_) => backend,
            None => {
                let entry = RegistryEntry {
                    name: name.clone(),
                    backend: ViewBackend::Cas(backend.clone()),
                };
                self.backends.insert(key, entry);
                backend
            }
        }
    }

    /// 大小写不敏感查找，返回（规范名, 后端）。
    pub fn lookup_backend(&self, name: &str) -> Option<(String, ViewBackend)> {
        self.backends
            .get(&name.to_ascii_lowercase())
            .map(|e| (e.name.clone(), e.backend.clone()))
    }

    /// 大小写不敏感的后端查找（挂载路由用）。
    pub fn get_backend(&self, name: &str) -> Option<ViewBackend> {
        self.lookup_backend(name).map(|(_, backend)| backend)
    }

    /// B1 overlay 访问（daemon/watcher/测试的既有语义；CAS 视图 → `None`）。
    pub fn get(&self, name: &str) -> Option<Arc<Overlay>> {
        self.get_backend(name)
            .and_then(|backend| backend.as_overlay().cloned())
    }

    /// 已注册 worktree 名（挂载根枚举用；保留原始大小写）。
    pub fn names(&self) -> Vec<String> {
        self.backends.iter().map(|e| e.name.clone()).collect()
    }

    /// 公共钩子（daemon index watcher 调用）：`initializing → ready`。
    /// 切换后视图 = 模板 + upper（应用 whiteout/rename）。CAS 视图无此
    /// 状态机（注册即 ready）→ NotFound。
    pub fn mark_ready(&self, name: &str) -> wtfs_core::Result<()> {
        match self.get(name) {
            Some(overlay) => overlay.mark_ready(),
            None => Err(wtfs_core::Error::NotFound(format!("worktree {name:?}"))),
        }
    }

    /// 公共钩子：初始化失败标记（不允许 worker 在该 worktree 上启动）。
    pub fn mark_failed(&self, name: &str) -> wtfs_core::Result<()> {
        match self.get(name) {
            Some(overlay) => overlay.mark_failed(),
            None => Err(wtfs_core::Error::NotFound(format!("worktree {name:?}"))),
        }
    }

    /// 挂载根下 `mkdir <name>` 时的自动注册（`git worktree add` 主路径）。
    ///
    /// 模板来源（spec §6.3.4）：`.tower/comms/state.json` 里该 worktree 的
    /// base ref；缺失则回退主仓库 HEAD；resolve 失败 → 无模板（全量 copy），
    /// worktree 仍然正确。
    pub fn auto_register(&self, name: &str) -> wtfs_core::Result<Arc<Overlay>> {
        if let Some(existing) = self.get(name) {
            return Ok(existing);
        }
        // worktree 名必须是单一路径组件（挂载根的一级目录名）。
        if name.contains('/') || name.contains('\\') {
            return Err(wtfs_core::Error::InvalidPath {
                path: name.to_string(),
                reason: "worktree name must be a single component".into(),
            });
        }
        validate_relative_path(name)?;

        let worktree_id = worktree_id_for_name(name);
        let upper_root = self
            .cfg
            .store_root
            .join("worktrees")
            .join(format!("wt-{}", worktree_id.0))
            .join("upper");
        std::fs::create_dir_all(&upper_root)?;

        let (template_id, lower_root, lower_files) = self.resolve_template(name);
        let mut config = OverlayConfig::new(worktree_id, &upper_root);
        config.template_id = template_id;
        config.lower_root = lower_root;
        config.dedupe_enabled = self.cfg.dedupe_enabled;
        config.volume_serial = self.cfg.volume_serial;

        let overlay = match Overlay::new(
            config.clone(),
            lower_files.clone(),
            self.overlay_store.clone(),
        ) {
            Ok(overlay) => overlay,
            Err(wtfs_core::Error::Overlay(msg))
                if msg.contains("already registered") =>
            {
                // 行已在（上次进程崩溃后重挂）：重开，状态机/元数据持久。
                Overlay::reopen(config, lower_files, self.overlay_store.clone())?
            }
            Err(e) => return Err(e),
        };
        let overlay = Arc::new(overlay);
        Ok(self.register(name, overlay))
    }

    /// 注销（merge/teardown 后 gc；daemon 用）。仅移除路由映射；upper 文件
    /// 与 DB 行由 `OverlayStore::unregister` + gc 流程负责（spec §6.6）。
    /// CAS 视图：upper（真实 worktree 目录）与标记日志由调用方（git
    /// worktree remove / gc）处置。
    pub fn unregister(&self, name: &str) -> bool {
        self.backends
            .remove(&name.to_ascii_lowercase())
            .is_some()
    }

    /// 模板解析：返回 (模板 id, lower 树, 文件清单)。任一步失败 → None
    /// （全量 copy 兜底，spec §6.3.6）。
    fn resolve_template(&self, name: &str) -> (Option<TemplateId>, Option<PathBuf>, Option<FileHashMap>) {
        let spec = template_spec_for_worktree(&self.cfg.repo_root, name)
            .unwrap_or_else(|| TemplateSpec::new(self.cfg.repo_root.clone(), "HEAD"));
        let template = match self.template_store.resolve(&spec) {
            Ok(Some(t)) => t,
            Ok(None) => {
                tracing::info!(worktree = name, spec.base = %spec.base_ref, "no ready template; full-copy fallback");
                return (None, None, None);
            }
            Err(e) => {
                tracing::warn!(worktree = name, error = %e, "template resolve failed; full-copy fallback");
                return (None, None, None);
            }
        };
        match self.template_store.files(template.id) {
            Ok(entries) => (
                Some(template.id),
                Some(template.tree_path.clone()),
                Some(entries.into_iter().map(|f| (f.path.clone(), f)).collect::<HashMap<_, _>>()),
            ),
            Err(e) => {
                tracing::warn!(worktree = name, error = %e, "template files unavailable; full-copy fallback");
                (None, None, None)
            }
        }
    }
}

/// Windows 长路径前缀：CAS upper 直接落在真实 worktree 下，dashboard
/// 这类深层 `specs/...` 路径会超过 MAX_PATH；`std::fs` 用 `\\?\` 才能
/// 访问。已有前缀/UNC 路径保持不变。
fn long_path(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s.starts_with(r"\\?\") {
        path.to_path_buf()
    } else {
        PathBuf::from(format!(r"\\?\{s}"))
    }
}

/// M18 显式流程的挂载侧入口：把一个已 `git worktree add --no-checkout` +
/// 索引种子的 worktree 注册为 CAS 视图。流程约定（docs/m18-cas-mount.md）：
///
/// 1. 调用方经 `wtfs_cas::add_worktree_no_checkout` 建真实 worktree 目录
///    （`.git` 指针所在；**必须在挂载点之外**，否则 mkdir 会被 B1 的
///    auto_register 拦截）；
/// 2. `wtfs_cas::Manifest::load` + `wtfs_cas::seed_stat_aligned` 完成索引
///    种子（skip-worktree + stat 对齐）；
/// 3. 挂载就绪（FS 已在服务）后调用本函数注册路由；
/// 4. **注册成功后再** `wtfs_cas::clear_skip_worktree` 清位（挂载失败/
///    未就绪不得清位——清位后未挂载的 status 必报全量删除）。
///
/// 视图布局：upper = 真实 worktree 目录（`.git` 经 upper 自然可见）；meta
/// （标记日志）= `<store_root>/cas-meta/<name>`；cat-file/缓存 = 调用方
/// 注入的 [`CasShared`]（per-挂载共享）。dev 用挂载卷序列号，mtime 用
/// 固定纪元——与 `seed_stat_aligned` 写入 index 的值逐位一致。
pub fn register_cas_worktree(
    registry: &VfsRegistry,
    name: &str,
    real_worktree: &Path,
    manifest: Arc<Manifest>,
    shared: Arc<CasShared>,
) -> wtfs_core::Result<CasBackend> {
    if name.contains('/') || name.contains('\\') {
        return Err(wtfs_core::Error::InvalidPath {
            path: name.to_string(),
            reason: "worktree name must be a single component".into(),
        });
    }
    validate_relative_path(name)?;
    if let Some(backend) = registry.get_backend(name) {
        // 幂等重入（崩溃恢复/重挂）：返回既有 CAS 视图。
        return match backend.as_cas() {
            Some(c) => Ok(c.clone()),
            None => Err(wtfs_core::Error::Overlay(format!(
                "worktree {name:?} already registered as a B1 overlay"
            ))),
        };
    }
    let mut config = CasViewConfig::new(
        long_path(real_worktree),
        registry.config().store_root.join("cas-meta").join(name),
    );
    // dev 必须与 WinFSP 卷序列号一致（git lstat 的 st_dev 比对口径）。
    config.volume_serial = registry.config().volume_serial;
    config.fixed_stat_epoch_ns = wtfs_core::FIXED_STAT_EPOCH_UNIX_NS;
    let backend = CasBackend::open(config, manifest, real_worktree, shared)
        .map_err(crate::backend::map_cas_error)?;
    Ok(registry.register_cas(name, backend))
}

/// `wt-<n>` → `WorktreeId(n)`；非Tower命名派生自 FNV-1a（同 repo 内唯一）。
fn worktree_id_for_name(name: &str) -> WorktreeId {
    if let Some(rest) = name.strip_prefix("wt-") {
        if let Ok(n) = rest.parse::<i64>() {
            if n > 0 {
                return WorktreeId(n);
            }
        }
    }
    WorktreeId(fnv1a_i63(name))
}

/// worktree 名的稳定 ino（挂载根枚举用；FNV-1a 64 → 正 62 位）。
pub fn worktree_ino(name: &str) -> u64 {
    fnv1a_i63(name) as u64
}

/// FNV-1a 64 → 正 i63（`WorktreeId` 为 i64 且 SQLite INTEGER 安全）。
/// 仅用于非 `wt-N` 命名的派生 id，M0 不追求加密强度。
fn fnv1a_i63(name: &str) -> i64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    (hash & 0x3FFF_FFFF_FFFF_FFFF).max(1) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use wtfs_core::WorktreeState;
    use wtfs_core::{DedupeState, OverlayStore};

    fn registry(dir: &std::path::Path) -> Arc<VfsRegistry> {
        let cfg = VfsConfig::new(dir.join("repo"), dir.join("store"));
        Arc::new(VfsRegistry::open(cfg).unwrap())
    }

    #[test]
    fn auto_register_initializing_then_ready_hook() {
        let dir = tempfile::tempdir().unwrap();
        let reg = registry(dir.path());

        let overlay = reg.auto_register("wt-3").unwrap();
        assert_eq!(overlay.worktree_id(), WorktreeId(3));
        assert_eq!(overlay.store().state(WorktreeId(3)).unwrap(), Some(WorktreeState::Initializing));

        // 同名幂等
        let again = reg.auto_register("wt-3").unwrap();
        assert!(Arc::ptr_eq(&overlay, &again));

        // daemon 钩子切换状态机
        reg.mark_ready("wt-3").unwrap();
        assert_eq!(
            overlay.store().state(WorktreeId(3)).unwrap(),
            Some(WorktreeState::Ready)
        );
        assert!(!overlay.is_initializing().unwrap());

        reg.mark_failed("wt-3").unwrap();
        assert_eq!(
            overlay.store().state(WorktreeId(3)).unwrap(),
            Some(WorktreeState::Failed)
        );

        // 未注册名报 NotFound
        assert!(reg.mark_ready("wt-99").is_err());
        assert!(reg.get("wt-99").is_none());
        assert_eq!(reg.names(), vec!["wt-3".to_string()]);
    }

    #[test]
    fn auto_register_rejects_bad_names_and_accepts_arbitrary_once() {
        let dir = tempfile::tempdir().unwrap();
        let reg = registry(dir.path());
        assert!(reg.auto_register("").is_err());
        assert!(reg.auto_register("a/b").is_err());
        assert!(reg.auto_register("con").is_err());
        // 非 wt-N 命名也可用（派生 id）
        let custom = reg.auto_register("custom-tree").unwrap();
        assert_eq!(custom.worktree_id(), WorktreeId(fnv1a_i63("custom-tree")));
        let unregistered = reg.unregister("custom-tree");
        assert!(unregistered);
        assert!(reg.get("custom-tree").is_none());
    }

    #[test]
    fn no_template_falls_back_to_full_copy_mode() {
        let dir = tempfile::tempdir().unwrap();
        let reg = registry(dir.path());
        let overlay = reg.auto_register("wt-1").unwrap();
        overlay.mark_ready().unwrap();
        // repo 不存在 → resolve 失败 → 无模板
        assert!(overlay.lower().is_none());
        let (state, _) = overlay.write("f.txt", b"content").unwrap();
        assert_eq!(state, DedupeState::New);
    }
}
