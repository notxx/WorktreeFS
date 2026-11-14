//! SQLite 元数据库：连接管理、WAL 配置与 schema 迁移（spec §8.1–§8.3）。
//!
//! 一个仓库一份 `meta.db`，模板与 overlay 共用；WAL + busy_timeout 允许多连接，
//! 写操作一律经 [`Db::with_tx`] 事务化。迁移用 `PRAGMA user_version` 驱动：
//! v1 = spec §8.2 全部九张表；v2 = 追加 `idx_stat_cache_wt_mtime` 索引；
//! v3 = 删除派生的 `dedupe` / `file_ids` / `stat_cache` 三张表（M11：这三类
//! 状态完全可由 upper 物理文件 + 模板清单 + whiteouts 推导，不再落库）；
//! 后续版本在 [`migrate`] 中逐级追加升级步骤。

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, MutexGuard};
use rusqlite::Connection;

use crate::{Error, Result};

/// 当前 schema 版本。
pub const SCHEMA_VERSION: i64 = 3;

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

/// v2 → v3：删除派生的每文件元数据表（M11/B110100）。
///
/// `dedupe`（写入决策）、`file_ids`（持久 ino 映射）、`stat_cache`（逻辑
/// mtime 缓存）三者的状态完全可由 upper 物理文件 + 模板清单 + whiteouts
/// 推导：upper 文件存在即 upper，缺失即 lower；ino = 路径稳定哈希；
/// upper mtime = 真实文件 mtime。OverlayFS/Docker overlay2 同样只在
/// upper 层自身保存状态。whiteouts/renames 保留原语义（M12 替换为标记树）。
const SCHEMA_V3: &str = r#"
DROP INDEX IF EXISTS idx_stat_cache_wt_mtime;
DROP TABLE IF EXISTS stat_cache;
DROP TABLE IF EXISTS file_ids;
DROP TABLE IF EXISTS dedupe;
"#;

/// [`Db`] 的事务计数（测试专用断言用；生产路径仅几次原子加）。
#[derive(Debug, Default)]
struct TxStats {
    /// `with_tx` 开启的事务数。
    begun: AtomicU64,
    /// `with_tx` 成功提交的事务数。
    committed: AtomicU64,
    /// `with_tx` 回滚/提交失败的事务数。
    rolled_back: AtomicU64,
}

/// [`Db::tx_stats`] 的只读快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxStatsSnapshot {
    pub begun: u64,
    pub committed: u64,
    pub rolled_back: u64,
}

impl TxStatsSnapshot {
    /// 显式事务（含回滚）总数。
    pub fn total(&self) -> u64 {
        self.committed + self.rolled_back
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

    /// 测试专用：当前事务计数快照（断言写路径的事务数为 O(1)）。
    #[cfg(test)]
    pub(crate) fn tx_stats(&self) -> TxStatsSnapshot {
        TxStatsSnapshot {
            begun: self.stats.begun.load(Ordering::Relaxed),
            committed: self.stats.committed.load(Ordering::Relaxed),
            rolled_back: self.stats.rolled_back.load(Ordering::Relaxed),
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
    if version < 3 {
        conn.execute_batch(SCHEMA_V3).map_err(db_err)?;
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
        // v3 保留的表：控制表 + whiteouts/renames。
        for table in [
            "repos",
            "templates",
            "template_files",
            "worktrees",
            "whiteouts",
            "renames",
        ] {
            conn.query_row(
                &format!("SELECT COUNT(*) FROM {table}"),
                rusqlite::params![],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or_else(|e| panic!("table {table} missing: {e}"));
        }
        // v3 删除的派生表：不存在。
        for table in ["dedupe", "file_ids", "stat_cache"] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    rusqlite::params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(exists, 0, "table {table} must be dropped in v3");
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

    /// 真实 v1 库（有全部子表数据、user_version=1）→ 打开时无损升 v3：
    /// 控制行与 whiteouts/renames 保留，派生三表删除。
    #[test]
    fn migrates_v1_db_to_v3_losslessly() {
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
                 INSERT INTO dedupe(worktree_id, path, state) VALUES (7, 'a.txt', 'lower');
                 INSERT INTO whiteouts(worktree_id, path) VALUES (7, 'gone.txt');
                 INSERT INTO renames(worktree_id, from_path, to_path) VALUES (7, 'src', 'dst');
                 INSERT INTO file_ids(worktree_id, path, id) VALUES (7, 'a.txt', 11);
                 INSERT INTO stat_cache(worktree_id, path, mtime_ns, size, flags)
                     VALUES (7, 'a.txt', 111, 3, 0);",
            )
            .unwrap();
        }

        let db = Db::open(&path).unwrap();
        {
            let conn = db.lock();
            let version: i64 = conn
                .query_row("PRAGMA user_version", rusqlite::params![], |r| r.get(0))
                .unwrap();
            assert_eq!(version, 3);
            for table in ["dedupe", "file_ids", "stat_cache"] {
                let exists: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                        rusqlite::params![table],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(exists, 0, "table {table} must be dropped");
            }
            for index in ["idx_stat_cache_wt_mtime", "idx_file_ids_wt_id"] {
                let exists: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                        rusqlite::params![index],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(exists, 0, "index {index} must be dropped");
            }
            // whiteouts/renames 无损。
            let whites: i64 = conn
                .query_row("SELECT COUNT(*) FROM whiteouts", rusqlite::params![], |r| r.get(0))
                .unwrap();
            assert_eq!(whites, 1);
            let renames: i64 = conn
                .query_row("SELECT COUNT(*) FROM renames", rusqlite::params![], |r| r.get(0))
                .unwrap();
            assert_eq!(renames, 1);
            // 控制表无损。
            let wts: i64 = conn
                .query_row("SELECT COUNT(*) FROM worktrees", rusqlite::params![], |r| r.get(0))
                .unwrap();
            assert_eq!(wts, 1);
        }
        // 迁移后的库可直接继续承载 overlay 元数据（whiteouts/renames 读写正常）。
        db.with_tx(|tx| {
            tx.execute(
                "INSERT INTO whiteouts(worktree_id, path) VALUES (7, 'another.txt')",
                rusqlite::params![],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .unwrap();
        let whites: i64 = db
            .lock()
            .query_row("SELECT COUNT(*) FROM whiteouts", rusqlite::params![], |r| r.get(0))
            .unwrap();
        assert_eq!(whites, 2);
    }

    /// 真实 v2 库（带 stat_cache 索引、user_version=2）→ 直接升 v3。
    #[test]
    fn migrates_v2_db_to_v3_drops_stat_cache_index() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("meta.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(SCHEMA_V2).unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
            conn.execute_batch(
                "INSERT INTO repos(id, root, common_git_dir, created_at) VALUES (1, 'r', '', 0);
                 INSERT INTO worktrees(id, repo_id, upper_path, status, config_json, created_at)
                     VALUES (7, 1, 'u', 'ready', '{}', 0);
                 INSERT INTO whiteouts(worktree_id, path) VALUES (7, 'gone.txt');",
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
            assert_eq!(indexes, 1, "v2 库应带 stat_cache 索引");
        }

        let db = Db::open(&path).unwrap();
        let conn = db.lock();
        let version: i64 = conn
            .query_row("PRAGMA user_version", rusqlite::params![], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 3);
        let stat_cache: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'stat_cache'",
                rusqlite::params![],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stat_cache, 0);
        let whites: i64 = conn
            .query_row("SELECT COUNT(*) FROM whiteouts", rusqlite::params![], |r| r.get(0))
            .unwrap();
        assert_eq!(whites, 1, "whiteouts 无损");
    }

    /// 全新库直接建成 v3（无派生三表），与迁移收敛到同一物理 schema。
    #[test]
    fn fresh_db_creates_v3_schema() {
        let db = Db::open_in_memory().unwrap();
        let conn = db.lock();
        let version: i64 = conn
            .query_row("PRAGMA user_version", rusqlite::params![], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        for table in ["whiteouts", "renames"] {
            conn.query_row(
                &format!("SELECT COUNT(*) FROM {table}"),
                rusqlite::params![],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or_else(|e| panic!("table {table} missing: {e}"));
        }
        for table in ["dedupe", "file_ids", "stat_cache"] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    rusqlite::params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(exists, 0, "fresh v3 must not create {table}");
        }
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
            }
        );
        assert_eq!(db.tx_stats().total(), 2);
    }
}
