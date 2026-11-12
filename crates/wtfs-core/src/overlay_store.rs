//! overlay 元数据的 SQLite 实现（spec §8.2/§8.3：[`OverlayStore`] 契约）。
//!
//! 覆盖 `worktrees` / `dedupe` / `whiteouts` / `renames` / `file_ids` / `stat_cache`：
//!
//! * `register` 落 `state='initializing'` 行；整份 [`OverlayConfig`] 以 JSON 持久化
//!   在 `worktrees.config_json`，`config()` 原样往返（volume_serial、dedupe 开关等）。
//! * 子表全部 `ON DELETE CASCADE`，`unregister` 只需删 `worktrees` 行。
//! * `file_id` 用 blake3(小写路径) 派生 62 位稳定值，落 `file_ids` 表持久化；
//!   派生冲突（同 id 不同路径）线性探测 +1，仍保持跨进程稳定。
//! * `dedupe.state` 存三态字符串 lower/upper/new（[`crate::DedupeState`]）。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use rusqlite::{params, OptionalExtension};

use crate::db::{db_err, now_ms};
use crate::{
    DedupeDecision, DedupeState, Error, OverlayConfig, OverlayStore, Result, WorktreeId,
    WorktreeState, validate_relative_path,
};

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

/// `stat_cache` 行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatCacheEntry {
    pub mtime_ns: i64,
    pub size: u64,
    /// 位 0：目录条目；其余保留。
    pub flags: u32,
}

impl StatCacheEntry {
    pub const FLAG_DIRECTORY: u32 = 1;
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

const MAX_INO: i64 = 0x3FFF_FFFF_FFFF_FFFF;

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

    // ---- 引擎用的批量/单点查询（trait 之外的 additive 补充） ----

    /// 全部 whiteout（lower 命名空间路径）。
    pub fn whiteouts(&self, id: WorktreeId) -> Result<HashSet<String>> {
        let conn = self.db.lock();
        let mut stmt = conn
            .prepare("SELECT path FROM whiteouts WHERE worktree_id = ?1")
            .map_err(db_err)?;
        let rows = stmt.query_map(params![id.0], |r| r.get::<_, String>(0)).map_err(db_err)?;
        let mut out = HashSet::new();
        for row in rows {
            out.insert(row.map_err(db_err)?);
        }
        Ok(out)
    }

    /// 全部目录重命名映射 (from → to)。
    pub fn renames(&self, id: WorktreeId) -> Result<Vec<(String, String)>> {
        let conn = self.db.lock();
        let mut stmt = conn
            .prepare("SELECT from_path, to_path FROM renames WHERE worktree_id = ?1")
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![id.0], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(db_err)?);
        }
        Ok(out)
    }

    /// 全部 dedupe 记录（路径 → 三态）。
    pub fn dedupe_states(&self, id: WorktreeId) -> Result<HashMap<String, DedupeState>> {
        let conn = self.db.lock();
        let mut stmt = conn
            .prepare("SELECT path, state FROM dedupe WHERE worktree_id = ?1")
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![id.0], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(db_err)?;
        let mut out = HashMap::new();
        for row in rows {
            let (path, state) = row.map_err(db_err)?;
            if let Some(s) = DedupeState::from_str_opt(&state) {
                out.insert(path, s);
            }
        }
        Ok(out)
    }

    pub fn stat_entry(&self, id: WorktreeId, path: &str) -> Result<Option<StatCacheEntry>> {
        let conn = self.db.lock();
        conn.query_row(
            "SELECT mtime_ns, size, flags FROM stat_cache WHERE worktree_id = ?1 AND path = ?2",
            params![id.0, path],
            |r| {
                Ok(StatCacheEntry {
                    mtime_ns: r.get(0)?,
                    size: r.get::<_, i64>(1)? as u64,
                    flags: r.get::<_, i64>(2)? as u32,
                })
            },
        )
        .optional()
        .map_err(db_err)
    }

    /// 全部 stat_cache 条目。
    pub fn stat_entries(&self, id: WorktreeId) -> Result<HashMap<String, StatCacheEntry>> {
        let conn = self.db.lock();
        let mut stmt = conn
            .prepare("SELECT path, mtime_ns, size, flags FROM stat_cache WHERE worktree_id = ?1")
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![id.0], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    StatCacheEntry {
                        mtime_ns: r.get(1)?,
                        size: r.get::<_, i64>(2)? as u64,
                        flags: r.get::<_, i64>(3)? as u32,
                    },
                ))
            })
            .map_err(db_err)?;
        let mut out = HashMap::new();
        for row in rows {
            let (path, entry) = row.map_err(db_err)?;
            out.insert(path, entry);
        }
        Ok(out)
    }

    /// upsert 一条 stat_cache。
    pub fn set_stat_entry(&self, id: WorktreeId, path: &str, entry: StatCacheEntry) -> Result<()> {
        self.db.with_tx(|tx| {
            tx.execute(
                "INSERT INTO stat_cache(worktree_id, path, mtime_ns, size, flags)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(worktree_id, path) DO UPDATE SET
                    mtime_ns = excluded.mtime_ns, size = excluded.size, flags = excluded.flags",
                params![id.0, path, entry.mtime_ns, entry.size as i64, entry.flags as i64],
            )
            .map_err(db_err)?;
            Ok(())
        })
    }

    pub fn remove_stat_entry(&self, id: WorktreeId, path: &str) -> Result<()> {
        self.db
            .with_tx(|tx| {
                tx.execute(
                    "DELETE FROM stat_cache WHERE worktree_id = ?1 AND path = ?2",
                    params![id.0, path],
                )
                .map_err(db_err)?;
                Ok(())
            })
    }

    /// 该 worktree 已发放的最大逻辑 mtime（时钟跨重启单调的基础）。
    pub fn max_stat_mtime(&self, id: WorktreeId) -> Result<Option<i64>> {
        let conn = self.db.lock();
        conn.query_row(
            "SELECT MAX(mtime_ns) FROM stat_cache WHERE worktree_id = ?1",
            params![id.0],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()
        .map_err(db_err)
        .map(|opt| opt.flatten())
    }

    /// 三态 dedupe 记录（trait `record_dedupe` 只能表达 lower/upper）。
    pub fn record_dedupe_state(&self, id: WorktreeId, path: &str, state: DedupeState) -> Result<()> {
        validate_relative_path(path)?;
        self.db.with_tx(|tx| {
            tx.execute(
                "INSERT INTO dedupe(worktree_id, path, state) VALUES (?1, ?2, ?3)
                 ON CONFLICT(worktree_id, path) DO UPDATE SET state = excluded.state",
                params![id.0, path, state.as_str()],
            )
            .map_err(db_err)?;
            Ok(())
        })
    }

    pub fn remove_dedupe(&self, id: WorktreeId, path: &str) -> Result<()> {
        self.db
            .with_tx(|tx| {
                tx.execute(
                    "DELETE FROM dedupe WHERE worktree_id = ?1 AND path = ?2",
                    params![id.0, path],
                )
                .map_err(db_err)?;
                Ok(())
            })
    }

    /// 删除 `dir` 及其子树的 dedupe / stat_cache 行（目录删除时清理）。
    pub fn remove_meta_subtree(&self, id: WorktreeId, dir: &str) -> Result<()> {
        let prefix = format!("{dir}/");
        self.db.with_tx(|tx| {
            for table in ["dedupe", "stat_cache"] {
                tx.execute(
                    &format!(
                        "DELETE FROM {table} WHERE worktree_id = ?1 AND (path = ?2 OR path LIKE ?3)"
                    ),
                    params![id.0, dir, format!("{prefix}%")],
                )
                .map_err(db_err)?;
            }
            Ok(())
        })
    }

    /// upper 内容随 rename 物理移动：把 from（含子树）的 dedupe/stat_cache 键改写为 to。
    pub fn rewrite_meta_prefix(&self, id: WorktreeId, from: &str, to: &str) -> Result<()> {
        let from_prefix = format!("{from}/");
        self.db.with_tx(|tx| {
            for table in ["dedupe", "stat_cache"] {
                let rows: Vec<(String, Vec<String>)> = {
                    let sql = match table {
                        "dedupe" => "SELECT path, state FROM dedupe WHERE worktree_id = ?1",
                        _ => "SELECT path, mtime_ns, size, flags FROM stat_cache WHERE worktree_id = ?1",
                    };
                    let mut stmt = tx.prepare(sql).map_err(db_err)?;
                    let mapped = stmt
                        .query_map(params![id.0], |r| {
                            Ok((r.get::<_, String>(0)?, {
                                if table == "dedupe" {
                                    vec![r.get::<_, String>(1)?]
                                } else {
                                    vec![
                                        r.get::<_, i64>(1)?.to_string(),
                                        r.get::<_, i64>(2)?.to_string(),
                                        r.get::<_, i64>(3)?.to_string(),
                                    ]
                                }
                            }))
                        })
                        .map_err(db_err)?;
                    let mut out = Vec::new();
                    for row in mapped {
                        out.push(row.map_err(db_err)?);
                    }
                    out
                };
                for (path, values) in rows {
                    if path != from && !path.starts_with(&from_prefix) {
                        continue;
                    }
                    let new_path = if path == from {
                        to.to_string()
                    } else {
                        format!("{to}/{}", &path[from_prefix.len()..])
                    };
                    tx.execute(
                        &format!("DELETE FROM {table} WHERE worktree_id = ?1 AND path = ?2"),
                        params![id.0, path],
                    )
                    .map_err(db_err)?;
                    match table {
                        "dedupe" => {
                            tx.execute(
                                "INSERT INTO dedupe(worktree_id, path, state) VALUES (?1, ?2, ?3)
                                 ON CONFLICT(worktree_id, path) DO UPDATE SET state = excluded.state",
                                params![id.0, new_path, values[0]],
                            )
                            .map_err(db_err)?;
                        }
                        _ => {
                            tx.execute(
                                "INSERT INTO stat_cache(worktree_id, path, mtime_ns, size, flags)
                                 VALUES (?1, ?2, ?3, ?4, ?5)
                                 ON CONFLICT(worktree_id, path) DO UPDATE SET
                                    mtime_ns = excluded.mtime_ns, size = excluded.size, flags = excluded.flags",
                                params![
                                    id.0,
                                    new_path,
                                    values[0].parse::<i64>().unwrap_or(0),
                                    values[1].parse::<i64>().unwrap_or(0),
                                    values[2].parse::<i64>().unwrap_or(0),
                                ],
                            )
                            .map_err(db_err)?;
                        }
                    }
                }
            }
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

    fn record_dedupe(&self, id: WorktreeId, path: &str, decision: DedupeDecision) -> Result<()> {
        let state = match decision {
            DedupeDecision::ToLower => DedupeState::Lower,
            DedupeDecision::ToUpper => DedupeState::Upper,
        };
        self.record_dedupe_state(id, path, state)
    }

    fn add_whiteout(&self, id: WorktreeId, path: &str) -> Result<()> {
        validate_relative_path(path)?;
        self.db
            .with_tx(|tx| {
                tx.execute(
                    "INSERT OR IGNORE INTO whiteouts(worktree_id, path) VALUES (?1, ?2)",
                    params![id.0, path],
                )
                .map_err(db_err)?;
                Ok(())
            })
    }

    fn remove_whiteout(&self, id: WorktreeId, path: &str) -> Result<()> {
        validate_relative_path(path)?;
        self.db
            .with_tx(|tx| {
                tx.execute(
                    "DELETE FROM whiteouts WHERE worktree_id = ?1 AND path = ?2",
                    params![id.0, path],
                )
                .map_err(db_err)?;
                Ok(())
            })
    }

    fn record_rename(&self, id: WorktreeId, from: &str, to: &str) -> Result<()> {
        validate_relative_path(from)?;
        validate_relative_path(to)?;
        if from == to {
            return Err(Error::Overlay(format!(
                "rename source and target are identical: {from}"
            )));
        }
        self.db
            .with_tx(|tx| {
                tx.execute(
                    "INSERT INTO renames(worktree_id, from_path, to_path) VALUES (?1, ?2, ?3)
                     ON CONFLICT(worktree_id, from_path) DO UPDATE SET to_path = excluded.to_path",
                    params![id.0, from, to],
                )
                .map_err(db_err)?;
                Ok(())
            })
    }

    fn file_id(&self, id: WorktreeId, path: &str) -> Result<u64> {
        validate_relative_path(path)?;
        let conn = self.db.lock();
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM file_ids WHERE worktree_id = ?1 AND path = ?2",
                params![id.0, path],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?;
        if let Some(v) = existing {
            return Ok(v as u64);
        }
        let mut candidate = derive_ino(path);
        for _ in 0..64 {
            let occupant: Option<String> = conn
                .query_row(
                    "SELECT path FROM file_ids WHERE worktree_id = ?1 AND id = ?2",
                    params![id.0, candidate],
                    |r| r.get(0),
                )
                .optional()
                .map_err(db_err)?;
            match occupant {
                None => break,
                Some(p) if p == path => break,
                Some(_) => {
                    candidate = if candidate >= MAX_INO { 2 } else { candidate + 1 };
                }
            }
        }
        conn.execute(
            "INSERT OR IGNORE INTO file_ids(worktree_id, path, id) VALUES (?1, ?2, ?3)",
            params![id.0, path, candidate],
        )
        .map_err(db_err)?;
        let v: i64 = conn
            .query_row(
                "SELECT id FROM file_ids WHERE worktree_id = ?1 AND path = ?2",
                params![id.0, path],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        Ok(v as u64)
    }

    fn unregister(&self, id: WorktreeId) -> Result<()> {
        // 子表 ON DELETE CASCADE：删主行即清空全部派生元数据；幂等。
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

    #[test]
    fn file_id_is_stable_deterministic_and_cleared_on_unregister() {
        let store = SqliteOverlayStore::new(Db::open_in_memory().unwrap());
        seed_template(&store, 1);
        store.register(&config(1, "C:/tmp/u1")).unwrap();
        let a1 = store.file_id(WorktreeId(1), "a/b.txt").unwrap();
        let a2 = store.file_id(WorktreeId(1), "a/b.txt").unwrap();
        assert_eq!(a1, a2);
        let b = store.file_id(WorktreeId(1), "a/c.txt").unwrap();
        assert_ne!(a1, b);
        assert!(matches!(
            store.file_id(WorktreeId(1), "../bad").unwrap_err(),
            Error::InvalidPath { .. }
        ));
        store.unregister(WorktreeId(1)).unwrap();
        assert!(store.state(WorktreeId(1)).unwrap().is_none());
        // 路径派生确定性：新 worktree 同路径得到同一 ino。
        store.register(&config(2, "C:/tmp/u2")).unwrap();
        assert_eq!(store.file_id(WorktreeId(2), "a/b.txt").unwrap(), a1);
        // unregister 幂等。
        store.unregister(WorktreeId(1)).unwrap();
    }

    #[test]
    fn whiteout_rename_dedupe_stat_and_subtree_ops() {
        let store = SqliteOverlayStore::new(Db::open_in_memory().unwrap());
        seed_template(&store, 1);
        store.register(&config(1, "C:/tmp/u1")).unwrap();
        let id = WorktreeId(1);

        store.add_whiteout(id, "gone.txt").unwrap();
        assert!(store.whiteouts(id).unwrap().contains("gone.txt"));
        store.remove_whiteout(id, "gone.txt").unwrap();
        assert!(store.whiteouts(id).unwrap().is_empty());

        store.record_rename(id, "src", "dst").unwrap();
        assert_eq!(store.renames(id).unwrap(), vec![("src".to_string(), "dst".to_string())]);
        assert!(matches!(
            store.record_rename(id, "x", "x").unwrap_err(),
            Error::Overlay(_)
        ));

        store.record_dedupe(id, "f.txt", DedupeDecision::ToLower).unwrap();
        assert_eq!(store.dedupe_states(id).unwrap()["f.txt"], DedupeState::Lower);
        store
            .record_dedupe_state(id, "f.txt", DedupeState::New)
            .unwrap();
        assert_eq!(store.dedupe_states(id).unwrap()["f.txt"], DedupeState::New);

        store
            .set_stat_entry(id, "f.txt", StatCacheEntry { mtime_ns: 111, size: 3, flags: 0 })
            .unwrap();
        store
            .set_stat_entry(id, "src/inner.txt", StatCacheEntry { mtime_ns: 222, size: 1, flags: 0 })
            .unwrap();
        assert_eq!(store.max_stat_mtime(id).unwrap(), Some(222));
        assert_eq!(store.stat_entry(id, "f.txt").unwrap().unwrap().mtime_ns, 111);

        store.rewrite_meta_prefix(id, "src", "dst").unwrap();
        let entries = store.stat_entries(id).unwrap();
        assert!(entries.contains_key("dst/inner.txt"));
        assert!(!entries.contains_key("src/inner.txt"));
        assert_eq!(entries["dst/inner.txt"].mtime_ns, 222);

        store.remove_meta_subtree(id, "dst").unwrap();
        assert!(!store.stat_entries(id).unwrap().contains_key("dst/inner.txt"));
        store.remove_stat_entry(id, "f.txt").unwrap();
        assert!(store.stat_entry(id, "f.txt").unwrap().is_none());
        assert_eq!(store.max_stat_mtime(id).unwrap(), None);
    }

    #[test]
    fn unregister_cascades_children() {
        let store = SqliteOverlayStore::new(Db::open_in_memory().unwrap());
        seed_template(&store, 1);
        store.register(&config(1, "C:/tmp/u1")).unwrap();
        let id = WorktreeId(1);
        store.add_whiteout(id, "x.txt").unwrap();
        store.file_id(id, "x.txt").unwrap();
        store.set_stat_entry(id, "x.txt", StatCacheEntry { mtime_ns: 5, size: 1, flags: 0 }).unwrap();
        store.record_dedupe_state(id, "x.txt", DedupeState::Upper).unwrap();
        store.record_rename(id, "a", "b").unwrap();
        store.unregister(id).unwrap();
        assert!(store.whiteouts(id).unwrap().is_empty());
        assert!(store.dedupe_states(id).unwrap().is_empty());
        assert!(store.stat_entries(id).unwrap().is_empty());
        assert!(store.renames(id).unwrap().is_empty());
    }

    #[test]
    fn derive_ino_is_62bit_positive_and_stable() {
        let a = derive_ino("src/lib.rs");
        assert!(a > 0 && a <= MAX_INO);
        assert_eq!(a, derive_ino("SRC/LIB.RS"), "case-insensitive path stability");
        for i in 0..1000 {
            let v = derive_ino(&format!("p{i}"));
            assert!(v > 0);
        }
    }
}
