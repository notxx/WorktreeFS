//! overlay 元数据的 SQLite 实现（spec §8.2/§8.3：[`OverlayStore`] 契约）。
//!
//! M12/B120100 起 overlay 元数据只保留 `worktrees`（注册/状态机/config JSON）。
//! `whiteouts` / `renames` 两张表随 schema v4 删除：删除墓碑改为 upper 层内的
//! OverlayFS 同款 `.wh.<name>` 标记文件，rename 映射改为 `meta/redirect/`
//! 标记（[`crate::overlay_markers`]）——unlink/rmdir 只写一个 O(1) 标记文件，
//! 不再开 SQLite 写事务；标记随 upper 目录由 gc 整体回收。M11 已删除的
//! `dedupe` / `file_ids` / `stat_cache` 派生表保持删除：upper 物理文件 +
//! 模板清单本身即权威状态——upper 存在即 Upper、缺失即 Lower；ino =
//! 路径稳定哈希（[`derive_ino`]）；upper mtime = 真实文件 mtime。
//!
//! * `register` 落 `state='initializing'` 行；整份 [`OverlayConfig`] 以 JSON
//!   持久化在 `worktrees.config_json`，`config()` 原样往返（volume_serial、
//!   dedupe 开关等）。
//! * `unregister`：删 `worktrees` 行（幂等）；标记随 upper 目录由 gc 回收。
//! * `file_id` 不再持久化：blake3(小写路径) 直接派生 62 位稳定值，跨进程
//!   天然一致（见 [`crate::overlay::Overlay::file_id`]）。

use std::path::PathBuf;

use rusqlite::{params, OptionalExtension};

use crate::db::{db_err, now_ms};
use crate::{Error, OverlayConfig, OverlayStore, Result, WorktreeId, WorktreeState};

/// overlay 元数据存储；与模板存储共享同一 [`Db`]（一个仓库一份 meta.db）。
pub struct SqliteOverlayStore {
    db: crate::Db,
}

/// daemon/M4 注册 worktree 时的补充信息；trait `register` 只有 [`OverlayConfig`]，
/// 这些列（repo/path/branch/base_commit）允许为空。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeRegistration {
    pub repo_root: Option<PathBuf>,
    /// worktree 在 repo 内的路径（如 `<repo>/.tower/worktrees/wt-1`）。
    pub path: Option<PathBuf>,
    pub branch: Option<String>,
    pub base_commit: Option<String>,
}

fn parse_status(s: &str) -> WorktreeState {
    match s {
        "ready" => WorktreeState::Ready,
        "failed" => WorktreeState::Failed,
        "removed" => WorktreeState::Removed,
        _ => WorktreeState::Initializing,
    }
}

/// blake3(小写路径) 前 8 字节 → 62 位正整数（SQLite INTEGER 安全；避开 0）。
/// file_id 的唯一来源：同一路径（大小写不敏感）恒得同一 ino，跨进程稳定。
pub(crate) fn derive_ino(path: &str) -> i64 {
    let hash = blake3::hash(path.to_ascii_lowercase().as_bytes());
    let mut v: u64 = 0;
    for b in &hash.as_bytes()[..8] {
        v = (v << 8) | u64::from(*b);
    }
    let v = v & 0x3FFF_FFFF_FFFF_FFFF;
    if v == 0 {
        1
    } else {
        v as i64
    }
}

impl SqliteOverlayStore {
    pub fn new(db: crate::Db) -> Self {
        Self { db }
    }

    pub fn db(&self) -> &crate::Db {
        &self.db
    }

    /// trait `register` 的扩展版：附带 repo/path/branch/base_commit 列。
    pub fn register_ext(&self, config: &OverlayConfig, meta: &WorktreeRegistration) -> Result<()> {
        let config_json = serde_json::to_string(config)
            .map_err(|e| Error::Overlay(format!("serialize overlay config: {e}")))?;
        self.db.with_tx(|tx| {
            let exists: Option<i64> = tx
                .query_row(
                    "SELECT id FROM worktrees WHERE id = ?1",
                    params![config.worktree_id.0],
                    |r| r.get(0),
                )
                .optional()
                .map_err(db_err)?;
            if exists.is_some() {
                return Err(Error::Overlay(format!(
                    "worktree {} already registered",
                    config.worktree_id
                )));
            }
            let repo_id = match &meta.repo_root {
                Some(root) => Some(crate::template_store::upsert_repo(tx, root, false)?),
                None => None,
            };
            tx.execute(
                "INSERT INTO worktrees(id, repo_id, path, branch, base_commit, template_id,
                                        upper_path, status, config_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'initializing', ?8, ?9)",
                params![
                    config.worktree_id.0,
                    repo_id,
                    meta.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
                    meta.branch,
                    meta.base_commit,
                    config.template_id.map(|t| t.0),
                    config.upper_root.to_string_lossy(),
                    config_json,
                    now_ms(),
                ],
            )
            .map_err(db_err)?;
            Ok(())
        })
    }

    /// 已注册 worktree 的状态清单（daemon 崩溃恢复用，additive）。
    pub fn registered(&self) -> Result<Vec<(WorktreeId, WorktreeState)>> {
        let conn = self.db.lock();
        let mut stmt = conn
            .prepare("SELECT id, status FROM worktrees ORDER BY id")
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![], |r| {
                Ok((WorktreeId(r.get::<_, i64>(0)?), parse_status(&r.get::<_, String>(1)?)))
            })
            .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(db_err)?);
        }
        Ok(out)
    }
}

impl OverlayStore for SqliteOverlayStore {
    fn register(&self, config: &OverlayConfig) -> Result<()> {
        self.register_ext(config, &WorktreeRegistration::default())
    }

    fn config(&self, id: WorktreeId) -> Result<Option<OverlayConfig>> {
        let conn = self.db.lock();
        let json: Option<String> = conn
            .query_row(
                "SELECT config_json FROM worktrees WHERE id = ?1",
                params![id.0],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?;
        drop(conn);
        json.map(|j| {
            serde_json::from_str(&j)
                .map_err(|e| Error::Overlay(format!("deserialize overlay config for {id}: {e}")))
        })
        .transpose()
    }

    fn state(&self, id: WorktreeId) -> Result<Option<WorktreeState>> {
        let conn = self.db.lock();
        let status: Option<String> = conn
            .query_row(
                "SELECT status FROM worktrees WHERE id = ?1",
                params![id.0],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?;
        Ok(status.map(|s| parse_status(&s)))
    }

    fn set_state(&self, id: WorktreeId, state: WorktreeState) -> Result<()> {
        let status = state.to_string();
        let n = self
            .db
            .with_tx(|tx| {
                tx.execute(
                    "UPDATE worktrees SET status = ?1 WHERE id = ?2",
                    params![status, id.0],
                )
                .map_err(db_err)
            })?;
        if n == 0 {
            return Err(Error::NotFound(id.to_string()));
        }
        Ok(())
    }

    fn unregister(&self, id: WorktreeId) -> Result<()> {
        // M12：v4 起无子表可级联（whiteouts/renames 已删）。删除标记为 upper
        // 内的 `.wh.*` 文件，随 upper 目录由 `wtfs gc` 整树回收（spec §6.6）——
        // 注销路径保持零逐项工作；同 id 重新注册时 [`MarkerStore::clear`]
        // 会清扫残留标记。
        self.db
            .with_tx(|tx| {
                tx.execute("DELETE FROM worktrees WHERE id = ?1", params![id.0])
                    .map_err(db_err)?;
                Ok(())
            })
    }
}

/// 对象安全冒烟：`Arc<dyn OverlayStore>` 是 M3（VFS）的使用形态。
#[allow(dead_code)]
fn assert_object_safe(store: std::sync::Arc<SqliteOverlayStore>) -> std::sync::Arc<dyn OverlayStore> {
    store as std::sync::Arc<dyn OverlayStore>
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Db, TemplateId, WorktreeId};
    use tempfile::TempDir;

    fn config(id: i64, upper: &str) -> OverlayConfig {
        let mut cfg = OverlayConfig::new(WorktreeId(id), upper);
        cfg.template_id = Some(TemplateId(1));
        cfg.volume_serial = 0x1234_5678;
        cfg.dedupe_enabled = false;
        cfg
    }

    /// `worktrees.template_id` 带 FK：先种一行模板，让带 template_id 的注册合法。
    fn seed_template(store: &SqliteOverlayStore, id: i64) {
        store
            .db()
            .with_tx(|tx| {
                tx.execute(
                    "INSERT INTO repos(id, root, common_git_dir, created_at) VALUES (1, 'r', '', 0)",
                    rusqlite::params![],
                )
                .map_err(db_err)?;
                tx.execute(
                    "INSERT INTO templates(id, repo_id, base_commit, tree_path, index_path, state, created_at)
                     VALUES (?1, 1, 'c0ffee', 't', 'i', 'ready', 0)",
                    rusqlite::params![id],
                )
                .map_err(db_err)?;
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn register_config_roundtrip_and_duplicate_rejected() {
        let store = SqliteOverlayStore::new(Db::open_in_memory().unwrap());
        seed_template(&store, 1);
        let cfg = config(7, "C:/tmp/upper7");
        store.register(&cfg).unwrap();
        assert!(matches!(
            store.register(&cfg).unwrap_err(),
            Error::Overlay(_)
        ));
        let got = store.config(WorktreeId(7)).unwrap().unwrap();
        assert_eq!(got.upper_root.to_string_lossy(), "C:/tmp/upper7");
        assert_eq!(got.volume_serial, 0x1234_5678);
        assert!(!got.dedupe_enabled);
        assert_eq!(got.template_id, Some(TemplateId(1)));
        assert!(store.config(WorktreeId(8)).unwrap().is_none());
        assert_eq!(
            store.state(WorktreeId(7)).unwrap(),
            Some(WorktreeState::Initializing)
        );
        store.set_state(WorktreeId(7), WorktreeState::Ready).unwrap();
        assert_eq!(store.state(WorktreeId(7)).unwrap(), Some(WorktreeState::Ready));
        assert!(matches!(
            store.set_state(WorktreeId(8), WorktreeState::Ready).unwrap_err(),
            Error::NotFound(_)
        ));
        assert_eq!(
            store.registered().unwrap(),
            vec![(WorktreeId(7), WorktreeState::Ready)]
        );
    }

    /// unregister 幂等删行；标记日志（meta/markers.log）与 meta/redirect 随
    /// meta/ 目录由 gc 回收——同 id 重新注册时 `MarkerStore::clear` 清扫残留。
    #[test]
    fn unregister_is_idempotent_and_markers_follow_upper() {
        let dir = TempDir::new().unwrap();
        let upper = dir.path().join("worktrees").join("wt-1").join("upper");
        std::fs::create_dir_all(&upper).unwrap();
        let markers = crate::overlay_markers::MarkerStore::new(upper.clone());
        markers.create_whiteout("gone.txt").unwrap();
        markers.create_redirect("z", "a").unwrap();

        let store = SqliteOverlayStore::new(Db::open_in_memory().unwrap());
        seed_template(&store, 1);
        store.register(&config(1, upper.to_string_lossy().as_ref())).unwrap();
        assert_eq!(store.state(WorktreeId(1)).unwrap(), Some(WorktreeState::Initializing));
        store.unregister(WorktreeId(1)).unwrap();
        assert!(store.state(WorktreeId(1)).unwrap().is_none());
        // 幂等。
        store.unregister(WorktreeId(1)).unwrap();
        // 标记日志随 meta/ 保留（gc 回收）；重新注册前清扫。
        assert!(
            markers.load().unwrap().whites.contains("gone.txt"),
            "标记日志随 meta/ 保留给 gc"
        );
        markers.clear().unwrap();
        assert!(markers.load().unwrap().whites.is_empty());
    }

    #[test]
    fn derive_ino_is_62bit_positive_and_stable() {
        const MAX_INO: i64 = 0x3FFF_FFFF_FFFF_FFFF;
        let a = derive_ino("src/lib.rs");
        assert!(a > 0 && a <= MAX_INO);
        assert_eq!(a, derive_ino("SRC/LIB.RS"), "case-insensitive path stability");
        for i in 0..1000 {
            let v = derive_ino(&format!("p{i}"));
            assert!(v > 0);
        }
    }
}
