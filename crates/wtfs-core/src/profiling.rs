//! env 门控的回调级 profiler（M13/B130100）：status/读路径归因用。
//!
//! `WTFS_VFS_PROFILE=1`（或 `true`）时启用：VFS 回调、overlay 逻辑入口与
//! NTFS 系统调用的次数/累计耗时/最大耗时，外加最慢调用 Top-32（带路径）。
//! 关闭时热路径只剩一次 `OnceLock` 读（一条原子 load），零计时、零分配、
//! 零锁——生产路径零开销（不满足"关闭即无插桩"的代码不允许合入）。
//!
//! 归因口径（基准 harness 取同一窗口的差值）：
//!
//! * git status 墙钟 − Σ回调墙钟 ≈ WinFSP IPC/内核派发 + git 自身 CPU；
//! * Σ回调墙钟 − ΣNTFS 系统调用墙钟 ≈ overlay 逻辑（锁/哈希/内存索引）；
//! * ΣNTFS 墙钟 = 元数据探测成本（次数 × 单次延迟，冷/热差异主要来源）。
//!
//! user/kernel CPU 用 [`process_cpu_times`]（GetProcessTimes）取 status
//! 前后进程级差值：kernel 增量 ≈ NTFS 内核侧开销，user 增量 ≈ handler 逻辑。
//!
//! 报告输出：[`report`]（文本）；`Daemon::shutdown` 在启用时把最终报告
//! best-effort 写到 `log_dir/vfs-profile.txt`。基准/测试直接调用
//! [`reset`] + [`report`] 分段归因。

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use parking_lot::Mutex;

/// 计量点。enum 顺序必须与 [`NAMES`] 一致（单元测试守护）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    // ---- VFS 回调（WinFSP user-mode handler 墙钟）----
    CbGetSecurityByName,
    CbOpen,
    CbCreate,
    CbOverwrite,
    CbGetFileInfo,
    CbGetSecurity,
    CbCleanup,
    CbClose,
    CbReadDirectory,
    CbRead,
    CbWrite,
    CbFlush,
    CbSetBasicInfo,
    CbSetDelete,
    CbSetFileSize,
    CbRename,
    // ---- overlay 逻辑入口 ----
    OvStat,
    OvResolve,
    OvList,
    OvListStat,
    /// VFS read_directory 逐子项 `overlay.stat`（优化目标观测点）。
    OvChildStat,
    /// `derive_ino`（blake3 路径哈希）调用。
    OvDeriveIno,
    /// `map_to_lower_with` 调用（观测 format!/to_string 分配频率）。
    OvMapToLower,
    // ---- NTFS 系统调用（std::fs 热路径）----
    NtSymlinkMetadata,
    /// `Path::is_dir` 形式的元数据探测。
    NtIsDirProbe,
    /// `fs::metadata` 形式的元数据探测。
    NtMetadata,
    NtReadDir,
    /// `DirEntry::metadata`（枚举后逐项补元数据）。
    NtDirEntryMeta,
    NtOpenFile,
    NtReadFile,
    NtSetModified,
    NtCreateDirAll,
    NtCopyFile,
    NtRename,
    NtRemoveFile,
}

impl Metric {
    /// 报告用名（分层前缀：cb.=回调 / ov.=overlay / nt.=NTFS 系统调用）。
    pub fn name(self) -> &'static str {
        NAMES[self as usize]
    }
}

static NAMES: [&str; 35] = [
    "cb.get_security_by_name",
    "cb.open",
    "cb.create",
    "cb.overwrite",
    "cb.get_file_info",
    "cb.get_security",
    "cb.cleanup",
    "cb.close",
    "cb.read_directory",
    "cb.read",
    "cb.write",
    "cb.flush",
    "cb.set_basic_info",
    "cb.set_delete",
    "cb.set_file_size",
    "cb.rename",
    "ov.stat",
    "ov.resolve",
    "ov.list",
    "ov.list_stat",
    "ov.child_stat",
    "ov.derive_ino",
    "ov.map_to_lower",
    "nt.symlink_metadata",
    "nt.is_dir_probe",
    "nt.metadata",
    "nt.read_dir",
    "nt.dir_entry_meta",
    "nt.open_file",
    "nt.read_file",
    "nt.set_modified",
    "nt.create_dir_all",
    "nt.copy_file",
    "nt.rename",
    "nt.remove_file",
];

#[derive(Debug)]
struct Counter {
    count: AtomicU64,
    total_ns: AtomicU64,
    max_ns: AtomicU64,
}

impl Counter {
    const fn new() -> Self {
        Self { count: AtomicU64::new(0), total_ns: AtomicU64::new(0), max_ns: AtomicU64::new(0) }
    }
}

static COUNTERS: [Counter; 35] = [
    Counter::new(), Counter::new(), Counter::new(), Counter::new(),
    Counter::new(), Counter::new(), Counter::new(), Counter::new(),
    Counter::new(), Counter::new(), Counter::new(), Counter::new(),
    Counter::new(), Counter::new(), Counter::new(), Counter::new(),
    Counter::new(), Counter::new(), Counter::new(), Counter::new(),
    Counter::new(), Counter::new(), Counter::new(), Counter::new(),
    Counter::new(), Counter::new(), Counter::new(), Counter::new(),
    Counter::new(), Counter::new(), Counter::new(), Counter::new(),
    Counter::new(), Counter::new(), Counter::new(),
];

/// 最慢调用记录阈值：超过才进 Top 列表（避免微秒级噪音淹没热点）。
const SLOW_NS: u64 = 200_000;
/// Top 慢调用列表长度。
const SLOW_KEEP: usize = 32;

#[derive(Debug, Clone)]
struct SlowCall {
    ns: u64,
    metric: &'static str,
    path: Option<String>,
}

static SLOW: OnceLock<Mutex<Vec<SlowCall>>> = OnceLock::new();

fn slow_slot() -> &'static Mutex<Vec<SlowCall>> {
    SLOW.get_or_init(|| Mutex::new(Vec::new()))
}

/// profiler 是否启用（`WTFS_VFS_PROFILE=1`/`true`；进程内首次访问读取 env）。
#[inline]
pub fn enabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        matches!(
            std::env::var("WTFS_VFS_PROFILE").as_deref(),
            Ok("1") | Ok("true") | Ok("TRUE") | Ok("on")
        )
    })
}

/// 记一次带耗时的计量（[`Span::done`] 的底层入口）。
pub fn record(metric: Metric, ns: u64) {
    let c = &COUNTERS[metric as usize];
    c.count.fetch_add(1, Ordering::Relaxed);
    c.total_ns.fetch_add(ns, Ordering::Relaxed);
    c.max_ns.fetch_max(ns, Ordering::Relaxed);
    if ns >= SLOW_NS {
        note_slow(metric, ns, None);
    }
}

/// 记一次带耗时与路径的计量（慢调用进 Top 列表时才物化路径字符串）。
pub fn record_with_path<F: FnOnce() -> String>(metric: Metric, ns: u64, path: F) {
    let c = &COUNTERS[metric as usize];
    c.count.fetch_add(1, Ordering::Relaxed);
    c.total_ns.fetch_add(ns, Ordering::Relaxed);
    c.max_ns.fetch_max(ns, Ordering::Relaxed);
    if ns >= SLOW_NS {
        note_slow(metric, ns, Some(path()));
    }
}

fn note_slow(metric: Metric, ns: u64, path: Option<String>) {
    let mut list = slow_slot().lock();
    let pos = list.partition_point(|c| c.ns >= ns);
    list.insert(pos, SlowCall { ns, metric: metric.name(), path });
    if list.len() > SLOW_KEEP {
        list.truncate(SLOW_KEEP);
    }
}

/// 计时 span：启用时起点取 Instant，[`Span::done`] 落账；禁用时全程零成本
/// （构造 = 一次原子 load，done = 一次分支）。
pub struct Span(Option<Instant>);

impl Span {
    #[inline]
    pub fn start() -> Self {
        if enabled() { Self(Some(Instant::now())) } else { Self(None) }
    }

    /// 结束并落账（禁用时为空操作）。
    #[inline]
    pub fn done(self, metric: Metric) {
        if let Some(t) = self.0 {
            record(metric, t.elapsed().as_nanos() as u64);
        }
    }

    /// 结束并落账；仅慢调用才调用 `path`（延迟物化路径字符串）。
    #[inline]
    pub fn done_with_path<F: FnOnce() -> String>(self, metric: Metric, path: F) {
        if let Some(t) = self.0 {
            record_with_path(metric, t.elapsed().as_nanos() as u64, path);
        }
    }
}

/// 全部计数与慢调用列表清零（基准分段归因用）。
pub fn reset() {
    for c in &COUNTERS {
        c.count.store(0, Ordering::Relaxed);
        c.total_ns.store(0, Ordering::Relaxed);
        c.max_ns.store(0, Ordering::Relaxed);
    }
    slow_slot().lock().clear();
}

/// 单个计量点的（count, total_ns）快照（基准 harness 归因求和用）。
pub fn metric_snapshot(metric: Metric) -> (u64, u64) {
    let c = &COUNTERS[metric as usize];
    (
        c.count.load(Ordering::Relaxed),
        c.total_ns.load(Ordering::Relaxed),
    )
}

/// 当前进程的 user/kernel CPU 累计毫秒（基准取窗口差值）；
/// 调用失败返回 (0, 0)。
pub fn process_cpu_times() -> (u64, u64) {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let ms = |f: FILETIME| {
        ((u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime)) / 10_000
    };
    unsafe {
        let (mut creation, mut exit, mut kernel, mut user) =
            (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        match GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        ) {
            Ok(()) => (ms(user), ms(kernel)),
            Err(_) => (0, 0),
        }
    }
}

/// 文本报告：计数/累计/均值/最大 + 最慢调用 Top-32。禁用时返回提示行。
pub fn report() -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{:<26} {:>9} {:>11} {:>9} {:>9}", "metric", "count", "total(ms)", "mean(us)", "max(us)");
    for (i, c) in COUNTERS.iter().enumerate() {
        let count = c.count.load(Ordering::Relaxed);
        if count == 0 {
            continue;
        }
        let total = c.total_ns.load(Ordering::Relaxed);
        let max = c.max_ns.load(Ordering::Relaxed);
        let _ = writeln!(
            out,
            "{:<26} {:>9} {:>11.1} {:>9.1} {:>9.1}",
            NAMES[i],
            count,
            total as f64 / 1e6,
            total as f64 / count as f64 / 1e3,
            max as f64 / 1e3,
        );
    }
    let slow = slow_slot().lock();
    if !slow.is_empty() {
        let _ = writeln!(out, "-- slowest calls (>= {} us) --", SLOW_NS / 1000);
        for c in slow.iter().take(SLOW_KEEP) {
            let _ = writeln!(
                out,
                "{:>8.2} ms {:<24} {}",
                c.ns as f64 / 1e6,
                c.metric,
                c.path.as_deref().unwrap_or("-"),
            );
        }
    }
    if out.is_empty() {
        out.push_str("(profiler disabled; set WTFS_VFS_PROFILE=1)\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_names_table_matches_enum() {
        assert_eq!(NAMES.len(), COUNTERS.len(), "NAMES 与 COUNTERS 等长");
        for m in [
            Metric::CbGetSecurityByName,
            Metric::CbOpen,
            Metric::CbGetFileInfo,
            Metric::CbReadDirectory,
            Metric::CbCleanup,
            Metric::CbClose,
            Metric::OvStat,
            Metric::OvChildStat,
            Metric::OvDeriveIno,
            Metric::NtSymlinkMetadata,
            Metric::NtIsDirProbe,
            Metric::NtMetadata,
            Metric::NtReadDir,
            Metric::NtRemoveFile,
        ] {
            assert!(!m.name().is_empty());
            assert!(NAMES.contains(&m.name()));
        }
    }

    #[test]
    fn record_and_reset_and_report() {
        reset();
        record(Metric::OvStat, 1_500);
        record_with_path(Metric::CbGetSecurityByName, 250_000, || "\\wt-1\\x".into());
        let text = report();
        assert!(text.contains("ov.stat"), "{text}");
        assert!(text.contains("cb.get_security_by_name"), "{text}");
        assert!(text.contains("slowest calls"), "{text}");
        assert!(text.contains("\\wt-1\\x"), "{text}");
        reset();
        assert!(!report().contains("ov.stat"), "reset 必须清零");
    }
}
