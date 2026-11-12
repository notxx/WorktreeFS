//! `wtfs gc`（spec §6.6）：dead 目录、孤儿 upper、无引用旧模板。
//!
//! 安全规则（M4 任务：绝不碰 `dashboard-fork-*` 等非 Tower 目录）：
//!
//! * 目录候选只认 Tower 命名 `wt-<正整数>`；其他名字（含 `custom-tree`）
//!   一律不进计划。
//! * dead 目录 = 无 `.git` 或未注册（meta.db 无对应 `worktrees` 行）。
//! * 孤儿 upper = `<store>/worktrees/wt-<n>/` 无对应注册行。
//! * 旧模板 = 无任何 worktree 引用、非 `building`（可能在途）、且年龄超过
//!   `max_age` 的 `templates` 行（含其 `template_files` 与树目录）。
//!
//! 元数据访问：直接开独立 SQLite 连接读 spec §8.2 的稳定 schema（wtfs-core
//! 的 `Db::lock` 是 crate 私有，daemon/CLI 各自的连接经 WAL 与 daemon 共存）。
//!
//! 挂载语义：daemon 在线 GC（`registry = Some`）经注销移除 FS 视图目录，不
//! 物理删除 junction 下的路径；CLI 离线 GC（`registry = None, mounted = false`）
//! 直接 `remove_dir_all` 真实目录。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use serde::Serialize;

/// GC 上下文。`registry`/`mounted` 语义见模块文档。
pub struct GcContext<'a> {
    pub mount_root: &'a Path,
    pub store_root: &'a Path,
    /// daemon 在线模式提供（注销路由）；CLI 离线模式 `None`。
    pub registry: Option<&'a wtfs_vfs::VfsRegistry>,
    /// mount_root 当前处于 FS 挂载中（目录是 FS 视图，注销即消失）。
    pub mounted: bool,
    /// 旧模板年龄阈值。
    pub max_age: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeadDir {
    pub name: String,
    pub worktree_id: i64,
    /// `no .git` / `unregistered`。
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrphanUpper {
    pub dir: String,
    pub worktree_id: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OldTemplate {
    pub id: i64,
    pub base_commit: String,
    /// `ready` / `failed`（`building` 不回收）。
    pub state: String,
    pub age_days: f64,
    /// 模板树目录（apply 时连同 index 快照一起删除）。
    pub tree_dir: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct GcPlan {
    pub dead_dirs: Vec<DeadDir>,
    pub orphan_uppers: Vec<OrphanUpper>,
    pub old_templates: Vec<OldTemplate>,
}

impl GcPlan {
    pub fn is_empty(&self) -> bool {
        self.dead_dirs.is_empty()
            && self.orphan_uppers.is_empty()
            && self.old_templates.is_empty()
    }

    /// 人读摘要（CLI 输出用）。
    pub fn summary(&self) -> String {
        format!(
            "dead dirs: {}, orphan uppers: {}, old templates: {}",
            self.dead_dirs.len(),
            self.orphan_uppers.len(),
            self.old_templates.len()
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct GcReport {
    pub removed_dead_dirs: usize,
    pub removed_uppers: usize,
    pub removed_templates: usize,
    pub errors: Vec<String>,
}

impl GcReport {
    pub fn summary(&self) -> String {
        let mut s = format!(
            "removed: {} dead dirs, {} orphan uppers, {} templates",
            self.removed_dead_dirs, self.removed_uppers, self.removed_templates
        );
        if !self.errors.is_empty() {
            s.push_str(&format!("; {} errors", self.errors.len()));
        }
        s
    }
}

/// 计划（只读，不做任何删除）。
pub fn plan(ctx: &GcContext<'_>) -> anyhow::Result<GcPlan> {
    let conn = open_meta_db(ctx.store_root)?;
    let registered = match &conn {
        Some(conn) => registered_ids(conn)?,
        None => HashSet::new(),
    };
    let referenced = match &conn {
        Some(conn) => referenced_template_ids(conn)?,
        None => HashSet::new(),
    };

    let mut gc_plan = GcPlan::default();

    if let Ok(entries) = std::fs::read_dir(ctx.mount_root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = parse_tower_worktree_id(&name) else {
                continue; // 非 Tower 命名：一律不碰。
            };
            if !entry.path().is_dir() {
                continue;
            }
            let has_git = entry.path().join(".git").exists();
            let reason = if !has_git {
                "no .git".to_string()
            } else if !registered.contains(&id) {
                "unregistered".to_string()
            } else {
                continue;
            };
            gc_plan.dead_dirs.push(DeadDir { name, worktree_id: id, reason });
        }
    }

    let uppers_root = ctx.store_root.join("worktrees");
    if let Ok(entries) = std::fs::read_dir(&uppers_root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = parse_tower_worktree_id(&name) else {
                continue;
            };
            if !entry.path().is_dir() || registered.contains(&id) {
                continue;
            }
            gc_plan.orphan_uppers.push(OrphanUpper {
                dir: entry.path().to_string_lossy().into_owned(),
                worktree_id: id,
            });
        }
    }

    if let Some(conn) = &conn {
        gc_plan.old_templates = old_templates(conn, referenced, ctx.max_age)?;
    }

    Ok(gc_plan)
}

/// 执行计划。逐项容错：单项失败记入 `GcReport::errors` 并继续。
pub fn apply(ctx: &GcContext<'_>, gc_plan: &GcPlan) -> anyhow::Result<GcReport> {
    let mut report = GcReport::default();
    let conn = open_meta_db(ctx.store_root)?;

    // plan→apply 之间的 TOCTOU 防护：wt-N 名字被移除又重建会复用同一 id，
    // 每个删除动作执行前按当前状态重新校验“仍是 dead”。
    let current_registered = match &conn {
        Some(conn) => registered_ids(conn)?,
        None => HashSet::new(),
    };

    for dead in &gc_plan.dead_dirs {
        let mounted_view = ctx.mounted;
        if mounted_view && ctx.registry.is_none() {
            report.errors.push(format!(
                "dead dir {}: mounted but no registry; cannot remove",
                dead.name
            ));
            continue;
        }
        let has_git_now = ctx.mount_root.join(&dead.name).join(".git").exists();
        let registered_now = current_registered.contains(&dead.worktree_id);
        if has_git_now && registered_now {
            // 名字被移除又重建（或仅补回了 .git）：不再是 dead，跳过。
            report
                .errors
                .push(format!("dead dir {} is live again; skipped", dead.name));
            continue;
        }
        // 1. 注销路由（在线模式：FS 视图目录随之消失）。
        if let Some(registry) = ctx.registry {
            if registry.get(&dead.name).is_some() {
                registry.unregister(&dead.name);
            }
        }
        // 2. 物理目录（仅离线模式存在真实目录可删）。
        if !ctx.mounted {
            let path = ctx.mount_root.join(&dead.name);
            if let Err(e) = std::fs::remove_dir_all(&path) {
                if path.exists() {
                    report
                        .errors
                        .push(format!("remove {}: {e}", path.display()));
                    continue;
                }
            }
        }
        // 3. DB 行（级联清 dedupe/whiteouts/renames/file_ids/stat_cache）。
        if let Some(conn) = &conn {
            if let Err(e) = conn.execute("DELETE FROM worktrees WHERE id = ?1", params![dead.worktree_id]) {
                report
                    .errors
                    .push(format!("delete worktree row {}: {e}", dead.worktree_id));
            }
        }
        // 4. upper 残留。
        remove_upper(ctx.store_root, dead.worktree_id, &mut report);
        report.removed_dead_dirs += 1;
    }

    for orphan in &gc_plan.orphan_uppers {
        let dir = PathBuf::from(&orphan.dir);
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            if dir.exists() {
                report.errors.push(format!("remove {}: {e}", dir.display()));
                continue;
            }
        }
        // `dir` 是 `<store>/worktrees/wt-<id>` 本身，已整体删除；其父目录
        // 是所有 worktree 共享的 worktrees 根，保留不动。
        report.removed_uppers += 1;
    }

    for template in &gc_plan.old_templates {
        if let Some(conn) = &conn {
            if let Err(e) = conn
                .execute(
                    "DELETE FROM templates WHERE id = ?1",
                    params![template.id],
                )
                .and_then(|_| {
                    // 外键开启时 template_files 级联；显式删一次兜底无 FK 的连接。
                    conn.execute(
                        "DELETE FROM template_files WHERE template_id = ?1",
                        params![template.id],
                    )
                })
            {
                report
                    .errors
                    .push(format!("delete template row {}: {e}", template.id));
                continue;
            }
        }
        let tree_dir = PathBuf::from(&template.tree_dir);
        if tree_dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&tree_dir) {
                report
                    .errors
                    .push(format!("remove template tree {}: {e}", tree_dir.display()));
                continue;
            }
        }
        report.removed_templates += 1;
    }

    Ok(report)
}

// ---- 帮助函数 ----

/// Tower worktree 目录名：`wt-<正整数>`。其他命名（dashboard-fork-*、
/// custom-tree 等）返回 `None`，GC 永不触碰。
pub fn parse_tower_worktree_id(name: &str) -> Option<i64> {
    let rest = name.strip_prefix("wt-")?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse::<i64>().ok().filter(|n| *n > 0)
}

fn remove_upper(store_root: &Path, id: i64, report: &mut GcReport) {
    let dir = store_root.join("worktrees").join(format!("wt-{id}"));
    if !dir.exists() {
        return;
    }
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        report.errors.push(format!("remove upper {}: {e}", dir.display()));
    }
}

/// 打开 store 的 meta.db（缺失 → `None`：全新 store 无可回收项）。
/// 独立连接 + WAL，与 daemon 的连接并存。
fn open_meta_db(store_root: &Path) -> anyhow::Result<Option<Connection>> {
    let path = store_root.join("meta.db");
    if !path.is_file() {
        return Ok(None);
    }
    let conn = Connection::open(&path)
        .map_err(|e| anyhow::anyhow!("open {}: {e}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("busy_timeout: {e}"))?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|e| anyhow::anyhow!("foreign_keys pragma: {e}"))?;
    Ok(Some(conn))
}

fn registered_ids(conn: &Connection) -> anyhow::Result<HashSet<i64>> {
    let mut stmt = conn
        .prepare("SELECT id FROM worktrees")
        .map_err(|e| anyhow::anyhow!("query worktrees: {e}"))?;
    let rows = stmt
        .query_map(params![], |r| r.get::<_, i64>(0))
        .map_err(|e| anyhow::anyhow!("query worktrees: {e}"))?;
    let mut out = HashSet::new();
    for row in rows {
        out.insert(row.map_err(|e| anyhow::anyhow!("query worktrees: {e}"))?);
    }
    Ok(out)
}

fn referenced_template_ids(conn: &Connection) -> anyhow::Result<HashSet<i64>> {
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT template_id FROM worktrees WHERE template_id IS NOT NULL",
        )
        .map_err(|e| anyhow::anyhow!("query referenced templates: {e}"))?;
    let rows = stmt
        .query_map(params![], |r| r.get::<_, i64>(0))
        .map_err(|e| anyhow::anyhow!("query referenced templates: {e}"))?;
    let mut out = HashSet::new();
    for row in rows {
        out.insert(row.map_err(|e| anyhow::anyhow!("query referenced templates: {e}"))?);
    }
    Ok(out)
}

fn old_templates(
    conn: &Connection,
    referenced: HashSet<i64>,
    max_age: Duration,
) -> anyhow::Result<Vec<OldTemplate>> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let cutoff = now_ms.saturating_sub(max_age.as_millis() as i64);

    let mut stmt = conn
        .prepare(
            "SELECT id, base_commit, state, tree_path, created_at
             FROM templates
             WHERE state != 'building' AND created_at < ?1
             ORDER BY id",
        )
        .map_err(|e| anyhow::anyhow!("query templates: {e}"))?;
    let rows = stmt
        .query_map(params![cutoff], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })
        .map_err(|e| anyhow::anyhow!("query templates: {e}"))?;
    let mut out = Vec::new();
    for row in rows {
        let (id, base_commit, state, tree_dir, created_at) =
            row.map_err(|e| anyhow::anyhow!("query templates: {e}"))?;
        if referenced.contains(&id) {
            continue;
        }
        let age_days =
            (now_ms - created_at).max(0) as f64 / (24.0 * 3600.0 * 1000.0);
        out.push(OldTemplate {
            id,
            base_commit,
            state,
            age_days: (age_days * 10.0).round() / 10.0,
            tree_dir,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wtfs_core::Db;

    struct Layout {
        dir: tempfile::TempDir,
        mount_root: PathBuf,
        store_root: PathBuf,
        conn: Connection,
    }

    fn layout() -> Layout {
        let dir = tempfile::tempdir().unwrap();
        let mount_root = dir.path().join(".tower").join("worktrees");
        let store_root = dir.path().join("store");
        std::fs::create_dir_all(&mount_root).unwrap();
        std::fs::create_dir_all(&store_root).unwrap();
        // schema 由 wtfs-core Db::open 创建；GC 用自己的连接读写同一文件。
        let db = Db::open(store_root.join("meta.db")).unwrap();
        drop(db);
        let conn = Connection::open(store_root.join("meta.db")).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        Layout { dir, mount_root, store_root, conn }
    }

    fn insert_worktree(l: &Layout, id: i64) {
        l.conn
            .execute(
                "INSERT INTO worktrees(id, upper_path, status, config_json, created_at)
                 VALUES (?1, 'x', 'ready', '{}', 0)",
                params![id],
            )
            .unwrap();
    }

    fn insert_template(l: &Layout, id: i64, created_at_ms: i64, state: &str, tree_dir: &Path) {
        l.conn
            .execute(
                "INSERT OR IGNORE INTO repos(id, root, created_at) VALUES (1, 'r', 0)",
                params![],
            )
            .unwrap();
        l.conn
            .execute(
                "INSERT INTO templates(id, repo_id, base_commit, tree_path, index_path, state, created_at)
                 VALUES (?1, 1, ?2, ?3, ?3, ?4, ?5)",
                params![id, format!("commit-{id}"), tree_dir.to_string_lossy(), state, created_at_ms],
            )
            .unwrap();
    }

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    fn ctx<'a>(l: &'a Layout, registry: Option<&'a wtfs_vfs::VfsRegistry>, mounted: bool) -> GcContext<'a> {
        GcContext {
            mount_root: &l.mount_root,
            store_root: &l.store_root,
            registry,
            mounted,
            max_age: Duration::from_secs(7 * 24 * 3600),
        }
    }

    #[test]
    fn parses_tower_names_only() {
        assert_eq!(parse_tower_worktree_id("wt-278"), Some(278));
        assert_eq!(parse_tower_worktree_id("wt-1"), Some(1));
        assert_eq!(parse_tower_worktree_id("wt-0"), None);
        assert_eq!(parse_tower_worktree_id("wt-x"), None);
        assert_eq!(parse_tower_worktree_id("wt-"), None);
        assert_eq!(parse_tower_worktree_id("wt-1a"), None);
        assert_eq!(parse_tower_worktree_id("dashboard-fork-1"), None);
        assert_eq!(parse_tower_worktree_id("custom-tree"), None);
        assert_eq!(parse_tower_worktree_id(""), None);
    }

    #[test]
    fn plan_and_apply_offline_respect_boundaries() {
        let l = layout();
        // 活跃 worktree：有 .git + 已注册 → 不动。
        std::fs::create_dir_all(l.mount_root.join("wt-6")).unwrap();
        std::fs::write(l.mount_root.join("wt-6").join(".git"), "gitdir: x").unwrap();
        insert_worktree(&l, 6);
        // dead：无 .git 但已注册。
        std::fs::create_dir_all(l.mount_root.join("wt-8").join("sub")).unwrap();
        insert_worktree(&l, 8);
        // dead：有 .git 但未注册。
        std::fs::create_dir_all(l.mount_root.join("wt-7")).unwrap();
        std::fs::write(l.mount_root.join("wt-7").join(".git"), "gitdir: x").unwrap();
        // 非 Tower 目录：永不动。
        std::fs::create_dir_all(l.mount_root.join("dashboard-fork-1")).unwrap();
        std::fs::write(l.mount_root.join("dashboard-fork-1").join("keep.txt"), "x").unwrap();
        // 非 wt-N 命名的注册目录：永不动。
        std::fs::create_dir_all(l.mount_root.join("custom-tree")).unwrap();

        // 孤儿 upper（无注册行）。
        let orphan_upper = l.store_root.join("worktrees").join("wt-99").join("upper");
        std::fs::create_dir_all(&orphan_upper).unwrap();
        // 存活 upper（有注册行）→ 不动。
        let live_upper = l.store_root.join("worktrees").join("wt-6").join("upper");
        std::fs::create_dir_all(&live_upper).unwrap();

        // 旧模板（无引用、30 天前）→ 回收；新鲜模板与被引用模板 → 不动。
        let old_tree = l.dir.path().join("store").join("templates").join("old");
        std::fs::create_dir_all(&old_tree).unwrap();
        std::fs::write(old_tree.join("f.txt"), "x").unwrap();
        insert_template(&l, 11, now_ms() - 30 * 24 * 3600 * 1000, "ready", &old_tree);
        let fresh_tree = l.dir.path().join("store").join("templates").join("fresh");
        std::fs::create_dir_all(&fresh_tree).unwrap();
        insert_template(&l, 12, now_ms(), "ready", &fresh_tree);
        let referenced_tree = l.dir.path().join("store").join("templates").join("refd");
        std::fs::create_dir_all(&referenced_tree).unwrap();
        insert_template(&l, 13, now_ms() - 30 * 24 * 3600 * 1000, "ready", &referenced_tree);
        l.conn
            .execute(
                "UPDATE worktrees SET template_id = 13 WHERE id = 6",
                params![],
            )
            .unwrap();
        // building 状态：即便旧也不回收。
        let building_tree = l.dir.path().join("store").join("templates").join("bld");
        std::fs::create_dir_all(&building_tree).unwrap();
        insert_template(&l, 14, now_ms() - 30 * 24 * 3600 * 1000, "building", &building_tree);

        let gc_ctx = ctx(&l, None, false);
        let gc_plan = plan(&gc_ctx).unwrap();

        assert_eq!(
            gc_plan
                .dead_dirs
                .iter()
                .map(|d| (d.name.as_str(), d.reason.as_str()))
                .collect::<Vec<_>>(),
            vec![("wt-7", "unregistered"), ("wt-8", "no .git")]
        );
        assert_eq!(gc_plan.orphan_uppers.len(), 1);
        assert_eq!(gc_plan.orphan_uppers[0].worktree_id, 99);
        assert_eq!(
            gc_plan.old_templates.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![11]
        );
        assert!(!gc_plan.is_empty());

        let report = apply(&gc_ctx, &gc_plan).unwrap();
        assert_eq!(report.removed_dead_dirs, 2);
        assert_eq!(report.removed_uppers, 1);
        assert_eq!(report.removed_templates, 1);
        assert!(report.errors.is_empty(), "errors: {:?}", report.errors);

        // 活跃项完好。
        assert!(l.mount_root.join("wt-6").join(".git").exists());
        assert!(live_upper.exists());
        assert!(l.mount_root.join("dashboard-fork-1").join("keep.txt").exists());
        assert!(l.mount_root.join("custom-tree").exists());
        assert!(fresh_tree.exists());
        assert!(referenced_tree.exists());
        assert!(building_tree.exists());
        // dead 已清。
        assert!(!l.mount_root.join("wt-7").exists());
        assert!(!l.mount_root.join("wt-8").exists());
        assert!(!orphan_upper.exists());
        assert!(!old_tree.exists());
        // DB 行已删（8）、保留（6）。
        let count: i64 = l
            .conn
            .query_row("SELECT COUNT(*) FROM worktrees", params![], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let count: i64 = l
            .conn
            .query_row("SELECT COUNT(*) FROM templates", params![], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn daemon_mode_unregisters_and_skips_physical_mount_dir() {
        let l = layout();
        std::fs::create_dir_all(l.mount_root.join("wt-5").join("sub")).unwrap();
        std::fs::write(l.mount_root.join("wt-5").join("stale.txt"), "x").unwrap();
        insert_worktree(&l, 5);

        let registry_path = l.dir.path().join("vfs-store");
        let registry = wtfs_vfs::VfsRegistry::open(wtfs_vfs::VfsConfig::new(
            l.dir.path().join("repo"),
            registry_path,
        ))
        .unwrap();
        registry.auto_register("wt-5").unwrap();
        assert!(registry.get("wt-5").is_some());

        // 上游 upper 真实目录（daemon 模式也必须清）。
        let upper = l.store_root.join("worktrees").join("wt-5").join("upper");
        std::fs::create_dir_all(&upper).unwrap();

        let gc_ctx = ctx(&l, Some(&registry), true);
        let gc_plan = plan(&gc_ctx).unwrap();
        assert_eq!(gc_plan.dead_dirs.len(), 1);
        let report = apply(&gc_ctx, &gc_plan).unwrap();
        assert_eq!(report.removed_dead_dirs, 1);
        assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
        assert!(registry.get("wt-5").is_none(), "路由已注销");
        assert!(!upper.exists(), "upper 已清");
        assert!(
            l.mount_root.join("wt-5").exists(),
            "mounted 模式不物理删挂载视图目录（由 FS 注销）"
        );
    }

    #[test]
    fn empty_store_plans_nothing() {
        // store 无 meta.db（全新布局）：计划为空，不报错。
        // 注意：不能先建库再删文件——Windows 上 SQLite 连接持有句柄时删除失败。
        let dir = tempfile::tempdir().unwrap();
        let mount_root = dir.path().join(".tower").join("worktrees");
        let store_root = dir.path().join("store");
        std::fs::create_dir_all(&mount_root).unwrap();
        std::fs::create_dir_all(&store_root).unwrap();
        let gc_ctx = GcContext {
            mount_root: &mount_root,
            store_root: &store_root,
            registry: None,
            mounted: false,
            max_age: Duration::from_secs(7 * 24 * 3600),
        };
        let gc_plan = plan(&gc_ctx).unwrap();
        assert!(gc_plan.is_empty());
    }
}
