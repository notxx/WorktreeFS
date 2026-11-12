//! SQLite 元数据库：连接管理、WAL 配置与 schema 迁移（spec §8.1–§8.3）。
//!
//! 一个仓库一份 `meta.db`，模板与 overlay 共用；WAL + busy_timeout 允许多连接，
//! 写操作一律经 [`Db::with_tx`] 事务化。迁移用 `PRAGMA user_version` 驱动：
//! v1 = spec §8.2 全部九张表；后续版本在 [`migrate`] 中追加升级步骤。

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, MutexGuard};
use rusqlite::Connection;

use crate::{Error, Result};

/// 当前 schema 版本。
pub const SCHEMA_VERSION: i64 = 1;

/// rusqlite 错误 → 统一 [`Error::Overlay`]。
pub(crate) fn db_err(e: rusqlite::Error) -> Error {
    Error::Overlay(format!("sqlite: {e}"))
}

/// 当前 Unix 毫秒时间戳（created_at 列）。
pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// 共享 SQLite 连接句柄；`Clone` 廉价（内部 `Arc<Mutex<Connection>>`），`Send + Sync`。
#[derive(Debug, Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

/// v1 schema（spec §8.2）。子表全部 `ON DELETE CASCADE`：
/// 注销 worktree / 重建失败模板时一行 DELETE 即可清空派生元数据。
const SCHEMA_V1: &str = r#"
CREATE TABLE repos (
    id INTEGER PRIMARY KEY,
    root TEXT NOT NULL UNIQUE,
    common_git_dir TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL
);
CREATE TABLE templates (
    id INTEGER PRIMARY KEY,
    repo_id INTEGER NOT NULL REFERENCES repos(id),
    base_commit TEXT NOT NULL,
    tree_path TEXT NOT NULL,
    index_path TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE(repo_id, base_commit)
);
CREATE TABLE template_files (
    template_id INTEGER NOT NULL REFERENCES templates(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    hash TEXT NOT NULL,
    size INTEGER NOT NULL,
    mode INTEGER NOT NULL,
    PRIMARY KEY(template_id, path)
);
CREATE TABLE worktrees (
    id INTEGER PRIMARY KEY,
    repo_id INTEGER REFERENCES repos(id),
    path TEXT,
    branch TEXT,
    base_commit TEXT,
    template_id INTEGER REFERENCES templates(id),
    upper_path TEXT NOT NULL,
    status TEXT NOT NULL,
    config_json TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX idx_worktrees_path ON worktrees(path);
CREATE TABLE dedupe (
    worktree_id INTEGER NOT NULL REFERENCES worktrees(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    state TEXT NOT NULL,
    PRIMARY KEY(worktree_id, path)
);
CREATE TABLE whiteouts (
    worktree_id INTEGER NOT NULL REFERENCES worktrees(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    PRIMARY KEY(worktree_id, path)
);
CREATE TABLE renames (
    worktree_id INTEGER NOT NULL REFERENCES worktrees(id) ON DELETE CASCADE,
    from_path TEXT NOT NULL,
    to_path TEXT NOT NULL,
    PRIMARY KEY(worktree_id, from_path)
);
CREATE TABLE file_ids (
    worktree_id INTEGER NOT NULL REFERENCES worktrees(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    id INTEGER NOT NULL,
    PRIMARY KEY(worktree_id, path)
);
CREATE UNIQUE INDEX idx_file_ids_wt_id ON file_ids(worktree_id, id);
CREATE TABLE stat_cache (
    worktree_id INTEGER NOT NULL REFERENCES worktrees(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    mtime_ns INTEGER NOT NULL,
    size INTEGER NOT NULL,
    flags INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(worktree_id, path)
);
"#;

impl Db {
    /// 打开（必要时创建）文件元数据库；父目录自动创建。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path).map_err(db_err)?;
        Self::init(conn)
    }

    /// 打开私有内存库（单元测试用）。
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(db_err)?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self> {
        // journal_mode 在内存库上返回 "memory"（无 WAL），无碍正确性。
        conn.execute_batch(
            "PRAGMA busy_timeout = 5000;
             PRAGMA foreign_keys = ON;
             PRAGMA synchronous = NORMAL;
             PRAGMA journal_mode = WAL;",
        )
        .map_err(db_err)?;
        migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// 独占连接（读路径/DDL 直用；写路径请走 [`Db::with_tx`]）。
    pub(crate) fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock()
    }

    /// 事务化写：闭包出错时自动回滚。
    pub fn with_tx<T>(
        &self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(db_err)?;
        let value = f(&tx)?;
        tx.commit().map_err(db_err)?;
        Ok(value)
    }
}

/// `PRAGMA user_version` 驱动的迁移；高于本 crate 支持的版本直接拒绝
/// （旧程序写新库会破坏数据）。
fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", rusqlite::params![], |r| r.get(0))
        .map_err(db_err)?;
    if version > SCHEMA_VERSION {
        return Err(Error::Overlay(format!(
            "meta db schema version {version} is newer than supported {SCHEMA_VERSION}"
        )));
    }
    if version == 0 {
        conn.execute_batch(SCHEMA_V1).map_err(db_err)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(db_err)?;
    }
    // 后续版本：if version < 2 { ... } 等，逐级升级。
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn creates_v1_schema_and_reopens_idempotently() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("meta.db");
        {
            let db = Db::open(&path).unwrap();
            db.with_tx(|tx| {
                tx.execute(
                    "INSERT INTO repos(root, common_git_dir, created_at) VALUES ('r', '', 0)",
                    rusqlite::params![],
                )
                .map_err(db_err)?;
                Ok(())
            })
            .unwrap();
        }
        // 重开：迁移幂等，数据仍在。
        let db = Db::open(&path).unwrap();
        let conn = db.lock();
        let version: i64 = conn
            .query_row("PRAGMA user_version", rusqlite::params![], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM repos", rusqlite::params![], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        for table in [
            "repos",
            "templates",
            "template_files",
            "worktrees",
            "dedupe",
            "whiteouts",
            "renames",
            "file_ids",
            "stat_cache",
        ] {
            conn.query_row(
                &format!("SELECT COUNT(*) FROM {table}"),
                rusqlite::params![],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or_else(|e| panic!("table {table} missing: {e}"));
        }
    }

    #[test]
    fn with_tx_rolls_back_on_error() {
        let db = Db::open_in_memory().unwrap();
        let err = db
            .with_tx::<()>(|tx| {
                tx.execute(
                    "INSERT INTO repos(root, common_git_dir, created_at) VALUES ('x', '', 0)",
                    rusqlite::params![],
                )
                .map_err(db_err)?;
                Err(Error::Overlay("boom".into()))
            })
            .unwrap_err();
        assert!(err.to_string().contains("boom"));
        let conn = db.lock();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM repos", rusqlite::params![], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "rolled back");
    }

    #[test]
    fn refuses_newer_schema() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("meta.db");
        {
            let db = Db::open(&path).unwrap();
            db.lock()
                .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
        }
        let err = Db::open(&path).unwrap_err();
        assert!(err.to_string().contains("newer than supported"));
    }
}
