//! SQLite 元数据库：连接管理、WAL 配置与 schema 迁移（spec §8.1–§8.3）。
//!
//! 一个仓库一份 `meta.db`，模板与 overlay 共用；WAL + busy_timeout 允许多连接，
//! 写操作一律经 [`Db::with_tx`] 事务化。迁移用 `PRAGMA user_version` 驱动：
//! v1 = spec §8.2 全部九张表；v2 = 追加 `idx_stat_cache_wt_mtime` 索引；
//! v3 = 删除派生的 `dedupe` / `file_ids` / `stat_cache` 三张表（M11：这三类
//! 状态完全可由 upper 物理文件 + 模板清单 + 删除标记推导，不再落库）；
//! v4 = 删除 `whiteouts` / `renames` 两张表（M12：删除墓碑/目录 opaque/
//! rename 映射改为 worktree 目录下的镜像标记树，见 [`crate::overlay_markers`]），
//! 已有行迁移成标记文件；后续版本在 [`migrate`] 中逐级追加升级步骤。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, MutexGuard};
use rusqlite::{params, Connection};

use crate::overlay_markers::{marker_root_for_upper, MarkerStore};
use crate::{Error, Result};

/// 当前 schema 版本。
pub const SCHEMA_VERSION: i64 = 4;

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

/// v1 → v2（历史迁移步骤）：`stat_cache` 的 `(worktree_id, mtime_ns)` 兜底
/// 索引。v1/v2 时代的 stat_cache 已在 v3 删除（当时曾由内存逻辑 mtime 时钟
/// 写入并靠本索引服务一次性 MAX 查询；该机制随 M11 派生表删除一并移除，
/// 现为 upper 真实文件 mtime）。本步骤仅为历史库升级路径保留。
const SCHEMA_V2: &str =
    "CREATE INDEX IF NOT EXISTS idx_stat_cache_wt_mtime ON stat_cache(worktree_id, mtime_ns);";

/// v2 → v3：删除派生的每文件元数据表（M11/B110100）。
///
/// `dedupe`（写入决策）、`file_ids`（持久 ino 映射）、`stat_cache`（逻辑
/// mtime 缓存）三者的状态完全可由 upper 物理文件 + 模板清单 + 删除标记
/// 推导：upper 文件存在即 upper，缺失即 lower；ino = 路径稳定哈希；
/// upper mtime = 真实文件 mtime。OverlayFS/Docker overlay2 同样只在
/// upper 层自身保存状态。
const SCHEMA_V3: &str = r#"
DROP INDEX IF EXISTS idx_stat_cache_wt_mtime;
DROP TABLE IF EXISTS stat_cache;
DROP TABLE IF EXISTS file_ids;
DROP TABLE IF EXISTS dedupe;
"#;

/// v3 → v4：删除 `whiteouts` / `renames` 两张表（M12/B120100）。
/// 行数据先于本 DDL 转成标记树（[`migrate_v3_to_v4`]）。
const SCHEMA_V4: &str = r#"
DROP TABLE IF EXISTS whiteouts;
DROP TABLE IF EXISTS renames;
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
        .query_row("PRAGMA user_version", params![], |r| r.get(0))
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
    if version < 4 {
        migrate_v3_to_v4(conn)?;
    }
    if version != SCHEMA_VERSION {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(db_err)?;
    }
    Ok(())
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![name],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

/// v3 → v4（M12/B120100）：`whiteouts` / `renames` 行转成 worktree 目录下
/// 的镜像标记树，然后 DROP 两张表。
///
/// 转换规则：
/// * `whiteouts(worktree_id, path)` → `<upper 父目录>/meta/whiteouts/<path>`
///   0 字节标记文件；
/// * `renames(worktree_id, from_path, to_path)` → `<...>/meta/redirect/<to_path>`
///   标记文件（内容 = from_path）。旧 `from_path` 侧的"已搬走"不可见语义由
///   redirect 映射本身给出（解析时 [`crate::overlay::map_to_lower_with`] 的
///   `MovedAway`），无需额外标记。
///
/// 幂等/可重入：标记写入幂等（重写同一内容），`DROP TABLE IF EXISTS` 幂等，
/// `user_version` 最后更新——任一步之后重跑都收敛到同一状态，不产生半套
/// 状态。标记写入失败直接报错（不静默丢墓碑；丢失的可见后果只是被删 lower
/// 内容重现，spec §8.3 允许，但应在打开时显式暴露）。
fn migrate_v3_to_v4(conn: &Connection) -> Result<()> {
    if !table_exists(conn, "whiteouts") && !table_exists(conn, "renames") {
        return Ok(());
    }
    let mut uppers: HashMap<i64, String> = HashMap::new();
    {
        let mut stmt = conn
            .prepare("SELECT id, upper_path FROM worktrees")
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(db_err)?;
        for row in rows {
            let (id, upper) = row.map_err(db_err)?;
            uppers.insert(id, upper);
        }
    }
    let marker_store = |id: i64| -> Option<MarkerStore> {
        uppers
            .get(&id)
            .map(|upper| MarkerStore::new(marker_root_for_upper(Path::new(upper))))
    };

    if table_exists(conn, "whiteouts") {
        let mut stmt = conn
            .prepare("SELECT worktree_id, path FROM whiteouts")
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(db_err)?;
        let pending: Vec<(i64, String)> = rows
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)?;
        drop(stmt);
        for (id, path) in pending {
            if let Some(markers) = marker_store(id) {
                markers.create_whiteout(&path)?;
            }
        }
    }

    if table_exists(conn, "renames") {
        let mut stmt = conn
            .prepare("SELECT worktree_id, from_path, to_path FROM renames")
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(db_err)?;
        let pending: Vec<(i64, String, String)> = rows
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)?;
        drop(stmt);
        for (id, from, to) in pending {
            if let Some(markers) = marker_store(id) {
                markers.create_redirect(&to, &from)?;
            }
        }
    }

    conn.execute_batch(SCHEMA_V4).map_err(db_err)?;
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
                    params![],
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
            .query_row("PRAGMA user_version", params![], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM repos", params![], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        // v4 保留的表：控制表（whiteouts/renames 已被 v4 删除）。
        for table in ["repos", "templates", "template_files", "worktrees"] {
            conn.query_row(
                &format!("SELECT COUNT(*) FROM {table}"),
                params![],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or_else(|e| panic!("table {table} missing: {e}"));
        }
        // v3/v4 删除的表：不存在。
        for table in ["dedupe", "file_ids", "stat_cache", "whiteouts", "renames"] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(exists, 0, "table {table} must be dropped by v4");
        }
    }

    #[test]
    fn with_tx_rolls_back_on_error() {
        let db = Db::open_in_memory().unwrap();
        let err = db
            .with_tx::<()>(|tx| {
                tx.execute(
                    "INSERT INTO repos(root, common_git_dir, created_at) VALUES ('x', '', 0)",
                    params![],
                )
                .map_err(db_err)?;
                Err(Error::Overlay("boom".into()))
            })
            .unwrap_err();
        assert!(err.to_string().contains("boom"));
        let conn = db.lock();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM repos", params![], |r| r.get(0))
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

    /// 真实 v1 库（含 whiteouts/renames 数据、user_version=1）→ 打开时升到
    /// v4：控制行保留，whiteouts/renames 行转成标记文件，五张已删表不存在。
    #[test]
    fn migrates_v1_db_to_v4_converting_rows_to_markers() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("meta.db");
        let upper = dir.path().join("worktrees").join("wt-7").join("upper");
        std::fs::create_dir_all(&upper).unwrap();
        {
            // 手工构造 v1 库：跑 v1 schema、写数据、标注 user_version=1。
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
            conn.execute_batch(&format!(
                "INSERT INTO repos(id, root, common_git_dir, created_at) VALUES (1, 'r', '', 0);
                 INSERT INTO worktrees(id, repo_id, upper_path, status, config_json, created_at)
                     VALUES (7, 1, '{}', 'ready', '{{}}', 0);
                 INSERT INTO dedupe(worktree_id, path, state) VALUES (7, 'a.txt', 'lower');
                 INSERT INTO whiteouts(worktree_id, path) VALUES (7, 'gone.txt'), (7, 'sub/deep/x.txt');
                 INSERT INTO renames(worktree_id, from_path, to_path) VALUES (7, 'src', 'dst');
                 INSERT INTO file_ids(worktree_id, path, id) VALUES (7, 'a.txt', 11);
                 INSERT INTO stat_cache(worktree_id, path, mtime_ns, size, flags)
                     VALUES (7, 'a.txt', 111, 3, 0);",
                upper.to_string_lossy().replace('\\', "/")
            ))
            .unwrap();
        }

        let db = Db::open(&path).unwrap();
        {
            let conn = db.lock();
            let version: i64 = conn
                .query_row("PRAGMA user_version", params![], |r| r.get(0))
                .unwrap();
            assert_eq!(version, 4);
            for table in ["dedupe", "file_ids", "stat_cache", "whiteouts", "renames"] {
                let exists: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                        params![table],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(exists, 0, "table {table} must be dropped");
            }
            for index in ["idx_stat_cache_wt_mtime", "idx_file_ids_wt_id"] {
                let exists: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                        params![index],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(exists, 0, "index {index} must be dropped");
            }
            // 控制表无损。
            let wts: i64 = conn
                .query_row("SELECT COUNT(*) FROM worktrees", params![], |r| r.get(0))
                .unwrap();
            assert_eq!(wts, 1);
        }
        // 标记树：whiteout 行 → 0 字节标记文件；rename 行 → redirect 标记。
        let meta = crate::overlay_markers::marker_root_for_upper(&upper);
        assert!(meta.join("whiteouts").join("gone.txt").is_file());
        assert!(meta.join("whiteouts").join("sub").join("deep").join("x.txt").is_file());
        let redirect = meta.join("redirect").join("dst");
        assert!(redirect.is_file());
        assert_eq!(std::fs::read_to_string(redirect).unwrap(), "src");
        // 重开幂等：无第二次转换副作用，版本稳定。
        drop(db);
        let db = Db::open(&path).unwrap();
        let version: i64 = db
            .lock()
            .query_row("PRAGMA user_version", params![], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 4);
        let state = crate::overlay_markers::MarkerStore::new(meta).load().unwrap();
        assert!(state.whites.contains("gone.txt"));
        assert_eq!(state.redirects, vec![("src".to_string(), "dst".to_string())]);
    }

    /// 真实 v2 库（带 stat_cache 索引、user_version=2）→ 直接升 v4。
    #[test]
    fn migrates_v2_db_to_v4_dropping_stat_cache_and_tombstone_tables() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("meta.db");
        let upper = dir.path().join("worktrees").join("wt-7").join("upper");
        std::fs::create_dir_all(&upper).unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(SCHEMA_V2).unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
            conn.execute_batch(&format!(
                "INSERT INTO repos(id, root, common_git_dir, created_at) VALUES (1, 'r', '', 0);
                 INSERT INTO worktrees(id, repo_id, upper_path, status, config_json, created_at)
                     VALUES (7, 1, '{}', 'ready', '{{}}', 0);
                 INSERT INTO whiteouts(worktree_id, path) VALUES (7, 'gone.txt');",
                upper.to_string_lossy().replace('\\', "/")
            ))
            .unwrap();
            let indexes: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index'
                     AND name = 'idx_stat_cache_wt_mtime'",
                    params![],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(indexes, 1, "v2 库应带 stat_cache 索引");
        }

        let db = Db::open(&path).unwrap();
        let conn = db.lock();
        let version: i64 = conn
            .query_row("PRAGMA user_version", params![], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 4);
        for table in ["stat_cache", "whiteouts", "renames"] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(exists, 0, "table {table} must be dropped");
        }
        drop(conn);
        let state = crate::overlay_markers::MarkerStore::new(
            crate::overlay_markers::marker_root_for_upper(&upper),
        )
        .load()
        .unwrap();
        assert!(state.whites.contains("gone.txt"), "whiteout 行无损转成标记");
    }

    /// v3 → v4 中途失败可重入：构造 v3 库、手工只转一半（先插入一条
    /// whiteout 行并写标记），模拟"标记已写、表未 DROP"的中途态；重跑迁移
    /// 收敛（标记重写幂等、表 DROP、版本到 4）。
    #[test]
    fn v4_migration_is_reentrant_after_partial_failure() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("meta.db");
        let upper = dir.path().join("worktrees").join("wt-7").join("upper");
        std::fs::create_dir_all(&upper).unwrap();
        let upper_slash = upper.to_string_lossy().replace('\\', "/");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(SCHEMA_V3).unwrap();
            conn.pragma_update(None, "user_version", 3).unwrap();
            conn.execute_batch(&format!(
                "INSERT INTO repos(id, root, common_git_dir, created_at) VALUES (1, 'r', '', 0);
                 INSERT INTO worktrees(id, repo_id, upper_path, status, config_json, created_at)
                     VALUES (7, 1, '{upper_slash}', 'ready', '{{}}', 0);
                 INSERT INTO whiteouts(worktree_id, path) VALUES (7, 'gone.txt'), (7, 'late.txt');
                 INSERT INTO renames(worktree_id, from_path, to_path) VALUES (7, 'src', 'dst');"
            ))
            .unwrap();
            // 模拟中途失败：只有 gone.txt 的标记写成功。
            let meta = crate::overlay_markers::marker_root_for_upper(&upper);
            crate::overlay_markers::MarkerStore::new(meta)
                .create_whiteout("gone.txt")
                .unwrap();
        }

        let db = Db::open(&path).unwrap();
        let version: i64 = db
            .lock()
            .query_row("PRAGMA user_version", params![], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 4);
        let state = crate::overlay_markers::MarkerStore::new(
            crate::overlay_markers::marker_root_for_upper(&upper),
        )
        .load()
        .unwrap();
        assert!(state.whites.contains("gone.txt"));
        assert!(state.whites.contains("late.txt"), "重跑补齐未写完的标记");
        assert_eq!(state.redirects, vec![("src".to_string(), "dst".to_string())]);
    }

    /// 全新库直接建成 v4（无 tombstone 两表），与迁移收敛到同一物理 schema。
    #[test]
    fn fresh_db_creates_v4_schema() {
        let db = Db::open_in_memory().unwrap();
        let conn = db.lock();
        let version: i64 = conn
            .query_row("PRAGMA user_version", params![], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        for table in ["repos", "templates", "template_files", "worktrees"] {
            conn.query_row(
                &format!("SELECT COUNT(*) FROM {table}"),
                params![],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or_else(|e| panic!("table {table} missing: {e}"));
        }
        for table in ["dedupe", "file_ids", "stat_cache", "whiteouts", "renames"] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(exists, 0, "fresh v4 must not create {table}");
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
                params![],
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

    /// 任务验收：真实 v3 库（含 whiteout/rename 数据）迁移后挂载 Overlay，
    /// 删除 / 重建 / rename 行为端到端正确。
    #[test]
    fn migrated_v3_rows_drive_overlay_semantics() {
        use crate::OverlayStore;
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("meta.db");
        let lower_dir = dir.path().join("lower");
        let upper_dir = dir.path().join("worktrees").join("wt-7").join("upper");
        std::fs::create_dir_all(&lower_dir).unwrap();
        std::fs::create_dir_all(&upper_dir).unwrap();
        std::fs::write(lower_dir.join("gone.txt"), b"template").unwrap();
        std::fs::create_dir_all(lower_dir.join("src")).unwrap();
        std::fs::write(lower_dir.join("src").join("f.txt"), b"payload").unwrap();
        let mut lower_files = std::collections::HashMap::new();
        for (path, content) in [("gone.txt", &b"template"[..]), ("src/f.txt", b"payload")] {
            lower_files.insert(
                path.to_string(),
                crate::TemplateFile {
                    path: path.to_string(),
                    hash: crate::dedupe::hash_bytes(content),
                    size: content.len() as u64,
                    mode: 0o100644,
                },
            );
        }
        {
            // v3 库：whiteout(gone.txt) + 目录改名(src → dst)。
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(SCHEMA_V3).unwrap();
            conn.pragma_update(None, "user_version", 3).unwrap();
            conn.execute_batch(&format!(
                "INSERT INTO repos(id, root, common_git_dir, created_at) VALUES (1, 'r', '', 0);
                 INSERT INTO worktrees(id, repo_id, upper_path, status, config_json, created_at)
                     VALUES (7, 1, '{}', 'ready', '{{}}', 0);
                 INSERT INTO whiteouts(worktree_id, path) VALUES (7, 'gone.txt');
                 INSERT INTO renames(worktree_id, from_path, to_path) VALUES (7, 'src', 'dst');
                 INSERT INTO templates(id, repo_id, base_commit, tree_path, index_path, state, created_at)
                     VALUES (1, 1, 'c0ffee', 't', 'i', 'ready', 0);
                 UPDATE worktrees SET template_id = 1 WHERE id = 7;",
                upper_dir.to_string_lossy().replace('\\', "/")
            ))
            .unwrap();
            // v3 时代 rename 的物理随迁：upper/src → upper/dst。
            std::fs::create_dir_all(upper_dir.join("src")).unwrap();
            std::fs::write(upper_dir.join("src").join("new.txt"), b"added").unwrap();
            std::fs::rename(upper_dir.join("src"), upper_dir.join("dst")).unwrap();
        }

        // 打开即迁移（v3 → v4）；挂载 Overlay。
        let db = Db::open(&db_path).unwrap();
        let store = std::sync::Arc::new(crate::SqliteOverlayStore::new(db));
        let mut cfg = crate::OverlayConfig::new(crate::WorktreeId(7), &upper_dir);
        cfg.lower_root = Some(lower_dir.clone());
        cfg.template_id = Some(crate::TemplateId(1));
        let of = crate::Overlay::reopen(cfg, Some(lower_files), store.clone()).unwrap();
        assert!(!of.is_initializing().unwrap());

        // 迁移语义：whiteout → gone.txt 不可见；redirect → dst 子树可读。
        assert_eq!(of.resolve("gone.txt").unwrap(), crate::Resolution::Whiteout);
        assert!(matches!(
            of.resolve("dst").unwrap(),
            crate::Resolution::Directory { has_lower: true, has_upper: true }
        ));
        let names: Vec<String> = of
            .list("dst")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["f.txt", "new.txt"]);
        assert_eq!(of.read("dst/f.txt").unwrap().as_deref(), Some(b"payload".as_slice()));
        assert_eq!(of.resolve("src").unwrap(), crate::Resolution::NotFound);

        // 删除：lower 文件（dst/f.txt）→ whiteout 标记（映射路径 src/f.txt）。
        assert!(of.remove("dst/f.txt").unwrap());
        assert_eq!(of.resolve("dst/f.txt").unwrap(), crate::Resolution::Whiteout);
        let markers = crate::MarkerStore::new(crate::marker_root_for_upper(&upper_dir));
        assert!(markers.load().unwrap().whites.contains("src/f.txt"));

        // 重建：写回模板一致内容 → 标记撤销、回落 lower、可见。
        let (state, _) = of.write("dst/f.txt", b"payload").unwrap();
        assert_eq!(state, crate::DedupeState::Lower);
        assert!(matches!(
            of.resolve("dst/f.txt").unwrap(),
            crate::Resolution::Lower { .. }
        ));
        assert!(!markers.load().unwrap().whites.contains("src/f.txt"));

        // rename：文件级在迁移子树内正常（whiteout + upper 搬移）。
        of.rename("dst/new.txt", "dst/moved.txt").unwrap();
        assert_eq!(of.resolve("dst/new.txt").unwrap(), crate::Resolution::NotFound);
        assert_eq!(of.read("dst/moved.txt").unwrap().as_deref(), Some(b"added".as_slice()));
        // 目录级（lower 有内容）→ EXDEV。
        let err = of.rename("dst", "elsewhere").unwrap_err();
        assert!(matches!(err, crate::Error::CrossDevice(_)), "got {err:?}");

        // 注销：标记树整树删除。
        store.unregister(crate::WorktreeId(7)).unwrap();
        assert!(!crate::marker_root_for_upper(&upper_dir).exists());
    }
}
