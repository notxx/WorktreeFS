//! overlay 删除标记（M12/B120100）：OverlayFS 式 in-parent whiteout，
//! 存放在 upper 层自身，替代 v3 及以前的 `whiteouts` 表。
//!
//! 布局（每个 worktree 一份）：
//!
//! ```text
//! <store>/worktrees/<id>/
//! ├─ upper/                     # 改动文件 + 删除标记（同一棵树，OverlayFS 同款）
//! │  ├─ .wh.<name>              # 删除标记：相对其父目录的文件或目录
//! │  └─ D/.wh.f                 # "删除 D/f" 的标记（f 为 lower 文件）
//! └─ meta/
//!    └─ redirect/<view-rel>     # 内容 = 原路径：v3 目录改名映射的迁移形态（只读）
//! ```
//!
//! * 删除路径 `P` 的标记 = `upper/parent(P)/.wh.<basename(P)>`（0 字节文件）。
//!   这与 OverlayFS 的 whiteout 布局一致：标记与其兄弟真实条目同层、以
//!   `.wh.` 前缀区分。**不会出现 file-vs-dir 冲突**：删除目录 `D` 的标记在
//!   `parent(D)/.wh.D`，而 `D` 子内容的标记在 `D/.wh.*` 内——remove_dir_all
//!   删除 `D` 时子标记随之消失，两者永不共存于同一路径
//!   （v3→v4 早期设计把全部标记镜像到独立树 `meta/whiteouts/<rel>` 下，
//!   目录与其子目录都被删除时 `meta/…/D` 既是标记文件又是父目录，触发
//!   ERROR_ACCESS_DENIED，已废弃）。
//! * 查找删除 = 标记路径存在性 O(1)；枚举某目录的删除项 = read_dir 对应
//!   upper 目录（`.wh.` 前缀过滤），绝不全树扫描。
//! * 写路径 O(1)：一次 `create_dir_all` + 一个空文件，零 SQLite 事务。
//! * 命名限制（与 OverlayFS 相同）：仓库内以 `.wh.` 开头的路径不落 upper
//!   标记命名空间——解析时此类路径跳过 upper（回落 lower/不存在），无法在
//!   upper 中物化。git/git 等真实仓库无此形态路径。
//! * opaque 语义（删除目录后 upper 重建、lower 子树继续隐藏）由同一标记
//!   机制给出：目录标记在 remove_dir_all 后落在 parent，重建只会显示 upper
//!   内容——与独立 opaque 标记等价，不再单设一棵树。
//! * 读取方（[`crate::overlay::Overlay`]）打开时把标记一次性装载进内存
//!   （[`MarkerState`]），热路径零标记 I/O；标记文件是持久真相。
//! * 注销 worktree：`meta/redirect` 树删除；upper 内标记随 upper 目录由
//!   `wtfs gc` 整体回收（spec §6.6），注销路径保持零逐项工作。同 id 重新
//!   注册时 [`MarkerStore::clear`] 清扫残留标记，防旧墓碑复活。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::{Error, Result, validate_relative_path};

/// 由 upper 物理根推导 redirect 标记树根：`<...>/upper` → `<...>/meta`。
/// 注册（`worktrees.upper_path`）与运行期使用同一推导，保证迁移/注销一致。
pub fn marker_root_for_upper(upper_root: &Path) -> PathBuf {
    upper_root
        .parent()
        .map(|p| p.join("meta"))
        .unwrap_or_else(|| PathBuf::from("meta"))
}

/// upper 内删除标记的文件名前缀（OverlayFS 同款约定）。
pub const WHITEOUT_PREFIX: &str = ".wh.";

/// 条目名是否为删除标记。
pub fn is_marker_name(name: &str) -> bool {
    name.starts_with(WHITEOUT_PREFIX)
}

/// upper 内某条目的标记文件路径：`upper/parent(rel)/.wh.<basename(rel)>`。
fn marker_path_for(upper_root: &Path, rel: &str) -> Result<PathBuf> {
    validate_relative_path(rel)?;
    let (parent, name) = match rel.rsplit_once('/') {
        Some((dir, name)) => (upper_root.join(dir), name),
        None => (upper_root.to_path_buf(), rel),
    };
    Ok(parent.join(format!("{WHITEOUT_PREFIX}{name}")))
}

/// 标记树的内存镜像（[`MarkerStore::load`] 一次性装载）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MarkerState {
    /// 删除标记路径（lower 命名空间；文件与目录删除共用，阻塞语义同构）。
    pub whites: HashSet<String>,
    /// 目录改名映射 (from → to)：redirect 标记在 `to`（视图路径），
    /// 内容为 `from`（lower 命名空间）。与 v3 `renames` 表的行序一致。
    pub redirects: Vec<(String, String)>,
}

impl MarkerState {
    /// 删除标记集合（保留 blockers 名称以兼容读路径：whiteout 与 opaque 对
    /// "隐藏 lower" 语义同构）。
    pub fn blockers(&self) -> &HashSet<String> {
        &self.whites
    }
}

/// 某个 worktree 的标记存储句柄（upper 内 whiteout + meta/redirect；无内部状态）。
#[derive(Debug, Clone)]
pub struct MarkerStore {
    upper_root: PathBuf,
    meta_root: PathBuf,
}

impl MarkerStore {
    /// 以 upper 物理根构造；redirect 树根按同一约定推导（upper 父目录/meta）。
    pub fn new(upper_root: PathBuf) -> Self {
        let meta_root = marker_root_for_upper(&upper_root);
        Self { upper_root, meta_root }
    }

    pub fn upper_root(&self) -> &Path {
        &self.upper_root
    }

    /// redirect 标记文件的绝对路径：`<meta>/redirect/<view_to>`。
    fn redirect_path(&self, view_to: &str) -> Result<PathBuf> {
        validate_relative_path(view_to)?;
        Ok(self.meta_root.join("redirect").join(view_to))
    }

    /// 删除 lower 文件/目录 → whiteout 标记（O(1)，零 SQLite 事务）。
    /// 文件与目录共用一种标记（阻塞语义同构，见模块文档）。
    pub fn create_whiteout(&self, rel: &str) -> Result<()> {
        let path = marker_path_for(&self.upper_root, rel)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::File::create(&path)?;
        Ok(())
    }

    /// 撤销 whiteout（重建/写回时；不存在 = 幂等成功），并收敛因标记而创建
    /// 的空父目录（只删空目录，绝不触碰真实 upper 内容）。
    pub fn remove_whiteout(&self, rel: &str) -> Result<()> {
        let path = marker_path_for(&self.upper_root, rel)?;
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        // 从标记文件父目录向上删除空目录；upper 根与含真实内容的目录自然止步。
        let mut cur = path.parent().map(|p| p.to_path_buf());
        while let Some(dir) = cur {
            if dir == self.upper_root {
                break;
            }
            let is_empty = std::fs::read_dir(&dir)
                .map(|mut rd| rd.next().is_none())
                .unwrap_or(false);
            if !is_empty || std::fs::remove_dir(&dir).is_err() {
                break;
            }
            cur = dir.parent().map(|p| p.to_path_buf());
        }
        Ok(())
    }

    /// 删除 lower 目录 → 删除标记（opaque 语义由"目录被删后重建仅显示
    /// upper 内容"给出，见模块文档）。
    pub fn create_opaque(&self, rel: &str) -> Result<()> {
        self.create_whiteout(rel)
    }

    /// 撤销目录删除标记。
    pub fn remove_opaque(&self, rel: &str) -> Result<()> {
        self.remove_whiteout(rel)
    }

    /// 目录改名映射：redirect 标记路径 = 改名后的视图路径，内容 = lower
    /// 命名空间的原始路径（v3 `renames` 行的持久化形态）。
    pub fn create_redirect(&self, view_to: &str, lower_from: &str) -> Result<()> {
        validate_relative_path(view_to)?;
        validate_relative_path(lower_from)?;
        let path = self.redirect_path(view_to)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, lower_from.as_bytes())?;
        Ok(())
    }

    /// 删除 redirect 标记。
    pub fn remove_redirect(&self, view_to: &str) -> Result<()> {
        validate_relative_path(view_to)?;
        let path = self.redirect_path(view_to)?;
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    /// 一次性装载全部标记（Overlay 打开时调用；之后热路径零标记 I/O）。
    /// 树不存在（全新 worktree / 已注销）→ 空状态。
    pub fn load(&self) -> Result<MarkerState> {
        let mut whites = HashSet::new();
        let mut redirects = Vec::new();
        walk_upper_markers(&self.upper_root, "", &mut whites)?;
        walk_redirect_markers(&self.meta_root.join("redirect"), "", &mut redirects)?;
        Ok(MarkerState { whites, redirects })
    }

    /// 把 upper 内标记同步为 `whites` 集合（mark_ready 时一次性物化缓冲的
    /// 删除标记）：补建缺失、删除多余；返回新建的标记数。空集 = 清空全部
    /// 标记 = 零创建。
    pub fn sync(&self, whites: &HashSet<String>) -> Result<usize> {
        let on_disk = {
            let mut whites_disk = HashSet::new();
            walk_upper_markers(&self.upper_root, "", &mut whites_disk)?;
            whites_disk
        };
        let mut created = 0usize;
        for rel in whites {
            if !on_disk.contains(rel) {
                self.create_whiteout(rel)?;
                created += 1;
            }
        }
        for rel in &on_disk {
            if !whites.contains(rel) {
                self.remove_whiteout(rel)?;
            }
        }
        Ok(created)
    }

    /// 清扫全部标记（同 id 重新注册时防旧墓碑复活）：upper 内 `.wh.*`
    /// （只删空目录，不触碰真实内容）+ meta/redirect 整树。
    pub fn clear(&self) -> Result<()> {
        sweep_upper_markers(&self.upper_root)?;
        match std::fs::remove_dir_all(&self.meta_root) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }
}

/// 递归收集 `dir`（upper 子树）内全部 `.wh.*` 标记对应的 overlay 相对路径。
fn walk_upper_markers(dir: &Path, prefix: &str, out: &mut HashSet<String>) -> Result<()> {
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
                "non-UTF-8 entry name under {}: {name:?}",
                dir.display()
            )));
        };
        if is_marker_name(name) {
            // 标记文件：还原 overlay 相对路径（parent + 去前缀的名字）。
            let rel = if prefix.is_empty() {
                name[WHITEOUT_PREFIX.len()..].to_string()
            } else {
                format!("{prefix}/{}", &name[WHITEOUT_PREFIX.len()..])
            };
            out.insert(rel);
            // 标记本身不是目录（`.wh.*` 目录属病态命名，与 OverlayFS 同限制）。
            continue;
        }
        if entry.file_type().map_err(Error::from)?.is_dir() {
            let rel = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}/{name}")
            };
            walk_upper_markers(&entry.path(), &rel, out)?;
        }
    }
    Ok(())
}

/// 递归清扫 upper 子树内的 `.wh.*` 标记文件（清空后顺手删除仅因标记存在的
/// 空目录；含真实内容的目录自然保留）。
fn sweep_upper_markers(dir: &Path) -> Result<()> {
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in read {
        let entry = entry.map_err(Error::from)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let path = entry.path();
        if is_marker_name(name) {
            if path.is_dir() {
                let _ = std::fs::remove_dir_all(&path);
            } else {
                let _ = std::fs::remove_file(&path);
            }
        } else if entry.file_type().map_err(Error::from)?.is_dir() {
            sweep_upper_markers(&path)?;
            // 子目录清空且本来不含真实内容 → 删除（best-effort）。
            let is_empty = std::fs::read_dir(&path)
                .map(|mut rd| rd.next().is_none())
                .unwrap_or(false);
            if is_empty {
                let _ = std::fs::remove_dir(&path);
            }
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
        let store = MarkerStore::new(upper.clone());
        (store, upper)
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
    fn whiteout_roundtrip_is_idempotent_and_prunes_created_dirs() {
        let dir = TempDir::new().unwrap();
        let (store, upper) = store(dir.path());
        store.create_whiteout("a/b/c.txt").unwrap();
        let marker = upper.join("a").join("b").join(".wh.c.txt");
        assert!(marker.is_file(), "标记必须落在父目录内");
        store.remove_whiteout("a/b/c.txt").unwrap();
        assert!(!marker.exists());
        assert!(!upper.join("a").exists(), "仅为标记创建的空目录应收敛");
        store.remove_whiteout("a/b/c.txt").unwrap();
        // 真实 upper 内容不受清理影响。
        std::fs::create_dir_all(upper.join("a").join("b")).unwrap();
        std::fs::write(upper.join("a").join("b").join("real.txt"), b"x").unwrap();
        store.create_whiteout("a/b/c.txt").unwrap();
        store.remove_whiteout("a/b/c.txt").unwrap();
        assert!(upper.join("a").join("b").join("real.txt").is_file());
    }

    #[test]
    fn dir_marker_lives_beside_dir_not_inside() {
        let dir = TempDir::new().unwrap();
        let (store, upper) = store(dir.path());
        // 删除目录 t：标记在 parent（upper 根），不与 t/ 内的子标记冲突。
        store.create_whiteout("t/x.txt").unwrap();
        assert!(upper.join("t").join(".wh.x.txt").is_file());
        store.create_whiteout("t").unwrap();
        assert!(upper.join(".wh.t").is_file(), "目录标记在 parent 层");
        let state = store.load().unwrap();
        assert!(state.whites.contains("t"));
        assert!(state.whites.contains("t/x.txt"));
        // remove_dir_all(upper/t)（真实删除流程）后子标记消失，t 的标记仍在。
        std::fs::remove_dir_all(upper.join("t")).unwrap();
        let state = store.load().unwrap();
        assert!(state.whites.contains("t"));
        assert!(!state.whites.contains("t/x.txt"), "子标记随目录删除消失");
    }

    #[test]
    fn redirect_roundtrip_and_load() {
        let dir = TempDir::new().unwrap();
        let (store, _) = store(dir.path());
        store.create_redirect("z", "a").unwrap();
        let upper = dir.path().join("wt-1").join("upper");
        let marker = marker_root_for_upper(&upper).join("redirect").join("z");
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "a");
        let state = store.load().unwrap();
        assert_eq!(state.redirects, vec![("a".to_string(), "z".to_string())]);
        store.remove_redirect("z").unwrap();
        assert!(!marker.exists());
        store.remove_redirect("z").unwrap(); // 幂等
    }

    #[test]
    fn clear_sweeps_upper_markers_and_redirect_tree() {
        let dir = TempDir::new().unwrap();
        let (store, upper) = store(dir.path());
        store.create_whiteout("gone.txt").unwrap();
        store.create_whiteout("d/x.txt").unwrap();
        store.create_redirect("z", "a").unwrap();
        std::fs::write(upper.join("keep.txt"), b"real").unwrap();

        store.clear().unwrap();
        assert!(!upper.join(".wh.gone.txt").exists());
        assert!(!upper.join(".wh.d").exists(), "纯标记目录被清");
        assert!(upper.join("keep.txt").is_file(), "真实内容保留");
        assert!(store.load().unwrap().whites.is_empty());
        assert!(store.load().unwrap().redirects.is_empty());
    }

    #[test]
    fn rejects_invalid_paths() {
        let dir = TempDir::new().unwrap();
        let (store, _) = store(dir.path());
        assert!(store.create_whiteout("../bad").is_err());
        assert!(store.remove_whiteout("../bad").is_err());
        assert!(store.create_redirect("z", "/abs").is_err());
    }
}
