//! 写入路径 dedupe：内容哈希与三态决策（spec §6.3/§8.3）。
//!
//! 决策口径（Lower/Upper/New）：
//!
//! * [`DedupeState::Lower`] —— 内容与模板文件完全一致：丢弃刚写入的副本，视图回落 lower；
//! * [`DedupeState::Upper`] —— 模板存在该路径但内容不同：写入留在 upper；
//! * [`DedupeState::New`] —— 模板中没有该路径：作为新文件写入 upper。
//!
//! 模板整体缺失（spec §6.3 兜底：全量 copy）时，所有写入都是 [`DedupeState::New`]。
//! [`DedupeState`] 是 M0 任务口径的细粒度三态；骨架契约 [`DedupeDecision`]
//! 语义不变，两者经 `From` 互通。

use std::io::Read;
use std::path::Path;

use crate::{DedupeDecision, Error, FileHash, Result, TemplateFile};

/// blake3 流式哈希一块的大小。
const CHUNK: usize = 64 * 1024;

/// 单文件 dedupe 三态（写入引擎与 `dedupe` 表共用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupeState {
    Lower,
    Upper,
    New,
}

impl DedupeState {
    /// SQLite `dedupe.state` 列的存储字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            DedupeState::Lower => "lower",
            DedupeState::Upper => "upper",
            DedupeState::New => "new",
        }
    }

    /// 解析 `dedupe.state` 列；未知值返回 `None`。
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "lower" => Some(DedupeState::Lower),
            "upper" => Some(DedupeState::Upper),
            "new" => Some(DedupeState::New),
            _ => None,
        }
    }

    /// `true` = 内容需要留在 upper（Lower 之外都是）。
    pub fn writes_upper(self) -> bool {
        !matches!(self, DedupeState::Lower)
    }
}

/// 骨架契约 [`DedupeDecision`] 的语义不变：Lower → ToLower，其余 → ToUpper。
impl From<DedupeState> for DedupeDecision {
    fn from(state: DedupeState) -> Self {
        match state {
            DedupeState::Lower => DedupeDecision::ToLower,
            DedupeState::Upper | DedupeState::New => DedupeDecision::ToUpper,
        }
    }
}

/// 三态决策：模板条目存在且哈希一致 → Lower；存在但不同 → Upper；缺失 → New。
pub fn decide_for_path(template_file: Option<&TemplateFile>, content: FileHash) -> DedupeState {
    match template_file {
        Some(t) if t.hash == content => DedupeState::Lower,
        Some(_) => DedupeState::Upper,
        None => DedupeState::New,
    }
}

/// 内存内容的 blake3。
pub fn hash_bytes(data: &[u8]) -> FileHash {
    FileHash::of_bytes(data)
}

/// 流式哈希一个 reader，返回 (哈希, 字节数)；大文件不整载内存。
pub fn hash_reader(reader: &mut impl Read) -> std::io::Result<(FileHash, u64)> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; CHUNK];
    let mut size = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((FileHash(*hasher.finalize().as_bytes()), size))
}

/// 流式哈希一个磁盘文件，返回 (哈希, 字节数)。
pub fn hash_file(path: impl AsRef<Path>) -> Result<(FileHash, u64)> {
    let mut file = std::fs::File::open(path.as_ref())?;
    hash_reader(&mut file).map_err(Error::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template_of(content: &[u8]) -> TemplateFile {
        TemplateFile {
            path: "f.txt".into(),
            hash: hash_bytes(content),
            size: content.len() as u64,
            mode: 0o100644,
        }
    }

    #[test]
    fn decide_lower_upper_new_matrix() {
        let t = template_of(b"template");
        assert_eq!(decide_for_path(Some(&t), hash_bytes(b"template")), DedupeState::Lower);
        assert!(!decide_for_path(Some(&t), hash_bytes(b"template")).writes_upper());
        assert_eq!(decide_for_path(Some(&t), hash_bytes(b"changed")), DedupeState::Upper);
        assert_eq!(decide_for_path(Some(&t), hash_bytes(b"")), DedupeState::Upper);
        // 模板缺该路径 → New；模板整体缺失（None）→ 全量 copy 兜底，一律 New。
        assert_eq!(decide_for_path(None, hash_bytes(b"template")), DedupeState::New);
        assert_eq!(decide_for_path(None, hash_bytes(b"other")), DedupeState::New);
        assert!(DedupeState::New.writes_upper());
    }

    #[test]
    fn dedupe_state_maps_to_contract_decision_without_changing_it() {
        assert_eq!(DedupeDecision::from(DedupeState::Lower), DedupeDecision::ToLower);
        assert_eq!(DedupeDecision::from(DedupeState::Upper), DedupeDecision::ToUpper);
        assert_eq!(DedupeDecision::from(DedupeState::New), DedupeDecision::ToUpper);
        assert_eq!(DedupeState::from_str_opt("lower"), Some(DedupeState::Lower));
        assert_eq!(DedupeState::from_str_opt("new"), Some(DedupeState::New));
        assert_eq!(DedupeState::from_str_opt("nope"), None);
        assert_eq!(DedupeState::Upper.as_str(), "upper");
    }

    #[test]
    fn hash_reader_streams_and_counts_size() {
        // 300 KiB 内容跨多个 64 KiB 块，与一次性哈希一致。
        let data: Vec<u8> = (0..300 * 1024u32).map(|i| (i % 251) as u8).collect();
        let (hash, size) = hash_reader(&mut data.as_slice()).unwrap();
        assert_eq!(size, data.len() as u64);
        assert_eq!(hash, hash_bytes(&data));
        let (empty_hash, empty_size) = hash_reader(&mut std::io::empty()).unwrap();
        assert_eq!(empty_size, 0);
        assert_eq!(empty_hash, hash_bytes(b""));
    }

    #[test]
    fn hash_file_reads_disk_content() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.bin");
        std::fs::write(&p, b"disk-bytes").unwrap();
        let (hash, size) = hash_file(&p).unwrap();
        assert_eq!(size, 10);
        assert_eq!(hash, hash_bytes(b"disk-bytes"));
        assert!(hash_file(dir.path().join("missing")).is_err());
    }
}
