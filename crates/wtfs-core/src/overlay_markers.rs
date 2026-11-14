//! overlay 删除标记（M12/B120100）：append-only 标记日志 + 内存镜像，
//! 替代 v3 及以前的 `whiteouts` 表与逐文件 `.wh.` 物理标记。
//!
//! 布局（每个 worktree 一份）：
//!
//! ```text
//! <store>/worktrees/<id>/
//! ├─ upper/                     # 改动文件（数据真相，无标记文件）
//! └─ meta/
//!    ├─ markers.log             # append-only 删除标记日志（ready 态）
//!    └─ redirect/<view-rel>     # 内容 = 原路径：v3 目录改名映射的迁移形态（只读）
//! ```
//!
//! * **ready 态**：每次删除/撤销删除 = 一条追加记录（先更新内存镜像，再
//!   `write_all` 一条记录），句柄常驻（`Mutex<Option<File>>`），单次 append
//!   ≈ µs 级——对比逐文件 `.wh.` 物理创建/删除（~0.4ms/次且被 Defender 按
//!   访问扫描放大，M12 R1 实测 remove +9.9s 的根因）。
//! * **initializing 期**：删除标记只缓冲在内存（M8 模式）——git checkout 的
//!   unlink→重建循环每文件产生一对标记增删，缓冲后干净检出零标记写入；
//!   `mark_ready` 时把存活集合一次性写成快照（空集 = 零写入）再切状态。
//! * **记录格式**：u32 LE 长度（覆盖 op+路径）+ u8 op（1=add，2=remove）+
//!   UTF-8 相对路径。单条记录单次 `write_all`；追加不 fsync——崩溃时尾部
//!   撕裂记录被忽略（长度越界/非法 op 即停），被删 lower 内容可能重现
//!   （spec §8.3 允许，与 M11 一致）。
//! * **打开时重放/压实**：顺序重放构建内存集合；条目数 >1024 且 > 2× 存活数
//!   时 temp+rename 压实为只含存活 add 的快照。
//! * **内存镜像仍是真相**：[`crate::overlay::Overlay`] 的标记镜像（打开时
//!   装载、写路径同步更新）服务读热路径；日志是持久形态，重开时重放恢复。
//! * 注销 worktree：日志与 `meta/` 随 upper 目录由 gc 整体回收；同 id 重新
//!   注册时 [`MarkerStore::clear`] 清扫日志与 redirect 树。

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::{Error, Result, validate_relative_path};

/// 由 upper 物理根推导标记日志根：`<...>/upper` → `<...>/meta`。
/// 注册（`worktrees.upper_path`）与运行期使用同一推导，保证迁移/注销一致。
pub fn marker_root_for_upper(upper_root: &Path) -> PathBuf {
    upper_root
        .parent()
        .map(|p| p.join("meta"))
        .unwrap_or_else(|| PathBuf::from("meta"))
}

/// 标记日志的 op 字节。
const OP_ADD: u8 = 1;
const OP_REMOVE: u8 = 2;

/// 压实阈值：日志条目数超过该值且远大于存活数时压缩。
const COMPACT_MIN_ENTRIES: usize = 1024;

/// 标记日志的内存镜像（[`MarkerStore::load`] 一次性重放）。
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

/// 某个 worktree 的标记存储句柄（append 日志 + redirect 树）。
#[derive(Debug)]
pub struct MarkerStore {
    meta_root: PathBuf,
    /// 常驻追加句柄（懒打开；由 Overlay 的 meta 锁串行化调用）。
    log: Mutex<Option<std::fs::File>>,
}

impl MarkerStore {
    /// 以 upper 物理根构造；日志/redirect 根按同一约定推导（upper 父目录/meta）。
    pub fn new(upper_root: PathBuf) -> Self {
        let meta_root = marker_root_for_upper(&upper_root);
        Self {
            meta_root,
            log: Mutex::new(None),
        }
    }

    fn log_path(&self) -> PathBuf {
        self.meta_root.join("markers.log")
    }

    fn redirect_path(&self, view_to: &str) -> Result<PathBuf> {
        validate_relative_path(view_to)?;
        Ok(self.meta_root.join("redirect").join(view_to))
    }

    /// 追加一条记录（单次 write_all；句柄懒打开、常驻）。
    fn append_record(&self, op: u8, rel: &str) -> Result<()> {
        validate_relative_path(rel)?;
        let mut buf = Vec::with_capacity(5 + rel.len());
        let payload = (1 + rel.len()) as u32;
        buf.extend_from_slice(&payload.to_le_bytes());
        buf.push(op);
        buf.extend_from_slice(rel.as_bytes());
        let mut guard = self.log.lock().expect("markers.log lock poisoned");
        let handle = guard.get_or_insert_with(|| -> std::fs::File {
            let path = self.log_path();
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .expect("markers.log must open")
        });
        handle.write_all(&buf)?;
        Ok(())
    }

    /// 删除 lower 文件/目录 → 追加 add 记录（O(1)，零 SQLite 事务；文件与
    /// 目录共用一种标记，阻塞语义同构）。
    pub fn create_whiteout(&self, rel: &str) -> Result<()> {
        self.append_record(OP_ADD, rel)
    }

    /// 撤销 whiteout（重建/写回时）→ 追加 remove 记录。
    pub fn remove_whiteout(&self, rel: &str) -> Result<()> {
        self.append_record(OP_REMOVE, rel)
    }

    /// 删除 lower 目录 → 同一 add 标记（opaque 语义由"目录被删后重建仅显示
    /// upper 内容"给出，见模块文档）。
    pub fn create_opaque(&self, rel: &str) -> Result<()> {
        self.append_record(OP_ADD, rel)
    }

    /// 撤销目录删除标记。
    pub fn remove_opaque(&self, rel: &str) -> Result<()> {
        self.append_record(OP_REMOVE, rel)
    }

    /// 重放日志构建内存集合；返回（集合，有效条目数）。撕裂尾记录忽略。
    fn replay(&self) -> Result<(HashSet<String>, usize)> {
        let mut whites = HashSet::new();
        let data = match std::fs::read(self.log_path()) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((whites, 0)),
            Err(e) => return Err(e.into()),
        };
        let mut entries = 0usize;
        let mut pos = 0usize;
        while pos + 4 <= data.len() {
            let len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            if len < 1 || pos + 4 + len > data.len() {
                break; // 撕裂/损坏尾：忽略。
            }
            let op = data[pos + 4];
            let rel = match std::str::from_utf8(&data[pos + 5..pos + 4 + len]) {
                Ok(rel) => rel,
                Err(_) => break,
            };
            match op {
                OP_ADD => {
                    whites.insert(rel.to_string());
                    entries += 1;
                }
                OP_REMOVE => {
                    whites.remove(rel);
                    entries += 1;
                }
                _ => break, // 未知 op：视为损坏尾。
            }
            pos += 4 + len;
        }
        Ok((whites, entries))
    }

    /// 用只含存活 add 的快照压实日志（temp + rename；旧句柄先关闭）。
    pub fn write_snapshot(&self, whites: &HashSet<String>) -> Result<()> {
        let tmp = self.meta_root.join("markers.log.tmp");
        if let Some(parent) = tmp.parent() {
            std::fs::create_dir_all(parent)?;
        }
        {
            let file = std::fs::File::create(&tmp)?;
            let mut writer = BufWriter::new(file);
            for rel in whites {
                let payload = (1 + rel.len()) as u32;
                writer.write_all(&payload.to_le_bytes())?;
                writer.write_all(&[OP_ADD])?;
                writer.write_all(rel.as_bytes())?;
            }
            writer.flush()?;
        }
        // Windows 不允许 rename 覆盖打开中的文件：先丢弃常驻句柄。
        *self.log.lock().expect("markers.log lock poisoned") = None;
        std::fs::rename(&tmp, self.log_path())?;
        Ok(())
    }

    /// 一次性装载：重放日志（按需压实）+ redirect 树。树不存在 → 空状态。
    pub fn load(&self) -> Result<MarkerState> {
        let (whites, entries) = self.replay()?;
        // 压实：条目数远大于存活数时压缩为快照（打开时重放/压缩）。
        if entries > COMPACT_MIN_ENTRIES && entries > 2 * whites.len() {
            self.write_snapshot(&whites)?;
        }
        let mut redirects = Vec::new();
        walk_redirect_markers(&self.meta_root.join("redirect"), "", &mut redirects)?;
        Ok(MarkerState { whites, redirects })
    }

    /// 把日志同步为 `whites` 快照（mark_ready 时物化 initializing 缓冲的
    /// 存活集合）：返回写入的记录数。空集且无既有日志 = 零写入。
    pub fn sync(&self, whites: &HashSet<String>) -> Result<usize> {
        if whites.is_empty() && !self.log_path().exists() {
            return Ok(0);
        }
        self.write_snapshot(whites)?;
        Ok(whites.len())
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

    /// 清扫全部标记（同 id 重新注册时防旧墓碑复活）：日志 + meta/redirect
    /// 整树。
    pub fn clear(&self) -> Result<()> {
        *self.log.lock().expect("markers.log lock poisoned") = None;
        match std::fs::remove_dir_all(&self.meta_root) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }
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

    /// 重放顺序：add→remove→add 的净效果与 add 一致。
    #[test]
    fn replay_applies_add_remove_in_order() {
        let dir = TempDir::new().unwrap();
        let (store, _) = store(dir.path());
        store.create_whiteout("b.txt").unwrap();
        store.remove_whiteout("b.txt").unwrap();
        store.create_whiteout("a").unwrap();
        store.create_whiteout("deep/nested/f.txt").unwrap();
        store.remove_whiteout("deep/nested/f.txt").unwrap();

        let state = store.load().unwrap();
        assert_eq!(state.whites, ["a".to_string()].into_iter().collect());
        assert!(state.blockers().contains("a"));
    }

    /// 撕裂尾记录：截断最后一条 → 重放忽略尾部，保留前面的完整记录。
    #[test]
    fn torn_tail_is_ignored() {
        let dir = TempDir::new().unwrap();
        let (store, _) = store(dir.path());
        store.create_whiteout("good.txt").unwrap();
        store.create_whiteout("torn.txt").unwrap();
        let log = store.log_path();
        let mut data = std::fs::read(&log).unwrap();
        // 截掉最后 3 字节（撕裂 op/路径）。
        data.truncate(data.len() - 3);
        std::fs::write(&log, &data).unwrap();
        // 句柄缓存的是旧文件内容视图之外的偏移——重开实例模拟崩溃重开。
        let reopened = MarkerStore::new(
            dir.path().join("wt-1").join("upper"),
        );
        let state = reopened.load().unwrap();
        assert!(state.whites.contains("good.txt"));
        assert!(!state.whites.contains("torn.txt"), "撕裂尾必须被忽略");
    }

    /// 压实：条目数超阈值且远大于存活数 → load 压缩为存活快照。
    #[test]
    fn compaction_runs_when_entries_dominate() {
        let dir = TempDir::new().unwrap();
        let (store, _) = store(dir.path());
        // 3000 add + 3000 remove（同一批路径的增删震荡），存活仅 2 条。
        for i in 0..1500 {
            store.create_whiteout(&format!("churn{i}.txt")).unwrap();
            store.remove_whiteout(&format!("churn{i}.txt")).unwrap();
        }
        store.create_whiteout("keep1.txt").unwrap();
        store.create_whiteout("keep2.txt").unwrap();

        let state = store.load().unwrap();
        assert_eq!(state.whites.len(), 2);
        // 压实后日志只剩 2 条 add 记录。
        let log = std::fs::read(store.log_path()).unwrap();
        let records = log.len() / (4 + 1 + 9); // 粗查：总长收缩到很小
        assert!(records <= 3, "压实后记录数应极少");
        let state2 = store.load().unwrap();
        assert_eq!(state2.whites.len(), 2, "压实后重放结果不变");
    }

    /// 快照写读往返（mark_ready 物化路径）。
    #[test]
    fn snapshot_roundtrip() {
        let dir = TempDir::new().unwrap();
        let (store, _) = store(dir.path());
        store.create_whiteout("stale.txt").unwrap();
        let mut live = HashSet::new();
        live.insert("live1.txt".to_string());
        live.insert("d/live2.txt".to_string());
        let written = store.sync(&live).unwrap();
        assert_eq!(written, 2);
        let state = store.load().unwrap();
        assert_eq!(state.whites, live);
        // 幂等：同集合再 sync 不改变语义。
        store.sync(&live).unwrap();
        assert_eq!(store.load().unwrap().whites, live);
    }

    /// redirect 标记往返与装载。
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

    /// clear：日志 + redirect 整树删除；真实 upper 内容不受影响。
    #[test]
    fn clear_removes_log_and_redirect_tree() {
        let dir = TempDir::new().unwrap();
        let (store, upper) = store(dir.path());
        store.create_whiteout("gone.txt").unwrap();
        store.create_redirect("z", "a").unwrap();
        std::fs::write(upper.join("keep.txt"), b"real").unwrap();

        store.clear().unwrap();
        assert!(store.load().unwrap().whites.is_empty());
        assert!(store.load().unwrap().redirects.is_empty());
        assert!(upper.join("keep.txt").is_file(), "真实内容保留");
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
