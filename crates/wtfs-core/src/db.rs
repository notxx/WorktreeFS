//! SQLite 元数据库：连接管理、WAL 配置与 schema 迁移（spec §8.1–§8.3）。
//!
//! 一个仓库一份 `meta.db`，模板与 overlay 共用；WAL + busy_timeout 允许多连接，
//! 写操作一律经 [`Db::with_tx`] 事务化。迁移用 `PRAGMA user_version` 驱动：
//! v1 = spec §8.2 全部九张表；v2 = 追加 `idx_stat_cache_wt_mtime` 索引；
//! 后续版本在 [`migrate`] 中逐级追加升级步骤。

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, MutexGuard};
use rusqlite::Connection;

use crate::{Error, Result};

/// 当前 schema 版本。
pub const SCHEMA_VERSION: i64 = 2;

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
    stats: Arc<TxStats>,
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

/// v1 → v2：`stat_cache` 的 `(worktree_id, mtime_ns)` 兜底索引。
///
/// 常规写路径已由内存逻辑 mtime 时钟取代逐写 `MAX(mtime_ns)` 查询；
/// 索引服务剩下的一次性查询——Overlay 打开时初始化时钟的
/// [`crate::overlay_store::SqliteOverlayStore::max_stat_mtime`]（无索引时
/// 在 stat_cache 增长后退化为全表扫描）。
const SCHEMA_V2: &str =
    "CREATE INDEX IF NOT EXISTS idx_stat_cache_wt_mtime ON stat_cache(worktree_id, mtime_ns);";

/// [`Db`] 的事务计数（测试专用断言用；生产路径仅几次原子加）。
#[derive(Debug, Default)]
struct TxStats {
    /// `with_tx` 开启的事务数。
    begun: AtomicU64,
    /// `with_tx` 成功提交的事务数。
    committed: AtomicU64,
    /// `with_tx` 回滚/提交失败的事务数。
    rolled_back: AtomicU64,
    /// `with_tx` 之外的 autocommit 写（每条语句自带一个写事务）。
    autocommit_writes: AtomicU64,
}

/// [`Db::tx_stats`] 的只读快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxStatsSnapshot {
    pub begun: u64,
    pub committed: u64,
    pub rolled_back: u64,
    pub autocommit_writes: u64,
}

impl TxStatsSnapshot {
    /// 显式事务（含回滚）+ autocommit 写的总数。
    pub fn total(&self) -> u64 {
        self.committed + self.rolled_back + self.autocommit_writes
    }
}

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
            stats: Arc::new(TxStats::default()),
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
        self.stats.begun.fetch_add(1, Ordering::Relaxed);
        let value = match f(&tx) {
            Ok(value) => value,
            // drop(tx) 的隐式回滚与原实现一致。
            Err(e) => {
                drop(tx);
                self.stats.rolled_back.fetch_add(1, Ordering::Relaxed);
                return Err(e);
            }
        };
        match tx.commit() {
            Ok(()) => {
                self.stats.committed.fetch_add(1, Ordering::Relaxed);
                Ok(value)
            }
            Err(e) => {
                self.stats.rolled_back.fetch_add(1, Ordering::Relaxed);
                Err(db_err(e))
            }
        }
    }

    /// 计一次 `with_tx` 之外的 autocommit 写（每条 = 一个隐式写事务）。
    /// 仅供确需绕过事务的单语句写路径调用（如 [`crate::overlay_store`] 的
    /// `file_id` 插入），保证事务计数器覆盖全部写事务。
    pub(crate) fn count_autocommit_write(&self) {
        self.stats.autocommit_writes.fetch_add(1, Ordering::Relaxed);
    }

    /// 测试专用：当前事务计数快照（断言写路径的事务数为 O(1)）。
    #[cfg(test)]
    pub(crate) fn tx_stats(&self) -> TxStatsSnapshot {
        TxStatsSnapshot {
            begun: self.stats.begun.load(Ordering::Relaxed),
            committed: self.stats.committed.load(Ordering::Relaxed),
            rolled_back: self.stats.rolled_back.load(Ordering::Relaxed),
            autocommit_writes: self.stats.autocommit_writes.load(Ordering::Relaxed),
        }
    }
}

/// `PRAGMA user_version` 驱动的迁移；高于本 crate 支持的版本直接拒绝
/// （旧程序写新库会破坏数据）。逐级升级：`version < N` 步骤只追加不重写，
/// v1 库与全新库最终收敛到同一物理 schema。
fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", rusqlite::params![], |r| r.get(0))
        .map_err(db_err)?;
    if version > SCHEMA_VERSION {
        return Err(Error::Overlay(format!(
            "meta db schema version {version} is newer than supported {SCHEMA_VERSION}"
        )));
    }
    if version < 1 {
        conn.execute_batch(SCHEMA_V1).map_err(db_err)?;
    }
    if version < 2 {
        conn.execute_batch(SCHEMA_V2).map_err(db_err)?;
    }
    if version != SCHEMA_VERSION {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(db_err)?;
    }
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

    /// 真实 v1 库（无 v2 索引、user_version=1、有数据）→ 打开时无损升 v2。
    #[test]
    fn migrates_v1_db_to_v2_with_stat_cache_mtime_index() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("meta.db");
        {
            // 手工构造 v1 库：跑 v1 schema、写数据、标注 user_version=1。
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
            conn.execute_batch(
                "INSERT INTO repos(id, root, common_git_dir, created_at) VALUES (1, 'r', '', 0);
                 INSERT INTO worktrees(id, repo_id, upper_path, status, config_json, created_at)
                     VALUES (7, 1, 'u', 'ready', '{}', 0);
                 INSERT INTO stat_cache(worktree_id, path, mtime_ns, size, flags)
                     VALUES (7, 'a.txt', 111, 3, 0), (7, 'b.txt', 222, 1, 0);",
            )
            .unwrap();
            let indexes: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index'
                     AND name = 'idx_stat_cache_wt_mtime'",
                    rusqlite::params![],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(indexes, 0, "v1 库不应带 v2 索引");
        }

        let db = Db::open(&path).unwrap();
        {
            let conn = db.lock();
            let version: i64 = conn
                .query_row("PRAGMA user_version", rusqlite::params![], |r| r.get(0))
                .unwrap();
            assert_eq!(version, 2);
            let has_index: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index'
                     AND name = 'idx_stat_cache_wt_mtime'",
                    rusqlite::params![],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(has_index, 1);
            // 数据无损
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM stat_cache", rusqlite::params![], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 2);
        }
        // 索引确实可用：MAX 查询走 (worktree_id, mtime_ns) 前缀。
        let max = db
            .lock()
            .query_row(
                "SELECT MAX(mtime_ns) FROM stat_cache WHERE worktree_id = 7",
                rusqlite::params![],
                |r| r.get::<_, Option<i64>>(0),
            )
            .unwrap();
        assert_eq!(max, Some(222));
    }

    /// 全新库直接建成 v2（含索引），与迁移收敛到同一物理 schema。
    #[test]
    fn fresh_db_creates_v2_schema_with_index() {
        let db = Db::open_in_memory().unwrap();
        let conn = db.lock();
        let version: i64 = conn
            .query_row("PRAGMA user_version", rusqlite::params![], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let has_index: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index'
                 AND name = 'idx_stat_cache_wt_mtime'",
                rusqlite::params![],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_index, 1);
    }

    /// 事务计数器：with_tx 提交/回滚与 autocommit 写分别计数。
    #[test]
    fn tx_stats_counts_commits_rollbacks_and_autocommit_writes() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.tx_stats().total(), 0);
        db.with_tx(|tx| {
            tx.execute(
                "INSERT INTO repos(root, common_git_dir, created_at) VALUES ('r', '', 0)",
                rusqlite::params![],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .unwrap();
        let err = db
            .with_tx::<()>(|_tx| Err(Error::Overlay("boom".into())))
            .unwrap_err();
        assert!(err.to_string().contains("boom"));
        assert_eq!(
            db.tx_stats(),
            TxStatsSnapshot {
                begun: 2,
                committed: 1,
                rolled_back: 1,
                autocommit_writes: 0,
            }
        );
        assert_eq!(db.tx_stats().total(), 2);
    }
}
