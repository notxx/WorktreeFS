//! 常驻 `git cat-file --batch` 读取器：按 oid 取 blob 的 CAS 读路径。
//!
//! 设计（M17 定稿）：
//!
//! * 单个子进程常驻（`git cat-file --batch`，stdin/stdout 管道），仓库内
//!   对象读取零进程冷启动；[`parking_lot::Mutex`] 串行化"请求—响应"协议
//!   （写一行 `<oid>\n` → 读一行头 `<oid> <type> <size>\n` → 精确读 size
//!   字节 + 尾随 `\n`），并发调用天然排队。
//! * 头行 `<oid> missing` → [`Error::NotFound`]（对象库里没有该 blob，例如
//!   gitlink 160000 指向的 commit 不是 blob）。协议半途中断（EOF/短读/坏
//!   头）时把管道置空，下次调用重新拉起子进程，不把坏流状态留给后续请求。
//! * 统计（[`CatFileStats`]）：请求数、blob 数、字节量、missing 数——
//!   hydration "零读对象" 的断言依据（索引种子测试用它证明 route A 不触发
//!   任何对象读取）。
//!
//! 局限：MVP 仅 sha1 仓库（40 位 hex oid）；type 非 `blob` 报
//! [`Error::Cas`]；子进程崩溃/被杀由 Drop 兜底回收。

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use crate::gitx::validate_oid;
use crate::{Error, Result};

/// cat-file I/O 计数（原子量，读路径旁路采样，不构成热路径成本）。
#[derive(Debug, Default)]
pub struct CatFileStats {
    requests: AtomicU64,
    blobs: AtomicU64,
    missing: AtomicU64,
    bytes: AtomicU64,
}

/// [`CatFileStats`] 的瞬时快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CatFileStatsSnapshot {
    pub requests: u64,
    pub blobs: u64,
    pub missing: u64,
    pub bytes: u64,
}

/// 常驻批处理管道。
struct Pipe {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// 按 oid 读取 blob 的常驻读取器；`Clone` 产生独立管道实例但复制当前计数
/// （调用方通常只用一个实例；Clone 仅为 Arc 之外的便利）。
pub struct CatFile {
    repo: PathBuf,
    pipe: Mutex<Option<Pipe>>,
    stats: CatFileStats,
}

impl Drop for CatFile {
    fn drop(&mut self) {
        if let Some(mut pipe) = self.pipe.lock().take() {
            let _ = pipe.child.kill();
            let _ = pipe.child.wait();
        }
    }
}

/// 协议探测结果（exists 用）：对象存在（含类型与大小）或 missing。
pub enum Probe {
    Object { ty: String, size: u64 },
    Missing,
}

impl CatFile {
    /// 以仓库根（任意能访问对象库的目录，含裸仓库或 linked worktree）构造；
    /// 子进程在首次使用时拉起。
    pub fn new(repo: impl Into<PathBuf>) -> Self {
        Self {
            repo: repo.into(),
            pipe: Mutex::new(None),
            stats: CatFileStats::default(),
        }
    }

    pub fn stats(&self) -> CatFileStatsSnapshot {
        CatFileStatsSnapshot {
            requests: self.stats.requests.load(Ordering::Relaxed),
            blobs: self.stats.blobs.load(Ordering::Relaxed),
            missing: self.stats.missing.load(Ordering::Relaxed),
            bytes: self.stats.bytes.load(Ordering::Relaxed),
        }
    }

    /// 该 oid 的对象是否存在（头行探测 + 内容旁路丢弃；任何类型都算存在）。
    pub fn exists(&self, oid: &str) -> Result<bool> {
        validate_oid(oid)?;
        self.stats.requests.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.pipe_or_spawn()?;
        let pipe = guard.as_mut().expect("pipe_or_spawn returns Some");
        match probe_via(pipe, oid) {
            Ok(Probe::Object { size, .. }) => {
                self.stats.blobs.fetch_add(1, Ordering::Relaxed);
                self.stats.bytes.fetch_add(size, Ordering::Relaxed);
                Ok(true)
            }
            Ok(Probe::Missing) => {
                self.stats.missing.fetch_add(1, Ordering::Relaxed);
                Ok(false)
            }
            Err(e) => {
                *guard = None;
                Err(e)
            }
        }
    }

    /// 按 oid 读取完整 blob 内容（CAS 读路径；大文件一次性入内存——流式
    /// 接口在 M18 随 VFS 读回调再议）。对象 missing 或类型非 blob 报错。
    pub fn read_blob(&self, oid: &str) -> Result<Vec<u8>> {
        validate_oid(oid)?;
        self.stats.requests.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.pipe_or_spawn()?;
        let pipe = guard.as_mut().expect("pipe_or_spawn returns Some");
        match read_blob_via(pipe, oid) {
            Ok(bytes) => {
                self.stats.blobs.fetch_add(1, Ordering::Relaxed);
                self.stats
                    .bytes
                    .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                Ok(bytes)
            }
            Err(ReadBlobError::Missing) => {
                self.stats.missing.fetch_add(1, Ordering::Relaxed);
                Err(Error::NotFound(format!("object {oid} missing")))
            }
            Err(ReadBlobError::Protocol(e)) => {
                *guard = None;
                Err(e)
            }
            Err(ReadBlobError::NotBlob { ty, size }) => {
                self.stats.blobs.fetch_add(1, Ordering::Relaxed);
                self.stats.bytes.fetch_add(size, Ordering::Relaxed);
                Err(Error::Cas(format!(
                    "object {oid} is {ty:?}, not a blob ({size} bytes)"
                )))
            }
        }
    }

    fn pipe_or_spawn(&self) -> Result<MutexGuard<'_, Option<Pipe>>> {
        let mut guard = self.pipe.lock();
        if guard.is_none() {
            *guard = Some(spawn_pipe(&self.repo)?);
        }
        Ok(guard)
    }
}

use parking_lot::MutexGuard;

fn spawn_pipe(repo: &std::path::Path) -> Result<Pipe> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::Git(format!("spawn git cat-file --batch failed: {e}")))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| Error::Git("cat-file stdin unavailable".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Git("cat-file stdout unavailable".into()))?;
    Ok(Pipe {
        child,
        stdin,
        stdout: BufReader::with_capacity(128 * 1024, stdout),
    })
}

#[derive(Debug)]
enum ReadBlobError {
    /// 对象库里没有该 oid（协议完整，管道可继续复用）。
    Missing,
    /// 对象存在但类型非 blob（内容已旁路丢弃，管道可继续复用）。
    NotBlob { ty: String, size: u64 },
    /// 协议/IO 层失败（管道状态不明，调用方必须重置）。
    Protocol(Error),
}

impl From<std::io::Error> for ReadBlobError {
    fn from(e: std::io::Error) -> Self {
        ReadBlobError::Protocol(Error::Io(e))
    }
}

impl From<Error> for ReadBlobError {
    fn from(e: Error) -> Self {
        ReadBlobError::Protocol(e)
    }
}

/// 单次协议交互：发请求 → 头行 → 读体（blob 返回内容，其余按枚举）。
fn read_blob_via(pipe: &mut Pipe, oid: &str) -> std::result::Result<Vec<u8>, ReadBlobError> {
    let header = request_header(pipe, oid)?;
    let (ty, size) = match header {
        Header::Missing => return Err(ReadBlobError::Missing),
        Header::Object { ty, size } => (ty, size),
    };
    if ty != "blob" {
        drain(&mut pipe.stdout, size)?;
        return Err(ReadBlobError::NotBlob { ty, size });
    }
    let mut buf = Vec::with_capacity(size.min(64 * 1024 * 1024) as usize);
    (&mut pipe.stdout).take(size).read_to_end(&mut buf)?;
    if buf.len() as u64 != size {
        return Err(ReadBlobError::Protocol(Error::Cas(format!(
            "object {oid}: short read {}/{} bytes; pipe reset",
            buf.len(),
            size
        ))));
    }
    let mut lf = [0u8; 1];
    pipe.stdout.read_exact(&mut lf)?;
    if lf[0] != b'\n' {
        return Err(ReadBlobError::Protocol(Error::Cas(format!(
            "object {oid}: missing record terminator; pipe reset"
        ))));
    }
    Ok(buf)
}

/// exists 用的探测：头行 + 内容旁路丢弃。
fn probe_via(pipe: &mut Pipe, oid: &str) -> Result<Probe> {
    let header = request_header(pipe, oid)?;
    match header {
        Header::Missing => Ok(Probe::Missing),
        Header::Object { ty, size } => {
            drain(&mut pipe.stdout, size)?;
            Ok(Probe::Object { ty, size })
        }
    }
}

/// 发请求读头行；协议层失败向上传播（调用方重置管道）。
fn request_header(pipe: &mut Pipe, oid: &str) -> Result<Header> {
    pipe.stdin.write_all(oid.as_bytes())?;
    pipe.stdin.write_all(b"\n")?;
    pipe.stdin.flush()?;
    let mut line = Vec::new();
    pipe.stdout.read_until(b'\n', &mut line)?;
    if line.last() != Some(&b'\n') {
        return Err(Error::Cas(format!(
            "cat-file: incomplete header for {oid} (child died?)"
        )));
    }
    line.pop();
    parse_header(&line).ok_or_else(|| {
        Error::Cas(format!(
            "cat-file: unparseable header {:?} for {oid}",
            String::from_utf8_lossy(&line)
        ))
    })
}

enum Header {
    /// `<oid> <type> <size>`；type 为 blob/tree/commit/tag 之一。
    Object { ty: String, size: u64 },
    /// `<oid> missing`。
    Missing,
}

fn parse_header(line: &[u8]) -> Option<Header> {
    let text = std::str::from_utf8(line).ok()?;
    let mut parts = text.split(' ');
    let oid = parts.next()?;
    if oid.len() != 40 || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let second = parts.next()?;
    if second == "missing" {
        return Some(Header::Missing);
    }
    let size = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    Some(Header::Object {
        ty: second.to_string(),
        size: size.parse().ok()?,
    })
}

/// 旁路丢弃 `size` 字节 + 尾随记录分隔符（exists/非 blob 路径）。
fn drain(stdout: &mut BufReader<ChildStdout>, size: u64) -> Result<()> {
    let mut remaining = size;
    let mut buf = [0u8; 16 * 1024];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        stdout.read_exact(&mut buf[..want])?;
        remaining -= want as u64;
    }
    let mut lf = [0u8; 1];
    stdout.read_exact(&mut lf)?;
    Ok(())
}

impl std::fmt::Debug for CatFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CatFile").field("repo", &self.repo).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_parse() {
        let h = parse_header(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391 blob 0").unwrap();
        match h {
            Header::Object { ty, size } => {
                assert_eq!(ty, "blob");
                assert_eq!(size, 0);
            }
            Header::Missing => panic!("expected object"),
        }
        let h = parse_header(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391 missing").unwrap();
        assert!(matches!(h, Header::Missing));
        assert!(parse_header(b"garbage").is_none());
        assert!(parse_header(b"short blob 1 x").is_none());
    }
}
