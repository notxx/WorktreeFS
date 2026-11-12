//! 模板存储的 SQLite 实现（spec §6.2：[`TemplateStore`] 契约）。
//!
//! 创建/更新流程：
//!
//! 1. `git rev-parse` 把 `base_ref` 解析为 commit；
//! 2. 已有 Ready 模板且 tree/index 完好 → 直接复用（幂等）；
//! 3. 否则（加进程内构建锁）先落 `state='building'` 行占位，
//!    `git worktree add --detach` 正常 checkout 到 `<store>/templates/<commit>/tree`
//!    （复用 Git 的 filter/CRLF/LFS 结果）；
//! 4. `git update-index --refresh` 后把 worktree index 快照到 `<commit>/index`；
//! 5. `git ls-files --stage -z` 取 mode/路径清单，逐文件流式 blake3；
//!    保留名/大小写冲突/非 UTF-8 路径在此时拒绝（spec §11.2）；
//! 6. 删除模板 `.git` 文件并 `git worktree prune`（模板不再跑 Git）；
//! 7. 单事务写入 `template_files` 并把模板置 `state='ready'`（原子切换）。
//!
//! 任一步失败：模板行置 `failed`、清理半成品目录；模板缺失时调用方
//! 全部写 upper（全量 copy），正确性不受影响。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use rusqlite::{params, OptionalExtension, Transaction};

use crate::db::{db_err, now_ms};
use crate::gitx;
use crate::{Db, Error, Result, Template, TemplateFile, TemplateId, TemplateState, TemplateStore, validate_relative_path};

/// 模板注册表：SQLite + 模板树/快照落盘。`store_root` 即 spec §8.1 的
/// `<repo-id>/` 目录（模板位于 `<store_root>/templates/<commit>/`）。
pub struct SqliteTemplateStore {
    db: Db,
    store_root: PathBuf,
    /// 模板更新期间加锁（spec §6.2）；`resolve` 只查不加锁。
    build_lock: Mutex<()>,
}

impl SqliteTemplateStore {
    /// 以 `store_root` 为模板根打开存储；元数据放在 `<store_root>/meta.db`。
    pub fn open(store_root: impl Into<PathBuf>) -> Result<Self> {
        let store_root = store_root.into();
        std::fs::create_dir_all(&store_root)?;
        let db = Db::open(store_root.join("meta.db"))?;
        Ok(Self {
            db,
            store_root,
            build_lock: Mutex::new(()),
        })
    }

    /// 复用已有 [`Db`]（与 overlay 元数据共享同一 meta.db 时使用）。
    pub fn with_db(db: Db, store_root: impl Into<PathBuf>) -> Result<Self> {
        let store_root = store_root.into();
        std::fs::create_dir_all(&store_root)?;
        Ok(Self {
            db,
            store_root,
            build_lock: Mutex::new(()),
        })
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn store_root(&self) -> &Path {
        &self.store_root
    }

    /// 指定模板的当前状态；行不存在返回 `None`。
    pub fn state(&self, id: TemplateId) -> Result<Option<TemplateState>> {
        let conn = self.db.lock();
        let state: Option<String> = conn
            .query_row(
                "SELECT state FROM templates WHERE id = ?1",
                params![id.0],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?;
        Ok(state.map(|s| parse_state(&s)))
    }

    /// repo + commit 的 Ready 模板（tree 与 index 快照仍在磁盘上才返回）。
    fn lookup_ready(&self, repo_root: &Path, commit: &str) -> Result<Option<Template>> {
        let norm = normalize_root(repo_root);
        let conn = self.db.lock();
        let mut stmt = conn
            .prepare(
                "SELECT t.id, t.tree_path, t.index_path
                 FROM templates t JOIN repos r ON r.id = t.repo_id
                 WHERE r.root = ?1 AND t.base_commit = ?2 AND t.state = 'ready'",
            )
            .map_err(db_err)?;
        let row = stmt
            .query_row(params![norm, commit], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .optional()
            .map_err(db_err)?;
        drop(stmt);
        drop(conn);
        match row {
            Some((id, tree, index)) => {
                let tree = PathBuf::from(tree);
                let index = PathBuf::from(index);
                if tree.is_dir() && index.is_file() {
                    Ok(Some(Template {
                        id: TemplateId(id),
                        repo_root: repo_root.to_path_buf(),
                        base_commit: commit.to_string(),
                        tree_path: tree,
                        index_path: index,
                        state: TemplateState::Ready,
                    }))
                } else {
                    // 行在但磁盘被清：视为不存在，走重建。
                    Ok(None)
                }
            }
            None => Ok(None),
        }
    }

    /// 占位行（state=building）：同 commit 的失败/中断旧行先清理。
    fn reserve_row(
        &self,
        repo: &Path,
        commit: &str,
        tree: &Path,
        index_path: &Path,
    ) -> Result<TemplateId> {
        self.db.with_tx(|tx| {
            let repo_id = upsert_repo(tx, repo, true)?;
            // 旧行清理：非 ready（上次失败/中断），或 ready 但 tree/index 已被
            // 外部删除——否则 UNIQUE(repo_id, base_commit) 会让重建永远失败（P2-1）。
            let stale: Vec<i64> = {
                let mut stmt = tx
                    .prepare(
                        "SELECT id, tree_path, index_path, state FROM templates
                         WHERE repo_id = ?1 AND base_commit = ?2",
                    )
                    .map_err(db_err)?;
                let rows = stmt
                    .query_map(params![repo_id, commit], |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                        ))
                    })
                    .map_err(db_err)?;
                let mut ids = Vec::new();
                for row in rows {
                    let (id, row_tree, row_index, state) = row.map_err(db_err)?;
                    let intact = state == "ready"
                        && Path::new(&row_tree).is_dir()
                        && Path::new(&row_index).is_file();
                    if !intact {
                        ids.push(id);
                    }
                }
                ids
            };
            for old in stale {
                tx.execute(
                    "UPDATE worktrees SET template_id = NULL WHERE template_id = ?1",
                    params![old],
                )
                .map_err(db_err)?;
                tx.execute(
                    "DELETE FROM template_files WHERE template_id = ?1",
                    params![old],
                )
                .map_err(db_err)?;
                tx.execute("DELETE FROM templates WHERE id = ?1", params![old])
                    .map_err(db_err)?;
            }
            tx.execute(
                "INSERT INTO templates(repo_id, base_commit, tree_path, index_path, state, created_at)
                 VALUES (?1, ?2, ?3, ?4, 'building', ?5)",
                params![
                    repo_id,
                    commit,
                    tree.to_string_lossy(),
                    index_path.to_string_lossy(),
                    now_ms()
                ],
            )
            .map_err(db_err)?;
            Ok(TemplateId(tx.last_insert_rowid()))
        })
    }

    /// checkout + 刷新 + index 快照 + 文件清单/哈希 + 模板脱离 git。
    fn materialize(
        &self,
        repo: &Path,
        commit: &str,
        tree: &Path,
        index_path: &Path,
    ) -> Result<Vec<TemplateFile>> {
        if let Some(parent) = tree.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if tree.exists() {
            std::fs::remove_dir_all(tree)?;
        }
        gitx::require_ok(
            &gitx::run_git(
                repo,
                &["worktree", "add", "--detach", "--quiet", &gitx::git_path(tree), commit],
            )?,
            "git worktree add",
        )?;
        // 全新 checkout 本应干净；exit 1 表示确有 stat 更新，同样接受。
        let refresh = gitx::run_git(tree, &["update-index", "--refresh"])?;
        if !refresh.ok() && refresh.status != Some(1) {
            gitx::require_ok(&refresh, "git update-index --refresh")?;
        }
        // index 快照：linked worktree 的 index 在 <主仓库>/.git/worktrees/<name>/index。
        let gitdir = gitx::worktree_git_dir(tree)?;
        let src_index = gitdir.join("index");
        if !src_index.is_file() {
            return Err(Error::Template(format!(
                "worktree index not found: {}",
                src_index.display()
            )));
        }
        std::fs::copy(&src_index, index_path)?;

        let entries = self.scan_files(tree)?;

        // 模板脱离 git：删 `.git` 文件并清理主仓库里的 worktree 元数据。
        let dot_git = tree.join(".git");
        if dot_git.is_file() {
            std::fs::remove_file(&dot_git)?;
        }
        gitx::require_ok(&gitx::run_git(repo, &["worktree", "prune"])?, "git worktree prune")?;
        Ok(entries)
    }

    /// `git ls-files --stage -z` + 逐文件流式 blake3；路径校验与大小写冲突检测。
    fn scan_files(&self, tree: &Path) -> Result<Vec<TemplateFile>> {
        let out = gitx::run_git(tree, &["ls-files", "--stage", "-z"])?;
        gitx::require_ok(&out, "git ls-files")?;
        let mut entries = Vec::new();
        let mut seen_lower = HashSet::new();
        for record in out.stdout.split(|b| *b == 0) {
            if record.is_empty() {
                continue;
            }
            let tab = record
                .iter()
                .position(|b| *b == b'\t')
                .ok_or_else(|| Error::Template("malformed ls-files record".into()))?;
            let meta = std::str::from_utf8(&record[..tab])
                .map_err(|_| Error::Template("non-UTF-8 ls-files meta".into()))?;
            let path = std::str::from_utf8(&record[tab + 1..])
                .map_err(|_| Error::InvalidPath {
                    path: "<non-utf-8>".into(),
                    reason: "non-UTF-8 path in template (spec §11.2)".into(),
                })?
                .to_string();
            let mode_str = meta.split(' ').next().unwrap_or_default();
            let mut mode = u32::from_str_radix(mode_str, 8)
                .map_err(|_| Error::Template(format!("bad git mode {mode_str:?}")))?;
            if mode == 0o160000 {
                // gitlink/submodule：v1 检测到即报错回退（spec §11.2）。
                return Err(Error::Template(format!("submodule entries unsupported: {path}")));
            }
            crate::validate_relative_path(&path)?;
            let lower = path.to_ascii_lowercase();
            if !seen_lower.insert(lower) {
                return Err(Error::InvalidPath {
                    path: path.clone(),
                    reason: "case-insensitive duplicate path".into(),
                });
            }
            // Windows 上 core.symlinks=false，symlink 以普通文件落地。
            if mode == 0o120000 {
                mode = 0o100644;
            }
            let full = tree.join(&path);
            let (hash, size) = dedupe_file(&full, &path)?;
            entries.push(TemplateFile { path, hash, size, mode });
        }
        Ok(entries)
    }

    /// 单事务写 `template_files` 并原子切换到 ready。
    fn persist_ready(
        &self,
        id: TemplateId,
        tree: &Path,
        index_path: &Path,
        entries: &[TemplateFile],
    ) -> Result<()> {
        self.db.with_tx(|tx| {
            tx.execute(
                "DELETE FROM template_files WHERE template_id = ?1",
                params![id.0],
            )
            .map_err(db_err)?;
            {
                let mut stmt = tx
                    .prepare(
                        "INSERT INTO template_files(template_id, path, hash, size, mode)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                    )
                    .map_err(db_err)?;
                for f in entries {
                    stmt.execute(params![
                        id.0,
                        f.path,
                        f.hash.to_hex(),
                        f.size as i64,
                        f.mode as i64
                    ])
                    .map_err(db_err)?;
                }
            }
            tx.execute(
                "UPDATE templates SET tree_path = ?1, index_path = ?2, state = 'ready' WHERE id = ?3",
                params![tree.to_string_lossy(), index_path.to_string_lossy(), id.0],
            )
            .map_err(db_err)?;
            Ok(())
        })
    }

    fn build(&self, repo: &Path, commit: &str) -> Result<Template> {
        let dir = self.store_root.join("templates").join(commit);
        let tree = dir.join("tree");
        let index_path = dir.join("index");
        let id = self.reserve_row(repo, commit, &tree, &index_path)?;
        match self.materialize(repo, commit, &tree, &index_path) {
            Ok(entries) => {
                self.persist_ready(id, &tree, &index_path, &entries)?;
                Ok(Template {
                    id,
                    repo_root: repo.to_path_buf(),
                    base_commit: commit.to_string(),
                    tree_path: tree,
                    index_path,
                    state: TemplateState::Ready,
                })
            }
            Err(e) => {
                let _ = self.db.with_tx(|tx| {
                    tx.execute(
                        "UPDATE templates SET state = 'failed' WHERE id = ?1",
                        params![id.0],
                    )
                    .map_err(db_err)?;
                    Ok(())
                });
                let _ = std::fs::remove_dir_all(&dir);
                Err(e)
            }
        }
    }
}

fn dedupe_file(full: &Path, path: &str) -> Result<(crate::FileHash, u64)> {
    crate::dedupe::hash_file(full).map_err(|e| {
        Error::Template(format!("template file unreadable: {path}: {e}"))
    })
}

fn parse_state(s: &str) -> TemplateState {
    match s {
        "ready" => TemplateState::Ready,
        "failed" => TemplateState::Failed,
        _ => TemplateState::Building,
    }
}

fn template_from_row(
    id: i64,
    base_commit: String,
    tree_path: String,
    index_path: String,
    state: String,
    repo_root: String,
) -> Template {
    Template {
        id: TemplateId(id),
        repo_root: PathBuf::from(repo_root),
        base_commit,
        tree_path: PathBuf::from(tree_path),
        index_path: PathBuf::from(index_path),
        state: parse_state(&state),
    }
}

/// 路径规范化（大小写敏感，仅统一分隔符/去尾斜杠/盘符小写），作为 `repos.root` 键。
pub(crate) fn normalize_root(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/');
    let mut owned = s.to_string();
    if owned.len() >= 2 && owned.as_bytes()[1] == b':' {
        owned[..1].make_ascii_lowercase();
    }
    owned
}

/// `template_files.hash` 列（hex 字符串）→ [`crate::FileHash`]，供 rusqlite 行闭包使用。
fn hash_from_hex(hex: &str) -> rusqlite::Result<crate::FileHash> {
    crate::FileHash::from_hex(hex).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            Box::new(e),
        )
    })
}

/// repo 行 upsert；`probe_common` 时尝试 `git rev-parse --git-common-dir`
/// 填 `common_git_dir`（overlay 注册等无 git 上下文的调用传 `false`）。
pub(crate) fn upsert_repo(
    tx: &Transaction<'_>,
    repo_root: &Path,
    probe_common: bool,
) -> Result<i64> {
    let norm = normalize_root(repo_root);
    let existing: Option<i64> = tx
        .query_row("SELECT id FROM repos WHERE root = ?1", params![norm], |r| r.get(0))
        .optional()
        .map_err(db_err)?;
    if let Some(id) = existing {
        return Ok(id);
    }
    let common = if probe_common {
        gitx::git_common_dir(repo_root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        String::new()
    };
    tx.execute(
        "INSERT INTO repos(root, common_git_dir, created_at) VALUES (?1, ?2, ?3)",
        params![norm, common, now_ms()],
    )
    .map_err(db_err)?;
    Ok(tx.last_insert_rowid())
}

impl TemplateStore for SqliteTemplateStore {
    /// 按 repo + base ref 精确解析已有 Ready 模板；只查不建、不加更新锁。
    /// ref 解析失败（含 repo 不可用）返回 `Ok(None)`，让调用方走兜底。
    fn resolve(&self, spec: &crate::TemplateSpec) -> Result<Option<Template>> {
        let commit = match gitx::rev_parse_commit(&spec.repo_root, &spec.base_ref) {
            Ok(c) => c,
            Err(_) => return Ok(None),
        };
        self.lookup_ready(&spec.repo_root, &commit)
    }

    fn create_or_update(&self, spec: &crate::TemplateSpec) -> Result<Template> {
        let repo = spec.repo_root.clone();
        if !repo.is_dir() {
            return Err(Error::Template(format!(
                "repo root not found: {}",
                repo.display()
            )));
        }
        let commit = gitx::rev_parse_commit(&repo, &spec.base_ref)?;
        let _guard = self.build_lock.lock();
        if let Some(existing) = self.lookup_ready(&repo, &commit)? {
            return Ok(existing);
        }
        self.build(&repo, &commit)
    }

    fn get(&self, id: TemplateId) -> Result<Option<Template>> {
        let conn = self.db.lock();
        conn.query_row(
            "SELECT t.id, t.base_commit, t.tree_path, t.index_path, t.state, r.root
             FROM templates t JOIN repos r ON r.id = t.repo_id
             WHERE t.id = ?1",
            params![id.0],
            |r| {
                Ok(template_from_row(
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()
        .map_err(db_err)
    }

    fn files(&self, id: TemplateId) -> Result<Vec<TemplateFile>> {
        if self.get(id)?.is_none() {
            return Err(Error::NotFound(id.to_string()));
        }
        let conn = self.db.lock();
        let mut stmt = conn
            .prepare(
                "SELECT path, hash, size, mode FROM template_files
                 WHERE template_id = ?1 ORDER BY path",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![id.0], |r| {
                let hex: String = r.get(1)?;
                Ok(TemplateFile {
                    path: r.get(0)?,
                    hash: hash_from_hex(&hex)?,
                    size: r.get::<_, i64>(2)? as u64,
                    mode: r.get::<_, i64>(3)? as u32,
                })
            })
            .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(db_err)?);
        }
        Ok(out)
    }

    fn file(&self, id: TemplateId, path: &str) -> Result<Option<TemplateFile>> {
        // 非法路径不可能存在于模板（创建时已校验），直接视为未命中。
        if validate_relative_path(path).is_err() {
            return Ok(None);
        }
        let conn = self.db.lock();
        conn.query_row(
            "SELECT path, hash, size, mode FROM template_files
             WHERE template_id = ?1 AND path = ?2",
            params![id.0, path],
            |r| {
                let hex: String = r.get(1)?;
                Ok(TemplateFile {
                    path: r.get(0)?,
                    hash: hash_from_hex(&hex)?,
                    size: r.get::<_, i64>(2)? as u64,
                    mode: r.get::<_, i64>(3)? as u32,
                })
            },
        )
        .optional()
        .map_err(db_err)
    }
}

/// 对象安全冒烟：`Arc<dyn TemplateStore>` 是 M3（VFS）/M4（daemon）的使用形态。
#[allow(dead_code)]
fn assert_object_safe(store: Arc<SqliteTemplateStore>) -> Arc<dyn TemplateStore> {
    store as Arc<dyn TemplateStore>
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn run_git_ok(repo: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("git spawn");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// 四个文件（含嵌套目录与可执行位）的最小仓库。
    fn init_repo(root: &Path) -> PathBuf {
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run_git_ok(&repo, &["init", "-b", "master"]);
        run_git_ok(&repo, &["config", "user.email", "wtfs-test@example.com"]);
        run_git_ok(&repo, &["config", "user.name", "wtfs-test"]);
        std::fs::write(repo.join("README.md"), "hello wtfs\n").unwrap();
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        std::fs::write(repo.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        std::fs::write(repo.join("src/deep/nested.txt"), "nested\n").unwrap();
        std::fs::create_dir_all(repo.join("bin")).unwrap();
        std::fs::write(repo.join("bin/run.sh"), "#!/bin/sh\necho hi\n").unwrap();
        run_git_ok(&repo, &["add", "-A"]);
        run_git_ok(&repo, &["update-index", "--chmod=+x", "bin/run.sh"]);
        run_git_ok(&repo, &["commit", "-m", "init", "--quiet"]);
        repo
    }

    fn head_commit(repo: &Path) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "master"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn template_create_snapshots_index_and_hashes_files() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo(dir.path());
        let store = SqliteTemplateStore::open(dir.path().join("store")).unwrap();
        let spec = crate::TemplateSpec::new(&repo, "master");

        let t = store.create_or_update(&spec).unwrap();
        assert_eq!(t.state, TemplateState::Ready);
        assert_eq!(t.base_commit, head_commit(&repo));
        assert!(t.tree_path.join("README.md").is_file());
        assert!(t.tree_path.join("src/deep/nested.txt").is_file());
        assert!(t.index_path.is_file());
        assert!(t.index_path.metadata().unwrap().len() > 0);
        // 模板脱离 git：`.git` 文件已删。
        assert!(!t.tree_path.join(".git").exists());

        let files = store.files(t.id).unwrap();
        assert_eq!(files.len(), 4);
        let by_path: std::collections::HashMap<&str, &TemplateFile> =
            files.iter().map(|f| (f.path.as_str(), f)).collect();
        // checkout 可能按 autocrlf 转换换行：哈希/大小与磁盘实际内容比对。
        let readme_bytes = std::fs::read(t.tree_path.join("README.md")).unwrap();
        let readme = by_path.get("README.md").unwrap();
        assert_eq!(readme.hash, crate::FileHash::of_bytes(&readme_bytes));
        assert_eq!(readme.size, readme_bytes.len() as u64);
        assert_eq!(readme.mode, 0o100644);
        assert_eq!(by_path.get("bin/run.sh").unwrap().mode, 0o100755);
        let nested_bytes = std::fs::read(t.tree_path.join("src/deep/nested.txt")).unwrap();
        assert_eq!(
            by_path.get("src/deep/nested.txt").unwrap().hash,
            crate::FileHash::of_bytes(&nested_bytes)
        );

        // 单文件查询（dedupe 热路径）。
        let lib_bytes = std::fs::read(t.tree_path.join("src/lib.rs")).unwrap();
        let one = store.file(t.id, "src/lib.rs").unwrap().unwrap();
        assert_eq!(one.hash, crate::FileHash::of_bytes(&lib_bytes));
        assert!(store.file(t.id, "missing.txt").unwrap().is_none());
        assert!(store.file(t.id, "../escape").unwrap().is_none());

        // 幂等：同 base 再 create 返回同一模板。
        let again = store.create_or_update(&spec).unwrap();
        assert_eq!(again.id, t.id);
        // resolve 命中 Ready 模板。
        assert_eq!(store.resolve(&spec).unwrap().unwrap().id, t.id);
        // get/状态查询。
        assert_eq!(store.get(t.id).unwrap().unwrap().base_commit, t.base_commit);
        assert_eq!(store.state(t.id).unwrap(), Some(TemplateState::Ready));
        assert!(store.get(TemplateId(999)).unwrap().is_none());
        assert!(matches!(
            store.files(TemplateId(999)).unwrap_err(),
            Error::NotFound(_)
        ));
    }

    #[test]
    fn template_bad_ref_and_missing_repo_fail_cleanly() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo(dir.path());
        let store = SqliteTemplateStore::open(dir.path().join("store")).unwrap();

        let err = store
            .create_or_update(&crate::TemplateSpec::new(&repo, "no-such-ref"))
            .unwrap_err();
        assert!(matches!(err, Error::Template(_)));
        // 失败不留 Ready 模板，也没有半成品目录。
        assert!(store.resolve(&crate::TemplateSpec::new(&repo, "master")).unwrap().is_none());

        let err = store
            .create_or_update(&crate::TemplateSpec::new(dir.path().join("nope"), "master"))
            .unwrap_err();
        assert!(err.to_string().contains("repo root not found"));

        assert!(store.resolve(&crate::TemplateSpec::new(&repo, "no-such-ref")).unwrap().is_none());
    }

    #[test]
    fn template_update_to_new_commit_creates_second_template() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo(dir.path());
        let store = SqliteTemplateStore::open(dir.path().join("store")).unwrap();
        let t1 = store.create_or_update(&crate::TemplateSpec::new(&repo, "master")).unwrap();

        std::fs::write(repo.join("src/lib.rs"), "pub fn g() -> u32 { 1 }\n").unwrap();
        run_git_ok(&repo, &["add", "-A"]);
        run_git_ok(&repo, &["commit", "-m", "second", "--quiet"]);

        let t2 = store.create_or_update(&crate::TemplateSpec::new(&repo, "master")).unwrap();
        assert_ne!(t1.id, t2.id);
        assert_ne!(t1.base_commit, t2.base_commit);
        let files = store.files(t2.id).unwrap();
        let lib = files.iter().find(|f| f.path == "src/lib.rs").unwrap();
        let lib_bytes = std::fs::read(t2.tree_path.join("src/lib.rs")).unwrap();
        assert_eq!(lib.hash, crate::FileHash::of_bytes(&lib_bytes));
        // 旧模板仍可用（gc 由 daemon 决定）。
        assert_eq!(store.get(t1.id).unwrap().unwrap().state, TemplateState::Ready);
    }

    #[test]
    fn template_rebuild_after_external_tree_deletion() {
        // P2-1 回归：Ready 行的 tree/index 被外部删除后必须能重建，
        // 不再撞 UNIQUE(repo_id, base_commit)。重建成功本身即证明旧行
        // 已被 reserve_row 清理（否则 INSERT 会触发 UNIQUE 约束）。
        let dir = TempDir::new().unwrap();
        let repo = init_repo(dir.path());
        let store = SqliteTemplateStore::open(dir.path().join("store")).unwrap();
        let spec = crate::TemplateSpec::new(&repo, "master");
        store.create_or_update(&spec).unwrap();

        std::fs::remove_dir_all(dir.path().join("store").join("templates")).unwrap();
        assert!(
            store.resolve(&spec).unwrap().is_none(),
            "tree 缺失不得再解析出模板"
        );

        let t2 = store.create_or_update(&spec).unwrap();
        assert_eq!(t2.state, TemplateState::Ready);
        assert_eq!(store.files(t2.id).unwrap().len(), 4);
        assert!(t2.tree_path.join("README.md").is_file());
        assert_eq!(store.resolve(&spec).unwrap().unwrap().id, t2.id);
    }

    #[test]
    fn normalize_root_unifies_separators_and_drive_case() {
        assert_eq!(normalize_root(Path::new("C:\\a\\b\\")), "c:/a/b");
        assert_eq!(normalize_root(Path::new("d:/x")), "d:/x");
    }
}
