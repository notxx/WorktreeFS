//! 回调级单元测试：直接驱动 [`WtfsContext`] 的 WinFSP 回调（不需要真实
//! 挂载）。真实 WinFSP 挂载 + `git worktree add` 的端到端测试见
//! `tests/mount_integration.rs`（默认 ignored）。

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Arc;

use tempfile::TempDir;
use winfsp::constants::FspCleanupFlags;
use winfsp::filesystem::{DirInfo, FileSecurity, FileInfo, FileSystemContext, VolumeInfo};
use winfsp::U16CString;
use wtfs_core::{
    Db, DedupeState, Overlay, OverlayConfig, OverlayStore, Resolution, SqliteOverlayStore,
    TemplateFile, WorktreeId,
};

use crate::context::{
    FileContext, WtfsContext, blank_open_file_info_for_test, marker_for_test, wildcard_match,
};
use crate::registry::{VfsConfig, VfsRegistry};
use crate::win::{
    FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME,
    FILE_ACTION_RENAMED_OLD_NAME, FILE_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF,
    FILE_OVERWRITE_IF, filetime_from_unix_ns,
};
use crate::security::fixed_security_descriptor_len;

const ONE: &[u8] = b"one\n";
const TWO: &[u8] = b"two\n";
const BEE: &[u8] = b"bee\n";
const RUN: &[u8] = b"run\n";

struct Fixture {
    _dir: TempDir,
    #[allow(dead_code)] // 信息字段：保留供调试/后续断言
    lower_dir: PathBuf,
    upper_dir: PathBuf,
    registry: Arc<VfsRegistry>,
    ctx: WtfsContext,
}

/// 带模板的 fixture：lower 树 + 内存清单 + 已注册（initializing）的 wt-1。
fn fixture() -> Fixture {
    // DirBuffer/notify 走 WinFSP DLL（delayload）；先按 PATH/注册表加载。
    crate::win::ensure_winfsp_runtime().expect("WinFSP runtime must be loadable");
    let dir = TempDir::new().unwrap();
    let lower_dir = dir.path().join("lower");
    let upper_dir = dir.path().join("upper-wt1");
    std::fs::create_dir_all(lower_dir.join("a")).unwrap();
    std::fs::create_dir_all(&upper_dir).unwrap();
    std::fs::write(lower_dir.join("a/1.txt"), ONE).unwrap();
    std::fs::write(lower_dir.join("a/2.txt"), TWO).unwrap();
    std::fs::write(lower_dir.join("b.txt"), BEE).unwrap();
    std::fs::write(lower_dir.join("exec.sh"), RUN).unwrap();

    let mut map: HashMap<String, TemplateFile> = HashMap::new();
    for (path, content, mode) in [
        ("a/1.txt", ONE, 0o100644u32),
        ("a/2.txt", TWO, 0o100644),
        ("b.txt", BEE, 0o100644),
        ("exec.sh", RUN, 0o100755),
    ] {
        map.insert(
            path.to_string(),
            TemplateFile {
                path: path.to_string(),
                hash: wtfs_core::hash_bytes(content),
                size: content.len() as u64,
                mode,
            },
        );
    }

    let cfg = VfsConfig::new(dir.path().join("repo"), dir.path().join("store"));
    let registry = Arc::new(VfsRegistry::open(cfg).unwrap());
    let mut config = OverlayConfig::new(WorktreeId(1), &upper_dir);
    config.lower_root = Some(lower_dir.clone());
    config.dedupe_enabled = true;
    let store = Arc::new(SqliteOverlayStore::new(Db::open_in_memory().unwrap()));
    let overlay = Arc::new(Overlay::new(config, Some(map), store).unwrap());
    registry.register("wt-1", overlay);

    let ctx = WtfsContext::new(registry.clone());
    Fixture {
        _dir: dir,
        lower_dir,
        upper_dir,
        registry,
        ctx,
    }
}

/// 带空文件的模板 fixture（B100100）：在 [`fixture`] 的文件之外增加根级
/// `empty.txt` 与嵌套 `nested/empty.txt`（模板含空 blob 条目，git checkout
/// 的零字节 blob 路径依赖它）。
fn fixture_with_empty() -> Fixture {
    // DirBuffer/notify 走 WinFSP DLL（delayload）；先按 PATH/注册表加载。
    crate::win::ensure_winfsp_runtime().expect("WinFSP runtime must be loadable");
    let dir = TempDir::new().unwrap();
    let lower_dir = dir.path().join("lower");
    let upper_dir = dir.path().join("upper-wt1");
    std::fs::create_dir_all(lower_dir.join("a")).unwrap();
    std::fs::create_dir_all(lower_dir.join("nested")).unwrap();
    std::fs::create_dir_all(&upper_dir).unwrap();
    std::fs::write(lower_dir.join("a/1.txt"), ONE).unwrap();
    std::fs::write(lower_dir.join("a/2.txt"), TWO).unwrap();
    std::fs::write(lower_dir.join("b.txt"), BEE).unwrap();
    std::fs::write(lower_dir.join("exec.sh"), RUN).unwrap();
    std::fs::write(lower_dir.join("empty.txt"), b"").unwrap();
    std::fs::write(lower_dir.join("nested/empty.txt"), b"").unwrap();

    let mut map: HashMap<String, TemplateFile> = HashMap::new();
    for (path, content, mode) in [
        ("a/1.txt", ONE, 0o100644u32),
        ("a/2.txt", TWO, 0o100644),
        ("b.txt", BEE, 0o100644),
        ("exec.sh", RUN, 0o100755),
        ("empty.txt", &b""[..], 0o100644),
        ("nested/empty.txt", &b""[..], 0o100644),
    ] {
        map.insert(
            path.to_string(),
            TemplateFile {
                path: path.to_string(),
                hash: wtfs_core::hash_bytes(content),
                size: content.len() as u64,
                mode,
            },
        );
    }

    let cfg = VfsConfig::new(dir.path().join("repo"), dir.path().join("store"));
    let registry = Arc::new(VfsRegistry::open(cfg).unwrap());
    let mut config = OverlayConfig::new(WorktreeId(1), &upper_dir);
    config.lower_root = Some(lower_dir.clone());
    config.dedupe_enabled = true;
    let store = Arc::new(SqliteOverlayStore::new(Db::open_in_memory().unwrap()));
    let overlay = Arc::new(Overlay::new(config, Some(map), store).unwrap());
    registry.register("wt-1", overlay);

    let ctx = WtfsContext::new(registry.clone());
    Fixture {
        _dir: dir,
        lower_dir,
        upper_dir,
        registry,
        ctx,
    }
}

fn p(s: &str) -> U16CString {
    U16CString::from_str(s).unwrap()
}

fn sec(fx: &Fixture, name: &str) -> winfsp::Result<FileSecurity> {
    fx.ctx.get_security_by_name(&p(name), None, |_| None)
}

fn open(fx: &Fixture, name: &str) -> winfsp::Result<FileContext> {
    open_as(fx, name, FILE_OPEN)
}

/// 以指定 disposition 打开（CreateOptions 高 8 位 = disposition，B060100）。
fn open_as(fx: &Fixture, name: &str, disposition: u32) -> winfsp::Result<FileContext> {
    fx.ctx
        .open(&p(name), disposition << 24, 0, &mut blank_open_file_info_for_test())
}

/// CREATE_ALWAYS 语义打开（FILE_OVERWRITE_IF）：真机 git/`std::fs::write` 路径。
fn open_overwrite(fx: &Fixture, name: &str) -> winfsp::Result<FileContext> {
    open_as(fx, name, FILE_OVERWRITE_IF)
}

fn create_file(fx: &Fixture, name: &str) -> FileContext {
    fx.ctx
        .create(
            &p(name),
            0,
            0,
            0,
            None,
            0,
            None,
            false,
            &mut blank_open_file_info_for_test(),
        )
        .unwrap()
}

fn create_dir(fx: &Fixture, name: &str) -> FileContext {
    fx.ctx
        .create(
            &p(name),
            FILE_DIRECTORY_FILE,
            0,
            0,
            None,
            0,
            None,
            false,
            &mut blank_open_file_info_for_test(),
        )
        .unwrap()
}

fn write(fx: &Fixture, fctx: &FileContext, data: &[u8]) -> u32 {
    let mut info = FileInfo::default();
    fx.ctx.write(fctx, data, 0, false, false, &mut info).unwrap()
}

fn file_info(fx: &Fixture, fctx: &FileContext) -> FileInfo {
    let mut info = FileInfo::default();
    fx.ctx.get_file_info(fctx, &mut info).unwrap();
    info
}

fn cleanup(fx: &Fixture, fctx: FileContext, flags: u32) {
    fx.ctx.cleanup(&fctx, None, flags);
}

fn read_via(fx: &Fixture, fctx: &FileContext, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    let n = fx.ctx.read(fctx, &mut buf, 0).unwrap() as usize;
    buf.truncate(n);
    buf
}

/// 解析 DirBuffer 读出的条目名（跳过 "." ".."）。
fn dir_names(fx: &Fixture, dir: &str) -> Vec<String> {
    let marker = unsafe { marker_for_test(None) };
    let mut buffer = vec![0u8; 64 * 1024];
    let fctx = open(fx, dir).unwrap();
    let n = fx
        .ctx
        .read_directory(&fctx, None, marker, &mut buffer)
        .unwrap() as usize;
    buffer.truncate(n);
    parse_entries(&buffer)
}

fn parse_entries(buffer: &[u8]) -> Vec<String> {
    let header = std::mem::size_of::<DirInfo<0>>();
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + header <= buffer.len() {
        let size = u16::from_le_bytes([buffer[pos], buffer[pos + 1]]) as usize;
        if size < header || pos + size > buffer.len() {
            break;
        }
        let name_bytes = &buffer[pos + header..pos + size];
        let units: Vec<u16> = name_bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        let name = String::from_utf16(&units).unwrap();
        if name != "." && name != ".." {
            out.push(name);
        }
        // WinFSP 目录缓冲按 8 字节对齐存放条目。
        let stride = (size + 7) & !7;
        if stride == 0 {
            break;
        }
        pos += stride;
    }
    out.sort();
    out
}

fn drain_events(fx: &Fixture) -> Vec<crate::NotifyEvent> {
    std::iter::from_fn(|| fx.ctx.notify_hub().pop()).collect()
}

#[test]
fn initializing_only_hides_enumeration_not_names() {
    let fx = fixture();

    // 按名查找走正常解析（git checkout 写后按路径 lstat 回填 index stat）：
    // 模板文件在 initializing 期同样可见
    let s = sec(&fx, "\\wt-1\\b.txt").unwrap();
    assert!(s.attributes & wtfs_core::FileAttributes::ARCHIVE.bits() != 0);
    assert!(open(&fx, "\\wt-1\\b.txt").is_ok());
    let root_sec = sec(&fx, "\\wt-1").unwrap();
    assert!(root_sec.attributes & wtfs_core::FileAttributes::DIRECTORY.bits() != 0);
    assert!(open(&fx, "\\wt-1").is_ok());

    // 挂载根可见；未注册名不可见
    assert!(sec(&fx, "\\").is_ok());
    assert!(open(&fx, "\\").is_ok());
    assert!(sec(&fx, "\\wt-99\\x").is_err());

    // 关键门控：worktree 根 readdir 只有空条目（git "目录必须为空"检查走枚举）
    let fctx = open(&fx, "\\wt-1").unwrap();
    let marker = unsafe { marker_for_test(None) };
    let mut buffer = vec![0u8; 4096];
    let n = fx
        .ctx
        .read_directory(&fctx, None, marker, &mut buffer)
        .unwrap() as usize;
    buffer.truncate(n);
    assert!(parse_entries(&buffer).is_empty(), "initializing 时不得枚举出条目");
}

#[test]
fn staged_write_dedupe_discards_template_identical_content() {
    let fx = fixture();
    let overlay = fx.registry.get("wt-1").unwrap();

    // git checkout 流：create + 全量写 + fstat（投影）+ close（终裁）
    let fctx = create_file(&fx, "\\wt-1\\b.txt");
    assert_eq!(write(&fx, &fctx, BEE), BEE.len() as u32);

    // 暂存期投影：内容与模板一致 → 固定 stat（index 对齐）
    let info = file_info(&fx, &fctx);
    assert_eq!(info.file_size, BEE.len() as u64);
    let epoch = filetime_from_unix_ns(wtfs_core::FIXED_STAT_EPOCH_UNIX_NS);
    assert_eq!(info.last_write_time, epoch);

    // 暂存内容可经句柄读回（按名查找走正常解析，模板文件可见）
    assert_eq!(read_via(&fx, &fctx, 16), BEE.to_vec());

    cleanup(&fx, fctx, 0);

    // 与模板一致 → upper 空、视图回落 lower；M11 起 dedupe 决策不再落库
    // （可推导状态），upper 空 + 视图可见即语义。
    assert_eq!(overlay.read("b.txt").unwrap().as_deref(), Some(BEE));
    overlay.mark_ready().unwrap();
    assert!(matches!(
        overlay.resolve("b.txt").unwrap(),
        Resolution::Lower { .. }
    ));
}

#[test]
fn staged_write_modified_content_lands_upper_with_real_mtime() {
    let fx = fixture();
    let overlay = fx.registry.get("wt-1").unwrap();

    let fctx = create_file(&fx, "\\wt-1\\b.txt");
    write(&fx, &fctx, b"changed");
    let info = file_info(&fx, &fctx);
    assert_eq!(info.file_size, 7);
    // 投影：与模板不一致 → 非固定纪元的 mtime
    assert_ne!(
        info.last_write_time,
        filetime_from_unix_ns(wtfs_core::FIXED_STAT_EPOCH_UNIX_NS)
    );
    cleanup(&fx, fctx, 0);

    assert!(fx.upper_dir.join("b.txt").is_file());
    assert_eq!(
        overlay.read("b.txt").unwrap().as_deref(),
        Some(b"changed".as_slice())
    );
    // M11：决策不落库；stat 推导自 upper 物理文件的真实 mtime。
    overlay.mark_ready().unwrap();
    assert!(matches!(
        overlay.resolve("b.txt").unwrap(),
        Resolution::Upper { .. }
    ));
    let st = overlay.stat("b.txt").unwrap().unwrap();
    assert!(st.mtime_ns > wtfs_core::FIXED_STAT_EPOCH_UNIX_NS);
    let real = std::fs::metadata(fx.upper_dir.join("b.txt"))
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64;
    assert_eq!(st.mtime_ns, real);

    // 通知：MODIFIED + 父目录
    let events = drain_events(&fx);
    assert!(events
        .iter()
        .any(|e| e.name == "\\wt-1\\b.txt" && e.action == FILE_ACTION_MODIFIED));
}

#[test]
fn staged_delete_discards_without_visible_trace() {
    let fx = fixture();
    let fctx = create_file(&fx, "\\wt-1\\b.txt");
    write(&fx, &fctx, b"partial");
    // delete-on-close：暂存直接丢弃（"partial" 不得出现）。B100100 起
    // create 的文件当场存在，delete 语义 = 删除（R2：撤占位 / 记 whiteout），
    // 不得回落 lower 复活旧内容。
    fx.ctx.set_delete(&fctx, &p("\\wt-1\\b.txt"), true).unwrap();
    cleanup(&fx, fctx, 0);
    assert!(!fx.upper_dir.join("b.txt").exists());
    let overlay = fx.registry.get("wt-1").unwrap();
    assert_eq!(overlay.resolve("b.txt").unwrap(), Resolution::Whiteout);
}

#[test]
fn ready_view_merge_read_and_in_place_dedupe() {
    let fx = fixture();
    let overlay = fx.registry.get("wt-1").unwrap();
    fx.registry.mark_ready("wt-1").unwrap();

    // 按名查找恢复可见
    let s = sec(&fx, "\\wt-1\\b.txt").unwrap();
    assert!(s.attributes & wtfs_core::FileAttributes::ARCHIVE.bits() != 0);
    let s = sec(&fx, "\\wt-1\\a").unwrap();
    assert!(s.attributes & wtfs_core::FileAttributes::DIRECTORY.bits() != 0);

    // 读 lower
    let fctx = open(&fx, "\\wt-1\\b.txt").unwrap();
    assert_eq!(read_via(&fx, &fctx, 16), BEE.to_vec());

    // 就地修改：copy-up + pwrite，cleanup 终裁仍 upper
    write(&fx, &fctx, b"modified");
    cleanup(&fx, fctx, 0);
    assert_eq!(
        overlay.read("b.txt").unwrap().as_deref(),
        Some(b"modified".as_slice())
    );

    // 改回模板内容（FILE_OVERWRITE 截断语义，git CREATE_ALWAYS 走这条）
    // → post-hoc 回收 upper
    let fctx = open(&fx, "\\wt-1\\b.txt").unwrap();
    let mut info = FileInfo::default();
    fx.ctx.overwrite(&fctx, 0, false, 0, None, &mut info).unwrap();
    write(&fx, &fctx, BEE);
    cleanup(&fx, fctx, 0);
    assert!(!fx.upper_dir.join("b.txt").exists());
    assert_eq!(overlay.read("b.txt").unwrap().as_deref(), Some(BEE));
}

#[test]
fn ready_overwrite_open_truncates_eof_on_shorter_write() {
    // B060100 回归：CREATE_ALWAYS（FILE_OVERWRITE_IF）覆写已有 upper 文件，
    // 更短的内容必须把 EOF 收缩到写入长度，不得残留旧尾部
    // （M4 真机缺陷："# hello\nd\n"）。
    let fx = fixture();
    let overlay = fx.registry.get("wt-1").unwrap();

    // initializing（checkout 流）先制造已有 upper 文件（长内容，非模板内容）
    let fctx = create_file(&fx, "\\wt-1\\b.txt");
    write(&fx, &fctx, b"longer changed content");
    cleanup(&fx, fctx, 0);

    fx.registry.mark_ready("wt-1").unwrap();
    assert!(matches!(
        overlay.resolve("b.txt").unwrap(),
        Resolution::Upper { .. }
    ));

    // 覆写打开 + 写更短内容 + 关闭：终裁后内容精确等于新写入
    // （非模板内容，确保留在 upper 供物理断言）
    let fctx = open_overwrite(&fx, "\\wt-1\\b.txt").unwrap();
    assert_eq!(write(&fx, &fctx, b"short"), 5);
    cleanup(&fx, fctx, 0);
    assert_eq!(
        overlay.read("b.txt").unwrap().as_deref(),
        Some(b"short".as_slice())
    );
    let md = std::fs::metadata(fx.upper_dir.join("b.txt")).unwrap();
    assert_eq!(md.len(), 5, "upper EOF must shrink to write");
}

#[test]
fn ready_overwrite_open_without_writes_lands_empty_file() {
    // CREATE_ALWAYS 语义打开后零写入即关闭 → 视图为空文件（Windows 在打开
    // 时即清空；git checkout 对空文件 blob 正是 create + 零写入）。
    let fx = fixture();
    let overlay = fx.registry.get("wt-1").unwrap();

    let fctx = create_file(&fx, "\\wt-1\\b.txt");
    write(&fx, &fctx, b"nonempty");
    cleanup(&fx, fctx, 0);

    fx.registry.mark_ready("wt-1").unwrap();
    let fctx = open_overwrite(&fx, "\\wt-1\\b.txt").unwrap();
    cleanup(&fx, fctx, 0);
    assert_eq!(overlay.read("b.txt").unwrap().as_deref(), Some(&b""[..]));

    // 空内容写回模板一致内容 → 仍回收回落 lower（dedupe 终裁语义不变）
    let fctx = open_overwrite(&fx, "\\wt-1\\b.txt").unwrap();
    write(&fx, &fctx, BEE);
    cleanup(&fx, fctx, 0);
    assert!(!fx.upper_dir.join("b.txt").exists());
    assert_eq!(overlay.read("b.txt").unwrap().as_deref(), Some(BEE));
}

#[test]
fn ready_overwrite_open_over_lower_skips_copy_up_and_truncates() {
    // CREATE_ALWAYS 覆写 lower（从未改过）文件且内容更短：不复制模板全文，
    // EOF 收缩到写入长度；写回模板内容仍回收回落 lower。
    let fx = fixture();
    let overlay = fx.registry.get("wt-1").unwrap();
    fx.registry.mark_ready("wt-1").unwrap();

    let fctx = open_overwrite(&fx, "\\wt-1\\exec.sh").unwrap();
    write(&fx, &fctx, b"x");
    cleanup(&fx, fctx, 0);
    assert_eq!(overlay.read("exec.sh").unwrap().as_deref(), Some(b"x".as_slice()));
    let st = overlay.stat("exec.sh").unwrap().unwrap();
    assert!(st.mtime_ns > wtfs_core::FIXED_STAT_EPOCH_UNIX_NS, "upper 逻辑 mtime");

    // 写回模板一致内容 → 回收 upper 回落 lower（dedupe 终裁语义保持）
    let fctx = open_overwrite(&fx, "\\wt-1\\exec.sh").unwrap();
    write(&fx, &fctx, RUN);
    cleanup(&fx, fctx, 0);
    assert!(!fx.upper_dir.join("exec.sh").exists());
    assert_eq!(
        overlay.read("exec.sh").unwrap().as_deref(),
        Some(RUN)
    );
}

#[test]
fn set_allocation_size_shrink_truncates_eof_for_std_create_always() {
    // B060100 回归（真机缺陷主路径）：Rust std 对 CREATE_ALWAYS 的模拟
    // （rust-lang/rust#115745：OPEN_ALWAYS + FileAllocationInfo(0) 手动清空）
    // 依赖"分配大小收缩即截断 EOF"的 NT 语义；分配收缩被忽略时 std::fs::write
    // 会残留旧尾部（M4 真机复现："# hello\nd\n"）。
    let fx = fixture();
    let overlay = fx.registry.get("wt-1").unwrap();

    // initializing 制造已有 upper 文件（10 字节）
    let fctx = create_file(&fx, "\\wt-1\\b.txt");
    write(&fx, &fctx, b"0123456789");
    cleanup(&fx, fctx, 0);

    fx.registry.mark_ready("wt-1").unwrap();
    // std::fs::write 的完整流：OPEN_IF 打开（无截断标记）→ 分配大小收缩
    // 到 0 → 写新内容 → 关闭终裁
    let fctx = open_as(&fx, "\\wt-1\\b.txt", FILE_OPEN_IF).unwrap();
    let mut info = FileInfo::default();
    fx.ctx.set_file_size(&fctx, 0, true, &mut info).unwrap();
    write(&fx, &fctx, b"short");
    cleanup(&fx, fctx, 0);
    assert_eq!(
        overlay.read("b.txt").unwrap().as_deref(),
        Some(b"short".as_slice())
    );
    let md = std::fs::metadata(fx.upper_dir.join("b.txt")).unwrap();
    assert_eq!(md.len(), 5, "EOF must shrink with allocation size");

    // 增长/持平的分配大小仍是纯预占提示：不改变 EOF
    let fctx = open_as(&fx, "\\wt-1\\b.txt", FILE_OPEN_IF).unwrap();
    let mut info = FileInfo::default();
    fx.ctx.set_file_size(&fctx, 4096, true, &mut info).unwrap();
    assert_eq!(file_info(&fx, &fctx).file_size, 5);
    cleanup(&fx, fctx, 0);
    assert_eq!(
        overlay.read("b.txt").unwrap().as_deref(),
        Some(b"short".as_slice())
    );
}

#[test]
fn plain_open_partial_write_keeps_tail_beyond_written_range() {
    // 普通打开（FILE_OPEN）+ 从 0 写更短内容 = 部分覆写（patch 语义）：
    // EOF 不得收缩（否则编辑器/构建工具的小范围写会静默丢数据）。
    let fx = fixture();
    let overlay = fx.registry.get("wt-1").unwrap();

    let fctx = create_file(&fx, "\\wt-1\\b.txt");
    write(&fx, &fctx, b"0123456789");
    cleanup(&fx, fctx, 0);

    fx.registry.mark_ready("wt-1").unwrap();
    let fctx = open(&fx, "\\wt-1\\b.txt").unwrap();
    write(&fx, &fctx, b"abc");
    cleanup(&fx, fctx, 0);
    assert_eq!(
        overlay.read("b.txt").unwrap().as_deref(),
        Some(b"abc3456789".as_slice()),
        "patch write must keep the tail beyond the written range"
    );
}

#[test]
fn whiteout_delete_and_recreate() {
    let fx = fixture();
    let overlay = fx.registry.get("wt-1").unwrap();
    fx.registry.mark_ready("wt-1").unwrap();

    // 删除 lower 文件 → whiteout
    let fctx = open(&fx, "\\wt-1\\b.txt").unwrap();
    fx.ctx.set_delete(&fctx, &p("\\wt-1\\b.txt"), true).unwrap();
    cleanup(&fx, fctx, FspCleanupFlags::FspCleanupDelete as u32);
    assert!(sec(&fx, "\\wt-1\\b.txt").is_err());
    assert_eq!(overlay.resolve("b.txt").unwrap(), Resolution::Whiteout);
    let whites = overlay.store().whiteouts(WorktreeId(1)).unwrap();
    assert!(whites.contains("b.txt"));

    // 目录非空不可删
    let dctx = open(&fx, "\\wt-1\\a").unwrap();
    assert!(fx.ctx.set_delete(&dctx, &p("\\wt-1\\a"), true).is_err());

    // 重建后 whiteout 撤销、可见
    let fctx = create_file(&fx, "\\wt-1\\b.txt");
    write(&fx, &fctx, b"fresh");
    cleanup(&fx, fctx, 0);
    let s = sec(&fx, "\\wt-1\\b.txt").unwrap();
    assert!(s.attributes & wtfs_core::FileAttributes::ARCHIVE.bits() != 0);
}

// ---- B100100：新建零写入关闭（git checkout 零字节 blob） ----

/// git checkout 写一个文件的完整回调序列（force 检出）：
/// 前置 lstat 命中 → unlink（whiteout）→ CREATE_NEW 新建 →（可选写入）→ close。
/// 返回 `rel` 的最终解析结果断言由调用方完成。
fn checkout_rewrite(fx: &Fixture, rel: &str, content: Option<&[u8]>) {
    let path = format!("\\wt-1\\{rel}");
    assert!(sec(fx, &path).is_ok(), "checkout 前置 lstat 必须命中模板: {rel}");
    let fctx = open(fx, &path).unwrap();
    fx.ctx.set_delete(&fctx, &p(&path), true).unwrap();
    cleanup(fx, fctx, FspCleanupFlags::FspCleanupDelete as u32);
    let fctx = create_file(fx, &path);
    if let Some(data) = content {
        write(fx, &fctx, data);
    }
    cleanup(fx, fctx, 0);
}

/// B100100 核心回归（模板覆盖的根级空文件）：修复前 unlink 留下的 whiteout
/// 无人撤销（零写入 close 无终裁），git 报 "unable to stat just-written file"；
/// 修复后 create 当场撤销 whiteout——一致空内容回落 lower，零 upper 物化、
/// 零 fsync。
#[test]
fn zero_write_create_clears_checkout_whiteout_for_template_empty_file() {
    let fx = fixture_with_empty();
    let overlay = fx.registry.get("wt-1").unwrap();

    checkout_rewrite(&fx, "empty.txt", None);
    assert!(
        matches!(overlay.resolve("empty.txt").unwrap(), Resolution::Lower { .. }),
        "whiteout 必须被撤销，回落 lower"
    );
    let s = sec(&fx, "\\wt-1\\empty.txt").unwrap();
    assert!(s.attributes & wtfs_core::FileAttributes::ARCHIVE.bits() != 0);
    assert!(!fx.upper_dir.join("empty.txt").exists(), "一致内容零 upper 残留");
    assert_eq!(overlay.io_stats().fsyncs(), 0, "空 blob 回落 lower 零 fsync");
    assert_eq!(overlay.io_stats().upper_writes(), 0, "空 blob 零 upper 物化");

    // whiteout 撤销后零残留（M11：决策不落库，视图回落 lower 即语义）。
    overlay.mark_ready().unwrap();
    assert!(matches!(
        overlay.resolve("empty.txt").unwrap(),
        Resolution::Lower { .. }
    ));
    assert!(overlay.store().whiteouts(WorktreeId(1)).unwrap().is_empty());
}

/// 嵌套空文件（git 先 mkdir 子目录再写）：同样必须恢复可见。
#[test]
fn zero_write_create_clears_checkout_whiteout_for_nested_empty_file() {
    let fx = fixture_with_empty();
    let overlay = fx.registry.get("wt-1").unwrap();

    let dctx = create_dir(&fx, "\\wt-1\\nested");
    cleanup(&fx, dctx, 0);
    checkout_rewrite(&fx, "nested/empty.txt", None);

    assert!(
        matches!(overlay.resolve("nested/empty.txt").unwrap(), Resolution::Lower { .. })
    );
    assert!(!fx.upper_dir.join("nested/empty.txt").exists());
    assert_eq!(overlay.io_stats().fsyncs(), 0);
    assert_eq!(overlay.io_stats().upper_writes(), 0);
}

/// 模板外新路径（含全量 copy 兜底模式）：create 当场物化空 upper 占位（路径
/// 立即可解析），零写入 close 经终裁持久化（NT 语义 CREATE_NEW + close =
/// 文件存在、EOF 0）。
#[test]
fn zero_write_create_materializes_new_empty_file() {
    let fx = fixture_with_empty();
    let overlay = fx.registry.get("wt-1").unwrap();

    let fctx = create_file(&fx, "\\wt-1\\brand-new.txt");
    cleanup(&fx, fctx, 0);

    assert!(matches!(
        overlay.resolve("brand-new.txt").unwrap(),
        Resolution::Upper { .. }
    ));
    let st = overlay.stat("brand-new.txt").unwrap().unwrap();
    assert_eq!(st.size, 0);
    assert_eq!(
        fx.upper_dir.join("brand-new.txt").metadata().unwrap().len(),
        0,
        "空 upper 文件 EOF 精确为 0"
    );
    // create 时占位物化（无 fsync）+ cleanup 零写入终裁持久化（1 fsync）。
    assert_eq!(overlay.io_stats().fsyncs(), 1);
    assert_eq!(overlay.io_stats().upper_writes(), 2);
    overlay.mark_ready().unwrap();
    // 占位 upper 是数据真相（M11：无 dedupe 行）。
    assert!(matches!(
        overlay.resolve("brand-new.txt").unwrap(),
        Resolution::Upper { .. }
    ));
}

/// ready 态的 `git checkout <空文件>`（lstat→unlink→create→零写 close）：
/// 修复前同样报 "unable to stat just-written file"；回归守护。
#[test]
fn ready_zero_write_recreate_of_empty_lower_file_stays_visible() {
    let fx = fixture_with_empty();
    let overlay = fx.registry.get("wt-1").unwrap();
    fx.registry.mark_ready("wt-1").unwrap();

    checkout_rewrite(&fx, "empty.txt", None);
    assert!(matches!(overlay.resolve("empty.txt").unwrap(), Resolution::Lower { .. }));
    assert!(!fx.upper_dir.join("empty.txt").exists());
}

/// B100100 混合检出序列：空文件 + 嵌套空文件 + 非空文件按 checkout 顺序
/// 全部走 unlink→create→写/close；模板一致内容零 upper/零 fsync，mark_ready
/// 后枚举完整、内容正确（git status 干净的 FS 侧前提）。
#[test]
fn mixed_checkout_empty_nested_and_nonempty_files_end_clean() {
    let fx = fixture_with_empty();
    let overlay = fx.registry.get("wt-1").unwrap();

    checkout_rewrite(&fx, "empty.txt", None);
    let dctx = create_dir(&fx, "\\wt-1\\nested");
    cleanup(&fx, dctx, 0);
    checkout_rewrite(&fx, "nested/empty.txt", None);
    checkout_rewrite(&fx, "b.txt", Some(BEE));
    checkout_rewrite(&fx, "a/1.txt", Some(ONE));

    for rel in ["empty.txt", "nested/empty.txt", "b.txt", "a/1.txt"] {
        assert!(
            matches!(overlay.resolve(rel).unwrap(), Resolution::Lower { .. }),
            "{rel} 必须可见"
        );
        assert!(!fx.upper_dir.join(rel).exists(), "{rel} 零 upper 残留");
    }
    assert_eq!(overlay.io_stats().fsyncs(), 0, "模板一致混合检出零 fsync");
    assert_eq!(overlay.io_stats().upper_writes(), 0, "模板一致混合检出零物化");

    overlay.mark_ready().unwrap();
    let mut root: Vec<String> = overlay
        .list("")
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    root.sort();
    assert_eq!(root, vec!["a", "b.txt", "empty.txt", "exec.sh", "nested"]);
    assert_eq!(
        overlay
            .list("nested")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect::<Vec<_>>(),
        vec!["empty.txt"]
    );
    assert_eq!(overlay.read("empty.txt").unwrap().as_deref(), Some(&b""[..]));
}

/// B100100 R2 回归：create→write→delete-on-close 不得残留 create 当场留下的
/// 状态——NotFound 占位连带 stat/dedupe 删除（幽灵空文件会让后续 CREATE_NEW
/// collision）；whiteout 已撤销的模板路径重新记 whiteout，lower 内容不复活。
#[test]
fn staged_delete_on_close_cleans_state_left_by_create() {
    let fx = fixture_with_empty();
    let overlay = fx.registry.get("wt-1").unwrap();

    // NotFound 变体（模板外新路径）：占位删除 + 后续 CREATE_NEW 不 collision
    let fctx = create_file(&fx, "\\wt-1\\brand-new.txt");
    write(&fx, &fctx, b"temp");
    let path = "\\wt-1\\brand-new.txt";
    fx.ctx.set_delete(&fctx, &p(path), true).unwrap();
    cleanup(&fx, fctx, FspCleanupFlags::FspCleanupDelete as u32);
    assert_eq!(
        overlay.resolve("brand-new.txt").unwrap(),
        Resolution::NotFound,
        "delete-on-close 后不得残留幽灵占位"
    );
    assert!(!fx.upper_dir.join("brand-new.txt").exists());
    let recreated = create_file(&fx, path);
    cleanup(&fx, recreated, 0);
    assert!(matches!(
        overlay.resolve("brand-new.txt").unwrap(),
        Resolution::Upper { .. }
    ));

    // whiteout 变体（模板覆盖空 blob）：unlink → create 撤销 whiteout →
    // 暂存写 → delete-on-close：重新记 whiteout，lower 内容不复活
    let path = "\\wt-1\\empty.txt";
    let unlink = open(&fx, path).unwrap();
    fx.ctx.set_delete(&unlink, &p(path), true).unwrap();
    cleanup(&fx, unlink, FspCleanupFlags::FspCleanupDelete as u32);
    let fctx = create_file(&fx, path);
    write(&fx, &fctx, b"temporary");
    fx.ctx.set_delete(&fctx, &p(path), true).unwrap();
    cleanup(&fx, fctx, FspCleanupFlags::FspCleanupDelete as u32);
    assert_eq!(
        overlay.resolve("empty.txt").unwrap(),
        Resolution::Whiteout,
        "已删除内容不得回落 lower 复活"
    );
    assert!(!fx.upper_dir.join("empty.txt").exists());

    // whiteout 变体（非空模板 b.txt）：同序列同语义
    let path = "\\wt-1\\b.txt";
    let unlink = open(&fx, path).unwrap();
    fx.ctx.set_delete(&unlink, &p(path), true).unwrap();
    cleanup(&fx, unlink, FspCleanupFlags::FspCleanupDelete as u32);
    let fctx = create_file(&fx, path);
    write(&fx, &fctx, b"temporary");
    fx.ctx.set_delete(&fctx, &p(path), true).unwrap();
    cleanup(&fx, fctx, FspCleanupFlags::FspCleanupDelete as u32);
    assert_eq!(overlay.resolve("b.txt").unwrap(), Resolution::Whiteout);
    assert!(!fx.upper_dir.join("b.txt").exists());

    // 对照：正常 checkout 序列（无 delete-on-close）不受影响
    checkout_rewrite(&fx, "nested/empty.txt", None);
    assert!(matches!(
        overlay.resolve("nested/empty.txt").unwrap(),
        Resolution::Lower { .. }
    ));
}

#[test]
fn set_delete_allows_empty_worktree_root_and_unregisters_on_cleanup() {
    // B060100 回归：`git worktree remove` 的收尾 rmdir 必须能删 worktree 根。
    // 正常文件/目录删除语义（非空拒绝、delete-on-close）不变。
    let fx = fixture();
    let overlay = fx.registry.get("wt-1").unwrap();

    // initializing（checkout 进行中）→ 拒绝根删除
    let dctx = open(&fx, "\\wt-1").unwrap();
    assert!(fx.ctx.set_delete(&dctx, &p("\\wt-1"), true).is_err());
    drop(dctx);

    fx.registry.mark_ready("wt-1").unwrap();

    // 挂载根不可删
    let root_ctx = open(&fx, "\\").unwrap();
    assert!(fx.ctx.set_delete(&root_ctx, &p("\\"), true).is_err());
    drop(root_ctx);

    // 视图非空 → 目录非空拒绝（与普通目录语义一致）
    let dctx = open(&fx, "\\wt-1").unwrap();
    assert!(fx.ctx.set_delete(&dctx, &p("\\wt-1"), true).is_err());

    // 自底向上删空视图（git 的删除顺序：文件 → 子目录 → 根）
    for name in ["\\wt-1\\a\\1.txt", "\\wt-1\\a\\2.txt"] {
        let fctx = open(&fx, name).unwrap();
        fx.ctx.set_delete(&fctx, &p(name), true).unwrap();
        cleanup(&fx, fctx, FspCleanupFlags::FspCleanupDelete as u32);
    }
    let dctx = open(&fx, "\\wt-1\\a").unwrap();
    fx.ctx.set_delete(&dctx, &p("\\wt-1\\a"), true).unwrap();
    cleanup(&fx, dctx, FspCleanupFlags::FspCleanupDelete as u32);
    for name in ["\\wt-1\\b.txt", "\\wt-1\\exec.sh"] {
        let fctx = open(&fx, name).unwrap();
        fx.ctx.set_delete(&fctx, &p(name), true).unwrap();
        cleanup(&fx, fctx, FspCleanupFlags::FspCleanupDelete as u32);
    }
    assert!(fx.registry.get("wt-1").is_some(), "视图清空前仍注册");

    // 取消删除（set_delete(false)）→ cleanup 不注销
    let dctx = open(&fx, "\\wt-1").unwrap();
    fx.ctx.set_delete(&dctx, &p("\\wt-1"), true).unwrap();
    fx.ctx.set_delete(&dctx, &p("\\wt-1"), false).unwrap();
    cleanup(&fx, dctx, 0);
    assert!(fx.registry.get("wt-1").is_some(), "取消删除后仍注册");

    // delete-on-close 根删除 → cleanup 注销路由
    let dctx = open(&fx, "\\wt-1").unwrap();
    fx.ctx.set_delete(&dctx, &p("\\wt-1"), true).unwrap();
    cleanup(&fx, dctx, 0);
    assert!(fx.registry.get("wt-1").is_none(), "根删除后必须注销路由");
    let events = drain_events(&fx);
    assert!(
        events
            .iter()
            .any(|e| e.name == "\\wt-1" && e.action == FILE_ACTION_REMOVED)
    );
    // overlay DB 行已删（级联清子表），upper 物理目录保留为孤儿——由
    // `wtfs gc` 回收（spec §6.6，与 daemon watcher 注销路径一致）
    assert_eq!(overlay.store().state(WorktreeId(1)).unwrap(), None);
    assert!(fx.upper_dir.exists(), "upper 目录留给 gc");
}

#[test]
fn rename_file_and_directory() {
    let fx = fixture();
    let overlay = fx.registry.get("wt-1").unwrap();
    fx.registry.mark_ready("wt-1").unwrap();

    // 文件级：whiteout 源 + upper 目标
    let fctx = open(&fx, "\\wt-1\\b.txt").unwrap();
    fx.ctx
        .rename(&fctx, &p("\\wt-1\\b.txt"), &p("\\wt-1\\c.txt"), false)
        .unwrap();
    assert!(sec(&fx, "\\wt-1\\b.txt").is_err());
    assert!(sec(&fx, "\\wt-1\\c.txt").is_ok());
    assert_eq!(overlay.read("c.txt").unwrap().as_deref(), Some(BEE));

    // rename 通知
    let events = drain_events(&fx);
    assert!(events
        .iter()
        .any(|e| e.action == FILE_ACTION_RENAMED_OLD_NAME && e.name == "\\wt-1\\b.txt"));
    assert!(events
        .iter()
        .any(|e| e.action == FILE_ACTION_RENAMED_NEW_NAME && e.name == "\\wt-1\\c.txt"));

    // 目录级：rename map
    let dctx = open(&fx, "\\wt-1\\a").unwrap();
    fx.ctx
        .rename(&dctx, &p("\\wt-1\\a"), &p("\\wt-1\\z"), false)
        .unwrap();
    assert!(sec(&fx, "\\wt-1\\a").is_err());
    assert!(sec(&fx, "\\wt-1\\z").is_ok());
    let fctx = open(&fx, "\\wt-1\\z\\1.txt").unwrap();
    assert_eq!(read_via(&fx, &fctx, 16), ONE.to_vec());

    // 目标已存在且不允许替换 → collision
    let fctx = open(&fx, "\\wt-1\\c.txt").unwrap();
    assert!(fx
        .ctx
        .rename(&fctx, &p("\\wt-1\\c.txt"), &p("\\wt-1\\z\\2.txt"), false)
        .is_err());
}

#[test]
fn read_directory_lists_merged_entries_and_worktrees() {
    let fx = fixture();
    let overlay = fx.registry.get("wt-1").unwrap();

    // initializing：挂载根只见 wt-1；wt-1 枚举为空
    assert_eq!(dir_names(&fx, "\\"), ["wt-1".to_string()]);
    assert!(dir_names(&fx, "\\wt-1").is_empty());

    overlay.mark_ready().unwrap();
    assert_eq!(dir_names(&fx, "\\"), ["wt-1".to_string()]);
    assert_eq!(dir_names(&fx, "\\wt-1"), ["a", "b.txt", "exec.sh"]);
    assert_eq!(dir_names(&fx, "\\wt-1\\a"), ["1.txt", "2.txt"]);

    // upper 新文件并入
    let fctx = create_file(&fx, "\\wt-1\\a\\3.txt");
    write(&fx, &fctx, b"three");
    cleanup(&fx, fctx, 0);
    assert_eq!(dir_names(&fx, "\\wt-1\\a"), ["1.txt", "2.txt", "3.txt"]);
}

#[test]
fn create_collision_after_ready_but_hidden_replace_during_initializing() {
    let fx = fixture();
    let overlay = fx.registry.get("wt-1").unwrap();

    // initializing：视图为空 → create "已存在"的 b.txt 合法（替换隐藏 upper）
    let fctx = create_file(&fx, "\\wt-1\\b.txt");
    write(&fx, &fctx, b"zzz");
    cleanup(&fx, fctx, 0);
    assert_eq!(
        overlay.read("b.txt").unwrap().as_deref(),
        Some(b"zzz".as_slice())
    );

    // ready：同名 create → collision
    overlay.mark_ready().unwrap();
    assert!(fx
        .ctx
        .create(
            &p("\\wt-1\\b.txt"),
            0,
            0,
            0,
            None,
            0,
            None,
            false,
            &mut blank_open_file_info_for_test()
        )
        .is_err());
    // 新名 create 合法
    let fctx = create_file(&fx, "\\wt-1\\new.txt");
    write(&fx, &fctx, b"n");
    cleanup(&fx, fctx, 0);
    assert_eq!(
        overlay.read("new.txt").unwrap().as_deref(),
        Some(b"n".as_slice())
    );
}

#[test]
fn root_dir_create_auto_registers_worktree() {
    let fx = fixture();
    let dctx = create_dir(&fx, "\\wt-2");
    let overlay = fx.registry.get("wt-2").expect("auto-registered");
    assert_eq!(overlay.worktree_id(), WorktreeId(2));
    assert!(overlay.is_initializing().unwrap());
    drop(dctx);

    // 挂载根创建文件被拒绝；非法名被拒绝
    assert!(fx
        .ctx
        .create(
            &p("\\stray.txt"),
            0,
            0,
            0,
            None,
            0,
            None,
            false,
            &mut blank_open_file_info_for_test()
        )
        .is_err());
    assert!(fx
        .ctx
        .create(
            &p("\\con"),
            FILE_DIRECTORY_FILE,
            0,
            0,
            None,
            0,
            None,
            false,
            &mut blank_open_file_info_for_test()
        )
        .is_err());

    // 挂载根枚举出现新 worktree
    assert_eq!(dir_names(&fx, "\\"), ["wt-1", "wt-2"]);
}

#[test]
fn flush_finalizes_staged_and_readonly_flush_is_noop() {
    let fx = fixture();
    let overlay = fx.registry.get("wt-1").unwrap();

    let fctx = create_file(&fx, "\\wt-1\\b.txt");
    write(&fx, &fctx, b"flushed");
    let mut info = FileInfo::default();
    fx.ctx.flush(Some(&fctx), &mut info).unwrap();
    // flush 即终裁：upper 已有内容
    assert!(fx.upper_dir.join("b.txt").is_file());
    assert_eq!(
        overlay.read("b.txt").unwrap().as_deref(),
        Some(b"flushed".as_slice())
    );
    cleanup(&fx, fctx, 0); // 幂等：无暂存状态

    // ready 后只读句柄 flush：不报错
    fx.registry.mark_ready("wt-1").unwrap();
    let fctx = open(&fx, "\\wt-1\\b.txt").unwrap();
    let mut info = FileInfo::default();
    fx.ctx.flush(Some(&fctx), &mut info).unwrap();
    let mut info = FileInfo::default();
    fx.ctx.flush(None, &mut info).unwrap();
}

#[test]
fn staged_truncate_and_set_file_size() {
    let fx = fixture();
    let fctx = create_file(&fx, "\\wt-1\\b.txt");
    write(&fx, &fctx, b"hello world");
    let mut info = FileInfo::default();
    fx.ctx.set_file_size(&fctx, 5, false, &mut info).unwrap();
    assert_eq!(read_via(&fx, &fctx, 16), b"hello".to_vec());
    // 分配大小设置不改变 EOF
    fx.ctx.set_file_size(&fctx, 4096, true, &mut info).unwrap();
    assert_eq!(file_info(&fx, &fctx).file_size, 5);
    cleanup(&fx, fctx, 0);
    let overlay = fx.registry.get("wt-1").unwrap();
    assert_eq!(
        overlay.read("b.txt").unwrap().as_deref(),
        Some(b"hello".as_slice())
    );
}

#[test]
fn set_basic_info_updates_upper_real_mtime() {
    let fx = fixture();
    let overlay = fx.registry.get("wt-1").unwrap();
    fx.registry.mark_ready("wt-1").unwrap();

    // upper 文件
    let fctx = create_file(&fx, "\\wt-1\\new.txt");
    write(&fx, &fctx, b"data");
    cleanup(&fx, fctx, 0);
    let before = overlay.stat("new.txt").unwrap().unwrap().mtime_ns;
    // FILETIME 精度 100ns：目标取 100 的倍数避免回写截断
    let target_ns = wtfs_core::FIXED_STAT_EPOCH_UNIX_NS + 123_456_700;
    let ft = filetime_from_unix_ns(target_ns);
    let fctx = open(&fx, "\\wt-1\\new.txt").unwrap();
    let mut info = FileInfo::default();
    fx.ctx
        .set_basic_info(&fctx, 0, 0, 0, ft, 0, &mut info)
        .unwrap();
    let after = overlay.stat("new.txt").unwrap().unwrap().mtime_ns;
    assert_eq!(after, target_ns);
    assert_ne!(before, after);
}

#[test]
fn volume_info_reports_store_root_drive() {
    let fx = fixture();
    let mut info: VolumeInfo = unsafe { std::mem::zeroed() };
    fx.ctx.get_volume_info(&mut info).unwrap();
    assert!(info.total_size > 0);
    // 卷标签：VolumeInfo 尾部 = label_length(u16) + label([u16; 32])
    let base = &info as *const VolumeInfo as *const u8;
    let label_len = unsafe { *(base.add(16) as *const u16) as usize };
    let units: Vec<u16> = unsafe {
        std::slice::from_raw_parts(base.add(18) as *const u16, label_len / 2)
            .iter()
            .copied()
            .take_while(|&u| u != 0)
            .collect()
    };
    assert_eq!(String::from_utf16_lossy(&units), "WorktreeFS");
}

#[test]
fn security_descriptor_shape() {
    let fx = fixture();
    let sd_len = fixed_security_descriptor_len();
    let mut buf = vec![0u8; sd_len as usize + 64];
    let fctx = open(&fx, "\\wt-1").unwrap();
    let n = fx
        .ctx
        .get_security(
            &fctx,
            Some(unsafe {
                std::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<c_void>(), buf.len())
            }),
        )
        .unwrap();
    assert_eq!(n, sd_len);
    assert!(!buf[..sd_len as usize].iter().all(|&b| b == 0));
    // 缓冲过小 → invalid parameter
    let mut small = vec![0u8; 8];
    assert!(fx
        .ctx
        .get_security(
            &fctx,
            Some(unsafe {
                std::slice::from_raw_parts_mut(small.as_mut_ptr().cast::<c_void>(), small.len())
            }),
        )
        .is_err());
}

#[test]
fn wildcard_matching() {
    assert!(wildcard_match("*.txt", "a.TXT"));
    assert!(wildcard_match("b*txt", "b-something-txt"));
    assert!(wildcard_match("exact", "exact"));
    assert!(!wildcard_match("b?txt", "bsome-txt"));
    assert!(!wildcard_match("*.txt", "a.rs"));
}

#[test]
fn read_directory_enumeration_ends_after_last_entry() {
    let fx = fixture();
    let overlay = fx.registry.get("wt-1").unwrap();
    overlay.mark_ready().unwrap();

    // 从头枚举：点条目 + 全部真实条目
    let fctx = open(&fx, "\\wt-1").unwrap();
    let marker = unsafe { marker_for_test(None) };
    let mut buffer = vec![0u8; 4096];
    let n = fx
        .ctx
        .read_directory(&fctx, None, marker, &mut buffer)
        .unwrap() as usize;
    buffer.truncate(n);
    assert_eq!(parse_entries(&buffer), ["a", "b.txt", "exec.sh"]);

    // 以最后一个真实条目为 marker → 无后续条目，winfsp 只返回 2 字节
    // 终止符（Size=0 的 DirInfo）= 内核视角"枚举结束"。
    let last = U16CString::from_str("exec.sh").unwrap();
    let marker = unsafe { marker_for_test(Some(&last)) };
    let mut buffer = vec![0u8; 4096];
    let n = fx
        .ctx
        .read_directory(&fctx, None, marker, &mut buffer)
        .unwrap();
    assert_eq!(n, 2, "terminator-only reply = no more entries");
    assert_eq!(&buffer[..2], &[0u8, 0u8]);
}

#[test]
fn full_write_content_survives_multiple_chunks_and_mixed_offsets() {
    let fx = fixture();
    let overlay = fx.registry.get("wt-1").unwrap();

    // 多 chunk 顺序写（cache manager 分页写）
    let fctx = create_file(&fx, "\\wt-1\\chunked.txt");
    assert_eq!(write(&fx, &fctx, b"hello "), 6);
    write_at_offset(&fx, &fctx, 6, b"world");
    assert_eq!(file_info(&fx, &fctx).file_size, 11);
    cleanup(&fx, fctx, 0);
    assert_eq!(
        overlay.read("chunked.txt").unwrap().as_deref(),
        Some(b"hello world".as_slice())
    );
}

fn write_at_offset(fx: &Fixture, fctx: &FileContext, offset: u64, data: &[u8]) -> u32 {
    let mut info = FileInfo::default();
    fx.ctx
        .write(fctx, data, offset, false, false, &mut info)
        .unwrap()
}

#[test]
fn open_file_info_helper_is_zeroed() {
    // OpenFileInfo 无公开构造；测试辅助的零值必须可安全传递（实现不写
    // normalized name）。
    let info = blank_open_file_info_for_test();
    assert_eq!(info.normalized_name_size(), 0);
}

// ---- M9/B090100：暂存写零拷贝终裁（I/O 插桩断言） ----

/// 递归统计目录下的文件数（upper 零残留断言用）。
fn count_files(dir: &std::path::Path) -> usize {
    let mut files = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for entry in rd.flatten() {
                if entry.path().is_dir() {
                    stack.push(entry.path());
                } else {
                    files += 1;
                }
            }
        }
    }
    files
}

/// 干净 N 文件 checkout 走 VFS 回调：零 upper 文件、零 fsync；视图回落 lower。
#[test]
fn clean_checkout_via_vfs_creates_zero_upper_files_and_zero_fsyncs() {
    let fx = fixture();
    let overlay = fx.registry.get("wt-1").unwrap();

    for (name, rel, content) in [
        ("\\wt-1\\a\\1.txt", "a/1.txt", ONE),
        ("\\wt-1\\a\\2.txt", "a/2.txt", TWO),
        ("\\wt-1\\b.txt", "b.txt", BEE),
        ("\\wt-1\\exec.sh", "exec.sh", RUN),
    ] {
        let fctx = create_file(&fx, name);
        assert_eq!(write(&fx, &fctx, content), content.len() as u32);
        // fstat 投影：一致内容报固定 stat（index 对齐语义不变）
        let info = file_info(&fx, &fctx);
        assert_eq!(info.file_size, content.len() as u64);
        assert_eq!(
            info.last_write_time,
            filetime_from_unix_ns(wtfs_core::FIXED_STAT_EPOCH_UNIX_NS)
        );
        cleanup(&fx, fctx, 0);
        assert!(matches!(overlay.resolve(rel).unwrap(), Resolution::Lower { .. }));
    }

    assert_eq!(overlay.io_stats().fsyncs(), 0, "干净 checkout 必须零 fsync");
    assert_eq!(
        overlay.io_stats().upper_writes(),
        0,
        "干净 checkout 必须零 upper 物化"
    );
    assert_eq!(count_files(&fx.upper_dir), 0, "upper 整棵零文件（含零临时副本）");
    overlay.mark_ready().unwrap();
    // M11：决策不落库；mark_ready 后视图回落 lower、零 whiteout 行。
    assert!(overlay.store().whiteouts(WorktreeId(1)).unwrap().is_empty());
    for rel in ["a/1.txt", "a/2.txt", "b.txt", "exec.sh"] {
        assert!(matches!(overlay.resolve(rel).unwrap(), Resolution::Lower { .. }));
    }
    assert_eq!(overlay.read("b.txt").unwrap().as_deref(), Some(BEE));
}

/// 改动文件恰好 1 upper 物化 + 1 fsync；混合 checkout 中一致文件不增加计数。
#[test]
fn changed_file_via_vfs_lands_upper_with_exactly_one_fsync() {
    let fx = fixture();
    let overlay = fx.registry.get("wt-1").unwrap();

    let fctx = create_file(&fx, "\\wt-1\\b.txt");
    write(&fx, &fctx, b"changed");
    cleanup(&fx, fctx, 0);
    assert!(fx.upper_dir.join("b.txt").is_file());
    assert_eq!(overlay.io_stats().fsyncs(), 1);
    assert_eq!(overlay.io_stats().upper_writes(), 1);

    // 混合 checkout：其余文件与模板一致 → 计数不再增长
    for (name, content) in [("\\wt-1\\a\\1.txt", ONE), ("\\wt-1\\exec.sh", RUN)] {
        let fctx = create_file(&fx, name);
        write(&fx, &fctx, content);
        cleanup(&fx, fctx, 0);
    }
    assert_eq!(overlay.io_stats().fsyncs(), 1, "一致内容零 fsync");
    assert_eq!(overlay.io_stats().upper_writes(), 1, "一致内容零物化");
    overlay.mark_ready().unwrap();
    assert_eq!(overlay.read("b.txt").unwrap().as_deref(), Some(b"changed".as_slice()));
    assert_eq!(overlay.read("a/1.txt").unwrap().as_deref(), Some(ONE));
}

/// 溢写暂存（>32MiB）终裁：溢写文件同卷 rename 消费，恰好一次 fsync；
/// 乱序小写使增量哈希失效后终裁从溢写文件整段重算，语义不变。
#[test]
fn staged_spill_write_finalizes_via_rename_with_single_fsync() {
    let fx = fixture();
    let overlay = fx.registry.get("wt-1").unwrap();

    let big: Vec<u8> = vec![b'x'; crate::context::STAGE_SPILL_BYTES + 1];
    let fctx = create_file(&fx, "\\wt-1\\big.bin");
    assert_eq!(write(&fx, &fctx, &big), big.len() as u32);
    let spill_count = |fx: &Fixture| {
        std::fs::read_dir(&fx.upper_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".wtfs-tmp-w-"))
            .count()
    };
    assert_eq!(spill_count(&fx), 1, "超阈值内容必须溢写");

    // 乱序写：增量哈希失效 → 终裁走 hash_file 整段重算（内容仍一致判定）
    write_at_offset(&fx, &fctx, 0, b"HEAD!");
    cleanup(&fx, fctx, 0);

    assert_eq!(overlay.io_stats().fsyncs(), 1, "溢写物化恰好一次 fsync");
    // B100100：create 当场物化空 upper 占位（1）+ 溢写终裁内容（1）= 2。
    assert_eq!(overlay.io_stats().upper_writes(), 2);
    assert_eq!(spill_count(&fx), 0, "溢写文件经 rename 消费");
    assert_eq!(overlay.stat("big.bin").unwrap().unwrap().size, big.len() as u64);
    let data = overlay.read("big.bin").unwrap().unwrap();
    assert_eq!(data.len(), big.len());
    assert_eq!(&data[..5], b"HEAD!", "乱序写生效");
    assert!(&data[5..].iter().all(|&b| b == b'x'));
    assert_eq!(count_files(&fx.upper_dir), 1);
}

/// P2-2 回归：溢写态 `set_file_size` 必须作用于溢写文件（而非已清空的
/// mem 缓冲），spill 长度与 staged.size 一致；终裁后 upper EOF、stat size
/// 与读回内容同步（收缩截尾、扩张零填充），哈希从溢写文件整段重算。
#[test]
fn staged_spill_set_file_size_updates_spill_eof_and_finalizes_consistently() {
    let fx = fixture();
    let overlay = fx.registry.get("wt-1").unwrap();

    // 32MiB+1024 触发溢写
    let head: Vec<u8> = vec![b'a'; crate::context::STAGE_SPILL_BYTES + 1024];
    let fctx = create_file(&fx, "\\wt-1\\big.bin");
    assert_eq!(write(&fx, &fctx, &head), head.len() as u32);

    // 收缩：溢写文件截尾，EOF 投影同步
    let mut info = FileInfo::default();
    let shrink = (crate::context::STAGE_SPILL_BYTES + 4) as u64;
    fx.ctx.set_file_size(&fctx, shrink, false, &mut info).unwrap();
    assert_eq!(file_info(&fx, &fctx).file_size, shrink);
    // 增长：零填充扩张，EOF 投影同步
    let grow = (crate::context::STAGE_SPILL_BYTES + 4096) as u64;
    fx.ctx.set_file_size(&fctx, grow, false, &mut info).unwrap();
    assert_eq!(file_info(&fx, &fctx).file_size, grow);
    cleanup(&fx, fctx, 0);

    // 终裁后：upper EOF = stat size = staged size；内容 = 头部 'a' + 尾部零
    let md = std::fs::metadata(fx.upper_dir.join("big.bin")).unwrap();
    assert_eq!(md.len(), grow, "upper EOF 必须与终裁 size 一致（旧实现发散）");
    assert_eq!(overlay.stat("big.bin").unwrap().unwrap().size, grow);
    let data = overlay.read("big.bin").unwrap().unwrap();
    assert_eq!(data.len() as u64, grow);
    assert!(
        data[..crate::context::STAGE_SPILL_BYTES + 4].iter().all(|&b| b == b'a'),
        "收缩保留头部内容"
    );
    assert!(
        data[crate::context::STAGE_SPILL_BYTES + 4..].iter().all(|&b| b == 0),
        "扩张区零填充"
    );
    // M9 目标不受影响：fsync 恰好 1（B100100 起 upper 物化计数含 create
    // 占位 1 + 终裁内容 1 = 2）
    assert_eq!(overlay.io_stats().fsyncs(), 1);
    assert_eq!(overlay.io_stats().upper_writes(), 2);
}
