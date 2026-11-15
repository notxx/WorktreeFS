//! hydration 缓存：manifest blob 落盘的本地形态（`<store>/cas-cache`）。
//!
//! 布局：`<store>/cas-cache/<oid 前 2 位>/<oid>`（256 个分片目录，单目录
//! 条目数 = 文件数/256，readdir/scandir 均摊 O(1)）。
//!
//! * [`HydrationCache::ensure`]：分片文件已存在 → 命中（仅探测
//!   `symlink_metadata`，不读内容）；大小与 manifest 不符视为缓存损坏 →
//!   删除重灌。未命中 → `git cat-file --batch` 读 blob → 同分片内
//!   `.tmp-<pid>-<seq>` 写入 → `sync_all` → 原子 rename——可见路径零半截
//!   文件；并发同 oid 各写各的临时文件，rename 后内容一致（blob 不可变）。
//! * 统计（[`CacheStatsSnapshot`]）：命中/未命中/灌入文件数与字节/逐出
//!   ——"status 干净零 hydrate" 与缓存收益的断言依据。
//! * GC（[`HydrationCache::gc`]）：按 mtime 最旧优先逐出至预算内；顺带
//!   清扫 `.tmp-*` 崩溃残留。MVP 无逐出后台任务（显式调用），LRU 精细化
//!   （atime/访问计数）留 M18。
//!
//! 约定：缓存文件是 blob 的忠实拷贝（只读使用）；删缓存不丢数据（可重灌）。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::cat_file::CatFile;
use crate::{Error, Result};

/// hydration 缓存（`<store>/cas-cache` 分片树 + 统计）。
#[derive(Debug)]
pub struct HydrationCache {
    root: PathBuf,
    hits: AtomicU64,
    misses: AtomicU64,
    hydrated_files: AtomicU64,
    hydrated_bytes: AtomicU64,
    evicted_files: AtomicU64,
    evicted_bytes: AtomicU64,
}

static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 统计瞬时快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheStatsSnapshot {
    pub hits: u64,
    pub misses: u64,
    pub hydrated_files: u64,
    pub hydrated_bytes: u64,
    pub evicted_files: u64,
    pub evicted_bytes: u64,
}

/// GC 结果（日志与测试断言用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GcReport {
    pub removed_files: u64,
    pub removed_bytes: u64,
    pub removed_tmp: u64,
}

impl HydrationCache {
    /// 以存储根构造（缓存树 = `<store_root>/cas-cache`，懒建目录）。
    pub fn open(store_root: impl Into<PathBuf>) -> Self {
        Self {
            root: store_root.into().join("cas-cache"),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            hydrated_files: AtomicU64::new(0),
            hydrated_bytes: AtomicU64::new(0),
            evicted_files: AtomicU64::new(0),
            evicted_bytes: AtomicU64::new(0),
        }
    }

    /// 缓存树根。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// oid → 缓存内最终路径（`<root>/<aa>/<oid>`）。
    pub fn blob_path(&self, oid: &str) -> PathBuf {
        self.root.join(&oid[..2]).join(oid)
    }

    pub fn stats(&self) -> CacheStatsSnapshot {
        CacheStatsSnapshot {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            hydrated_files: self.hydrated_files.load(Ordering::Relaxed),
            hydrated_bytes: self.hydrated_bytes.load(Ordering::Relaxed),
            evicted_files: self.evicted_files.load(Ordering::Relaxed),
            evicted_bytes: self.evicted_bytes.load(Ordering::Relaxed),
        }
    }

    /// 返回 oid blob 的本地缓存路径，未命中则经 `cat` 灌入。
    /// `expected_size` 来自 manifest：命中文件大小不符 → 删除重灌（缓存
    /// 损坏自愈）；灌入后大小不符 → 报错并清理临时文件（对象库异常信号）。
    pub fn ensure(&self, cat: &CatFile, oid: &str, expected_size: u64) -> Result<PathBuf> {
        let final_path = self.blob_path(oid);
        if let Ok(md) = std::fs::symlink_metadata(&final_path) {
            if md.is_file() && md.len() == expected_size {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Ok(final_path);
            }
            // 大小不符 = 损坏（blob 不可变），删掉重灌。
            let _ = std::fs::remove_file(&final_path);
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        self.hydrate_inner(cat, oid, expected_size, &final_path)?;
        Ok(final_path)
    }

    /// 强制灌入（不管是否已缓存）；命中方不要调用（用 [`Self::ensure`]）。
    pub fn hydrate(&self, cat: &CatFile, oid: &str, expected_size: u64) -> Result<PathBuf> {
        let final_path = self.blob_path(oid);
        self.misses.fetch_add(1, Ordering::Relaxed);
        self.hydrate_inner(cat, oid, expected_size, &final_path)?;
        Ok(final_path)
    }

    fn hydrate_inner(
        &self,
        cat: &CatFile,
        oid: &str,
        expected_size: u64,
        final_path: &Path,
    ) -> Result<()> {
        let bytes = cat.read_blob(oid)?;
        if bytes.len() as u64 != expected_size {
            return Err(Error::Cas(format!(
                "object {oid}: blob size {} != manifest size {expected_size}",
                bytes.len()
            )));
        }
        let shard = final_path
            .parent()
            .ok_or_else(|| Error::Cas(format!("bad cache path {final_path:?}")))?;
        std::fs::create_dir_all(shard)?;
        let tmp = shard.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let staged = (|| -> std::io::Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
            Ok(())
        })();
        if let Err(e) = staged {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        // 并发同 oid：双方 rename 到同一目标，内容一致（blob 不可变），后者
        // 覆盖前者——原子性由同目录 rename 保证。
        if let Err(e) = std::fs::rename(&tmp, final_path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        self.hydrated_files.fetch_add(1, Ordering::Relaxed);
        self.hydrated_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    /// 缓存树当前字节量（遍历分片求和；GC/基准报告用）。
    pub fn cached_bytes(&self) -> u64 {
        let (total, _) = scan(&self.root);
        total
    }

    /// 逐出至 `max_bytes` 预算内：按 mtime 最旧优先删除正式缓存文件；
    /// 顺带清扫全部分片内的 `.tmp-*` 残留（含大小预算之外的清理）。
    /// MVP 同步执行、无后台任务。
    pub fn gc(&self, max_bytes: u64) -> Result<GcReport> {
        let mut removed_tmp = 0u64;
        for shard in shard_dirs(&self.root) {
            for entry in std::fs::read_dir(&shard)?.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with(".tmp-") {
                    let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    if std::fs::remove_file(entry.path()).is_ok() {
                        removed_tmp += 1;
                        self.evicted_files.fetch_add(1, Ordering::Relaxed);
                        self.evicted_bytes.fetch_add(bytes, Ordering::Relaxed);
                    }
                }
            }
        }
        let mut files = list_cache_files(&self.root)?;
        let mut total: u64 = files.iter().map(|f| f.size).sum();
        // mtime 升序（最旧优先）；mtime 并列时按路径序保证确定性。
        files.sort_by(|a, b| a.modified.cmp(&b.modified).then(a.path.cmp(&b.path)));
        let mut removed_files = 0u64;
        let mut removed_bytes = 0u64;
        for f in files {
            if total <= max_bytes {
                break;
            }
            if std::fs::remove_file(&f.path).is_ok() {
                total = total.saturating_sub(f.size);
                removed_files += 1;
                removed_bytes += f.size;
                self.evicted_files.fetch_add(1, Ordering::Relaxed);
                self.evicted_bytes.fetch_add(f.size, Ordering::Relaxed);
            }
        }
        Ok(GcReport {
            removed_files,
            removed_bytes,
            removed_tmp,
        })
    }

    /// 清空缓存树（测试/注销用；数据真相在对象库，删缓存不丢数据）。
    pub fn clear(&self) -> Result<()> {
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

struct CacheFile {
    path: PathBuf,
    size: u64,
    modified: std::time::SystemTime,
}

fn shard_dirs(root: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs
}

fn scan(root: &Path) -> (u64, usize) {
    let files = list_cache_files(root).unwrap_or_default();
    let bytes = files.iter().map(|f| f.size).sum();
    (bytes, files.len())
}

fn list_cache_files(root: &Path) -> Result<Vec<CacheFile>> {
    let mut out = Vec::new();
    for shard in shard_dirs(root) {
        for entry in std::fs::read_dir(&shard)?.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with(".tmp-") {
                continue;
            }
            let md = entry.metadata()?;
            if md.is_file() {
                out.push(CacheFile {
                    path: entry.path(),
                    size: md.len(),
                    modified: md.modified().unwrap_or(std::time::UNIX_EPOCH),
                });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(n: u8) -> String {
        format!("{n:02x}{}", "ab".repeat(19))
    }

    #[test]
    fn blob_path_shards_by_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let c = HydrationCache::open(dir.path());
        let p = c.blob_path(&oid(0xab));
        assert!(p.starts_with(dir.path().join("cas-cache").join("ab")));
        assert_eq!(p.file_name().unwrap().to_str().unwrap(), oid(0xab));
    }

    #[test]
    fn stats_roundtrip_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let c = HydrationCache::open(dir.path());
        let f = c.blob_path(&oid(1));
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, [0u8; 10]).unwrap();
        assert_eq!(c.cached_bytes(), 10);
        assert_eq!(c.stats().hydrated_files, 0, "直接落盘不计入灌入统计");
        c.clear().unwrap();
        assert_eq!(c.cached_bytes(), 0);
        c.clear().unwrap(); // 幂等
    }
}
