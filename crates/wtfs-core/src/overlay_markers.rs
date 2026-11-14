//! overlay 标记树（M12/B120100）：whiteout/opaque/redirect 的 OverlayFS 式
//! 镜像目录树，替代 v3 及以前的 `whiteouts` / `renames` 两张 SQLite 表。
//!
//! 布局（每个 worktree 一份，与 upper 同根）：
//!
//! ```text
//! <store>/worktrees/<id>/
//! ├─ upper/                  # 改动文件（数据真相）
//! └─ meta/                   # 标记树根（= upper 物理路径的父目录 / meta）
//!    ├─ whiteouts/<rel>      # 0 字节标记文件：删除的 lower 文件
//!    ├─ opaque/<rel>         # 0 字节标记文件：删除的 lower 目录（隐藏整棵 lower 子树）
//!    └─ redirect/<view-rel>  # 内容 = 原路径：目录级改名的 lower 侧映射
//! ```
//!
//! * 标记路径与 overlay 相对路径一一对应（lower 命名空间；redirect 以改名后
//!   的视图路径为键）。存在性检查 O(1)；枚举某目录的标记子项只 read_dir
//!   对应目录，绝不全树扫描。
//! * opaque 选 0 字节文件而非目录标记：与 whiteout 同构，撤销 = 单个
//!   unlink，枚举/清理逻辑完全一致。
//! * 写路径 O(1)：一次 `create_dir_all` + 一个空文件（或 unlink），**零
//!   SQLite 事务**——删除大仓库时每文件只付一次文件系统元数据操作的代价。
//! * 读取方（[`crate::overlay::Overlay`]）在打开时把标记树一次性装载进内存
//!   镜像（[`MarkerState`]），热路径零标记 I/O；标记文件是持久真相，内存
//!   镜像在每次写路径同步维护（同锁），两者不发散。
//! * 注销 worktree = 整树删除（[`MarkerStore::wipe`]），不再逐行 DELETE。
//!
//! `whiteouts/` 与 `opaque/` 在"隐藏 lower"语义上同构（阻止该路径及其子树
//! 的 lower 命中），分两棵树只为语义可读与镜像 OverlayFS 的概念；redirect
//! 独立：键是视图路径，内容是 lower 命名空间的原始路径。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::{Error, Result, validate_relative_path};

/// 由 upper 物理根推导标记树根：`<...>/upper` → `<...>/meta`。
/// 注册（`worktrees.upper_path`）与运行期使用同一推导，保证迁移/注销一致。
pub fn marker_root_for_upper(upper_root: &Path) -> PathBuf {
    upper_root
        .parent()
        .map(|p| p.join("meta"))
        .unwrap_or_else(|| PathBuf::from("meta"))
}

/// 标记树的三棵子树名。
const WHITEOUTS: &str = "whiteouts";
const OPAQUE: &str = "opaque";
const REDIRECT: &str = "redirect";

/// 标记树的内存镜像（[`MarkerStore::load`] 一次性装载）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MarkerState {
    /// whiteout 标记路径（lower 命名空间）。
    pub whites: HashSet<String>,
    /// opaque 目录标记路径（lower 命名空间）。
    pub opaques: HashSet<String>,
    /// 目录改名映射 (from → to)：redirect 标记在 `to`（视图路径），
    /// 内容为 `from`（lower 命名空间）。与 v3 `renames` 表的行序一致。
    pub redirects: Vec<(String, String)>,
}

impl MarkerState {
    /// 合并的"隐藏 lower"集合：whiteout ∪ opaque（两者对解析语义同构）。
    pub fn blockers(&self) -> HashSet<String> {
        self.whites.iter().chain(self.opaques.iter()).cloned().collect()
    }
}

/// 某个 worktree 的标记树句柄（路径推导 + 文件操作；无内部状态）。
#[derive(Debug, Clone)]
pub struct MarkerStore {
    root: PathBuf,
}

impl MarkerStore {
    /// 以标记树根构造（调用方通常用 [`marker_root_for_upper`] 推导）。
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 标记文件的绝对路径：`<root>/<kind>/<rel>`。
    fn marker_path(&self, kind: &str, rel: &str) -> PathBuf {
        self.root.join(kind).join(rel)
    }

    /// 撤销标记后收敛空父目录（best-effort，防反复增删留下空目录堆积）。
    /// 从标记文件父目录向上删除空目录，直到 kind 子树根（含）。
    fn prune_empty_parents(&self, kind: &str, rel: &str) {
        let mut cur = self.marker_path(kind, rel).parent().map(|p| p.to_path_buf());
        let stop = self.root.join(kind);
        while let Some(dir) = cur {
            if dir == stop {
                break;
            }
            // 只删空目录；非空/不存在立即停止。
            let is_empty = std::fs::read_dir(&dir).map(|mut rd| rd.next().is_none()).unwrap_or(false);
            if !is_empty || std::fs::remove_dir(&dir).is_err() {
                break;
            }
            cur = dir.parent().map(|p| p.to_path_buf());
        }
    }

    /// 写入 0 字节标记文件（父目录自动创建）。
    fn create_marker(&self, kind: &str, rel: &str) -> Result<()> {
        validate_relative_path(rel)?;
        let path = self.marker_path(kind, rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::File::create(&path)?;
        Ok(())
    }

    /// 删除标记文件（不存在 = 幂等成功），并收敛空父目录。
    fn remove_marker(&self, kind: &str, rel: &str) -> Result<()> {
        validate_relative_path(rel)?;
        let path = self.marker_path(kind, rel);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            // 幂等：标记本来就不存在。
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        self.prune_empty_parents(kind, rel);
        Ok(())
    }

    /// 删除 lower 文件 → whiteout 标记（O(1)，零 SQLite 事务）。
    pub fn create_whiteout(&self, rel: &str) -> Result<()> {
        self.create_marker(WHITEOUTS, rel)
    }

    /// 撤销 whiteout（lower 文件重建/写回时）。
    pub fn remove_whiteout(&self, rel: &str) -> Result<()> {
        self.remove_marker(WHITEOUTS, rel)
    }

    /// 删除 lower 目录 → opaque 标记（隐藏整棵 lower 子树）。
    pub fn create_opaque(&self, rel: &str) -> Result<()> {
        self.create_marker(OPAQUE, rel)
    }

    /// 撤销 opaque 标记。
    pub fn remove_opaque(&self, rel: &str) -> Result<()> {
        self.remove_marker(OPAQUE, rel)
    }

    /// 目录改名映射：redirect 标记路径 = 改名后的视图路径，内容 = lower
    /// 命名空间的原始路径（v3 `renames` 行的持久化形态）。
    pub fn create_redirect(&self, view_to: &str, lower_from: &str) -> Result<()> {
        validate_relative_path(view_to)?;
        validate_relative_path(lower_from)?;
        let path = self.marker_path(REDIRECT, view_to);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, lower_from.as_bytes())?;
        Ok(())
    }

    /// 删除 redirect 标记（目录被删除/映射迁移时）。
    pub fn remove_redirect(&self, view_to: &str) -> Result<()> {
        self.remove_marker(REDIRECT, view_to)
    }

    /// 一次性装载整棵标记树（Overlay 打开时调用；之后热路径零标记 I/O）。
    /// 树不存在（全新 worktree / 已注销）→ 空状态。
    pub fn load(&self) -> Result<MarkerState> {
        let mut whites = HashSet::new();
        let mut opaques = HashSet::new();
        let mut redirects = Vec::new();
        walk_marker_files(&self.root.join(WHITEOUTS), "", &mut whites)?;
        walk_marker_files(&self.root.join(OPAQUE), "", &mut opaques)?;
        walk_redirect_markers(&self.root.join(REDIRECT), "", &mut redirects)?;
        Ok(MarkerState { whites, opaques, redirects })
    }

    /// 整树删除（注销 worktree）：单个 remove_dir_all，不再逐行 DELETE。
    /// 树不存在 = 幂等成功。
    pub fn wipe(&self) -> Result<()> {
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// 递归收集 `dir` 下全部标记文件的相对路径（`prefix` 为已积累的父路径）。
/// 目录不存在 = 空集。全树只在此打开时装载走一遍。
fn walk_marker_files(dir: &Path, prefix: &str, out: &mut HashSet<String>) -> Result<()> {
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in read {
        let entry = entry.map_err(Error::from)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(Error::Overlay(format!(
                "non-UTF-8 marker name under {}: {name:?}",
                dir.display()
            )));
        };
        let rel = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        if entry.file_type().map_err(Error::from)?.is_dir() {
            walk_marker_files(&entry.path(), &rel, out)?;
        } else {
            out.insert(rel);
        }
    }
    Ok(())
}

/// 递归装载 redirect 标记：路径 = 视图路径（to），文件内容 = lower 原路径
/// （from）。内容必须是合法相对路径（迁移与运行期写入方都保证）。
fn walk_redirect_markers(dir: &Path, prefix: &str, out: &mut Vec<(String, String)>) -> Result<()> {
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in read {
        let entry = entry.map_err(Error::from)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(Error::Overlay(format!(
                "non-UTF-8 redirect marker name under {}: {name:?}",
                dir.display()
            )));
        };
        let rel = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        if entry.file_type().map_err(Error::from)?.is_dir() {
            walk_redirect_markers(&entry.path(), &rel, out)?;
        } else {
            let from = std::fs::read_to_string(entry.path())?;
            if let Err(e) = validate_relative_path(from.trim()) {
                return Err(Error::Overlay(format!(
                    "invalid redirect marker target {rel:?}: {e}"
                )));
            }
            out.push((from.trim().to_string(), rel));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store(dir: &Path) -> (MarkerStore, PathBuf) {
        let upper = dir.join("wt-1").join("upper");
        std::fs::create_dir_all(&upper).unwrap();
        let root = marker_root_for_upper(&upper);
        (MarkerStore::new(root.clone()), root)
    }

    #[test]
    fn marker_root_derives_from_upper_parent() {
        let dir = TempDir::new().unwrap();
        let upper = dir.path().join("worktrees").join("wt-3").join("upper");
        assert_eq!(
            marker_root_for_upper(&upper),
            dir.path().join("worktrees").join("wt-3").join("meta")
        );
    }

    #[test]
    fn whiteout_roundtrip_is_idempotent_and_prunes_empty_dirs() {
        let dir = TempDir::new().unwrap();
        let (store, root) = store(dir.path());
        store.create_whiteout("a/b/c.txt").unwrap();
        let marker = root.join("whiteouts").join("a").join("b").join("c.txt");
        assert!(marker.is_file());
        // 重复创建幂等（0 字节覆盖）。
        store.create_whiteout("a/b/c.txt").unwrap();
        store.remove_whiteout("a/b/c.txt").unwrap();
        assert!(!marker.exists());
        // 空父目录收敛，不留 a/b 链。
        assert!(!root.join("whiteouts").join("a").exists());
        // 二次删除幂等。
        store.remove_whiteout("a/b/c.txt").unwrap();
    }

    #[test]
    fn opaque_and_redirect_roundtrip() {
        let dir = TempDir::new().unwrap();
        let (store, root) = store(dir.path());
        store.create_opaque("gone-dir").unwrap();
        assert!(root.join("opaque").join("gone-dir").is_file());
        store.remove_opaque("gone-dir").unwrap();
        assert!(!root.join("opaque").join("gone-dir").exists());

        store.create_redirect("z", "a").unwrap();
        let marker = root.join("redirect").join("z");
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "a");
        store.remove_redirect("z").unwrap();
        assert!(!marker.exists());
    }

    #[test]
    fn load_walks_tree_and_parses_redirect_content() {
        let dir = TempDir::new().unwrap();
        let (store, _) = store(dir.path());
        assert_eq!(store.load().unwrap(), MarkerState::default(), "无树 = 空状态");

        store.create_whiteout("b.txt").unwrap();
        store.create_whiteout("deep/nested/f.txt").unwrap();
        store.create_opaque("dir").unwrap();
        store.create_redirect("z", "a").unwrap();
        store.create_redirect("w/sub", "a/sub").unwrap();

        let state = store.load().unwrap();
        assert!(state.whites.contains("b.txt"));
        assert!(state.whites.contains("deep/nested/f.txt"));
        assert_eq!(state.opaques, ["dir".to_string()].into_iter().collect());
        let mut redirects = state.redirects.clone();
        redirects.sort();
        assert_eq!(
            redirects,
            vec![
                ("a".to_string(), "z".to_string()),
                ("a/sub".to_string(), "w/sub".to_string()),
            ]
        );
        assert!(state.blockers().contains("b.txt"));
        assert!(state.blockers().contains("dir"));
    }

    #[test]
    fn wipe_removes_tree_idempotently() {
        let dir = TempDir::new().unwrap();
        let (store, root) = store(dir.path());
        store.create_whiteout("x.txt").unwrap();
        store.wipe().unwrap();
        assert!(!root.exists());
        store.wipe().unwrap();
    }

    #[test]
    fn rejects_invalid_paths() {
        let dir = TempDir::new().unwrap();
        let (store, _) = store(dir.path());
        assert!(store.create_whiteout("../bad").is_err());
        assert!(store.remove_whiteout("../bad").is_err());
        assert!(store.create_opaque("a//b").is_err());
        assert!(store.create_redirect("z", "/abs").is_err());
        assert!(store.remove_redirect("z").is_ok(), "不存在的 redirect 幂等");
    }
}
