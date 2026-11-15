//! 删除标记（whiteout）：复用 M12/B120100 日志语义的独立实现。
//!
//! 与 wtfs-core `MarkerStore`（`crates/wtfs-core/src/overlay_markers.rs`）
//! 同一语义与同一记录格式——删除墓碑持久化为 append-only 日志，内存镜像
//! 是读路径真相，打开时重放装载，写路径同步追加：
//!
//! * 记录 = u32 LE 长度（覆盖 op+路径）+ u8 op（1=add，2=remove）+ UTF-8
//!   相对路径。单条记录单次 `write_all`；句柄常驻（懒打开），追加不 fsync
//!   ——崩溃时尾部撕裂记录在重放时忽略（长度越界/非法 op 即停），被删
//!   lower 内容可能重现（spec §8.3 允许，与 M12 一致）。
//! * 重放后条目数 > 1024 且 > 2× 存活数时 temp+rename 压实为只含存活
//!   add 的快照（同 M12 阈值）。
//! * whiteout 阻塞语义与 M12 同构：路径自身或任一祖先目录命中即隐藏
//!   （祖先命中时 upper 目录若已重建则按 opaque 语义只显示 upper 内容，
//!   见 [`crate::view`]）。
//!
//! 与 M12 的差异（有意裁剪）：无 redirect 标记（C MVP 不迁移 v3 rename
//! map，目录级 rename 默认 EXDEV）；无 initializing 缓冲（C 的 worktree
//! 没有全量 checkout 写入循环，删除即时持久即可，崩溃安全不减）。

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use parking_lot::Mutex;

use crate::{Error, Result, validate_relative_path};

/// op 字节（与 M12 同值）。
const OP_ADD: u8 = 1;
const OP_REMOVE: u8 = 2;

/// 压实阈值（与 M12 同值）。
const COMPACT_MIN_ENTRIES: usize = 1024;

/// 删除标记存储：append-only 日志 + 内存镜像。
#[derive(Debug)]
pub struct MarkerLog {
    log_path: PathBuf,
    /// 常驻追加句柄（懒打开；调用方串行化——CasView 的写路径经
    /// `&mut self` 方法访问镜像与日志，天然单写者）。
    log: Mutex<Option<std::fs::File>>,
    /// 存活 whiteout 集合（lower 命名空间）。
    whites: HashSet<String>,
    /// 重放时读过的总记录数（压实决策用）。
    replayed_entries: usize,
}

impl MarkerLog {
    /// 打开（或创建）`<meta_root>/markers.log` 并重放装载镜像。
    pub fn open(meta_root: impl Into<PathBuf>) -> Result<Self> {
        let log_path = meta_root.into().join("markers.log");
        let mut whites = HashSet::new();
        let mut replayed_entries = 0usize;
        let raw = match std::fs::read(&log_path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        let mut off = 0usize;
        while off + 5 <= raw.len() {
            let len =
                u32::from_le_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]]) as usize;
            // 守卫（镜像 wtfs-core overlay_markers）：长度必须覆盖 op 字节
            // （len < 1 = 零长度记录）且完整落在缓冲内，否则按撕裂/损坏尾
            // 处理——截断尾部并停止重放，绝不越界（payload[0] 前置条件）。
            if len < 1 || off + 4 + len > raw.len() {
                break;
            }
            let payload = &raw[off + 4..off + 4 + len];
            let (op, rel_bytes) = (payload[0], &payload[1..]);
            let Ok(rel) = std::str::from_utf8(rel_bytes) else {
                break;
            };
            match op {
                OP_ADD => {
                    whites.insert(rel.to_string());
                }
                OP_REMOVE => {
                    whites.remove(rel);
                }
                _ => break, // 非法 op：到此为止（后续视为损坏尾部）
            }
            replayed_entries += 1;
            off += 4 + len;
        }
        let mut log = Self {
            log_path,
            log: Mutex::new(None),
            whites,
            replayed_entries,
        };
        log.compact_if_needed()?;
        Ok(log)
    }

    /// 存活 whiteout 集合（只读镜像）。
    pub fn whites(&self) -> &HashSet<String> {
        &self.whites
    }

    /// 路径自身或任一祖先目录是否被 whiteout 阻塞（祖先命中 = opaque
    /// 语义：整棵 lower 子树隐藏）。
    pub fn blocks(&self, path: &str) -> bool {
        if path.is_empty() {
            return false;
        }
        let mut cur = path;
        loop {
            if self.whites.contains(cur) {
                return true;
            }
            match cur.rfind('/') {
                Some(idx) => cur = &cur[..idx],
                None => return false,
            }
        }
    }

    /// 仅祖先目录命中（不含路径自身）。
    pub fn ancestor_blocks(&self, path: &str) -> bool {
        let mut cur = path;
        while let Some(idx) = cur.rfind('/') {
            cur = &cur[..idx];
            if self.whites.contains(cur) {
                return true;
            }
        }
        false
    }

    /// 记 whiteout（镜像 + 追加记录；集合无变化时零 I/O）。
    pub fn add(&mut self, path: &str) -> Result<()> {
        validate_relative_path(path)?;
        if self.whites.insert(path.to_string()) {
            self.append(OP_ADD, path)?;
            self.replayed_entries += 1;
            self.compact_if_needed()?;
        }
        Ok(())
    }

    /// 撤销 whiteout（镜像 + 追加记录；无条目时零 I/O）。
    pub fn remove(&mut self, path: &str) -> Result<()> {
        validate_relative_path(path)?;
        if self.whites.remove(path) {
            self.append(OP_REMOVE, path)?;
            self.replayed_entries += 1;
            self.compact_if_needed()?;
        }
        Ok(())
    }

    fn append(&mut self, op: u8, rel: &str) -> Result<()> {
        let mut buf = Vec::with_capacity(5 + rel.len());
        let payload = (1 + rel.len()) as u32;
        buf.extend_from_slice(&payload.to_le_bytes());
        buf.push(op);
        buf.extend_from_slice(rel.as_bytes());
        let mut guard = self.log.lock();
        let handle = guard.get_or_insert_with(|| {
            if let Some(parent) = self.log_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.log_path)
                .expect("markers.log must open")
        });
        handle.write_all(&buf).map_err(|e| {
            Error::Io(std::io::Error::new(
                e.kind(),
                format!("append markers.log: {e}"),
            ))
        })
    }

    /// 条目数超阈值且远大于存活数 → temp+rename 压实（同 M12）。
    fn compact_if_needed(&mut self) -> Result<()> {
        if self.replayed_entries > COMPACT_MIN_ENTRIES
            && self.replayed_entries > 2 * self.whites.len()
        {
            let snapshot: Vec<u8> = self
                .whites
                .iter()
                .map(|rel| {
                    let mut buf = Vec::with_capacity(5 + rel.len());
                    let payload = (1 + rel.len()) as u32;
                    buf.extend_from_slice(&payload.to_le_bytes());
                    buf.push(OP_ADD);
                    buf.extend_from_slice(rel.as_bytes());
                    buf
                })
                .flatten()
                .collect();
            let tmp = self.log_path.with_extension("log.tmp");
            std::fs::write(&tmp, &snapshot)?;
            std::fs::rename(&tmp, &self.log_path)?;
            // 换文件后常驻句柄作废，下次 append 重开（追加到新文件）。
            *self.log.lock() = None;
            self.replayed_entries = self.whites.len();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_add_remove_and_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta").join("markers.log");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        let mut log = MarkerLog::open(dir.path().join("meta")).unwrap();
        log.add("a/b.txt").unwrap();
        log.add("dir").unwrap();
        log.remove("a/b.txt").unwrap();
        drop(log);

        // 追加一条撕裂尾记录（长度声明越界）
        let mut raw = std::fs::read(&path).unwrap();
        raw.extend_from_slice(&99u32.to_le_bytes());
        raw.extend_from_slice(b"broken");
        std::fs::write(&path, &raw).unwrap();

        let log = MarkerLog::open(dir.path().join("meta")).unwrap();
        assert!(log.whites().contains("dir"));
        assert!(!log.whites().contains("a/b.txt"));
        assert!(log.blocks("dir"));
        assert!(log.blocks("dir/child.txt"), "祖先目录命中隐藏子树");
        assert!(!log.blocks("a/b.txt"));
    }

    #[test]
    fn add_noop_when_present_and_remove_noop_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = MarkerLog::open(dir.path().join("meta")).unwrap();
        log.add("x").unwrap();
        let before = std::fs::metadata(dir.path().join("meta").join("markers.log"))
            .unwrap()
            .len();
        log.add("x").unwrap();
        log.remove("y").unwrap();
        let after = std::fs::metadata(dir.path().join("meta").join("markers.log"))
            .unwrap()
            .len();
        assert_eq!(before, after, "集合无变化时零追加");
    }

    /// P2-1 回归：四字节零长度记录出现在边界（后随垃圾字节使循环成立）
    /// 必须截断尾部、不 panic（镜像 wtfs-core 的 `len < 1` 守卫）。
    #[test]
    fn replay_tolerates_zero_length_record_at_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta").join("markers.log");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        let mut log = MarkerLog::open(dir.path().join("meta")).unwrap();
        log.add("keep.txt").unwrap();
        drop(log);

        // 追加：u32 长度 = 0 的记录 + 1 字节垃圾（凑足循环进入条件）
        let mut raw = std::fs::read(&path).unwrap();
        raw.extend_from_slice(&0u32.to_le_bytes());
        raw.extend_from_slice(&[0xFF]);
        // 再补一条 4 字节零长度记录收尾（循环入口即退出，同样不得 panic）
        raw.extend_from_slice(&0u32.to_le_bytes());
        std::fs::write(&path, &raw).unwrap();

        let log = MarkerLog::open(dir.path().join("meta")).unwrap();
        assert!(
            log.whites().contains("keep.txt"),
            "有效记录不受损坏尾部影响"
        );
        assert_eq!(log.whites().len(), 1);
    }
}
