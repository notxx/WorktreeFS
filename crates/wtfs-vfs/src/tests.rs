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
    Db, DedupeState, Overlay, OverlayConfig, Resolution, SqliteOverlayStore, TemplateFile,
    WorktreeId,
};

use crate::context::{
    FileContext, WtfsContext, blank_open_file_info_for_test, marker_for_test, wildcard_match,
};
use crate::registry::{VfsConfig, VfsRegistry};
use crate::win::{
    FILE_ACTION_MODIFIED, FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME,
    FILE_DIRECTORY_FILE, filetime_from_unix_ns,
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

fn p(s: &str) -> U16CString {
    U16CString::from_str(s).unwrap()
}

fn sec(fx: &Fixture, name: &str) -> winfsp::Result<FileSecurity> {
    fx.ctx.get_security_by_name(&p(name), None, |_| None)
}

fn open(fx: &Fixture, name: &str) -> winfsp::Result<FileContext> {
    fx.ctx.open(&p(name), 0, 0, &mut blank_open_file_info_for_test())
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

    // 与模板一致 → upper 空、视图回落 lower
    assert!(!fx.upper_dir.join("b.txt").exists());
    assert_eq!(overlay.read("b.txt").unwrap().as_deref(), Some(BEE));
    assert_eq!(
        overlay
            .store()
            .dedupe_states(WorktreeId(1))
            .unwrap()
            .get("b.txt"),
        Some(&DedupeState::Lower)
    );
}

#[test]
fn staged_write_modified_content_lands_upper_with_logical_mtime() {
    let fx = fixture();
    let overlay = fx.registry.get("wt-1").unwrap();

    let fctx = create_file(&fx, "\\wt-1\\b.txt");
    write(&fx, &fctx, b"changed");
    let info = file_info(&fx, &fctx);
    assert_eq!(info.file_size, 7);
    // 投影：与模板不一致 → 逻辑 mtime（≠ 固定纪元）
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
    assert_eq!(
        overlay
            .store()
            .dedupe_states(WorktreeId(1))
            .unwrap()
            .get("b.txt"),
        Some(&DedupeState::Upper)
    );
    let st = overlay.stat("b.txt").unwrap().unwrap();
    assert!(st.mtime_ns > wtfs_core::FIXED_STAT_EPOCH_UNIX_NS);

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
    // delete-on-close：暂存直接丢弃，视图无痕
    fx.ctx.set_delete(&fctx, &p("\\wt-1\\b.txt"), true).unwrap();
    cleanup(&fx, fctx, 0);
    assert!(!fx.upper_dir.join("b.txt").exists());
    let overlay = fx.registry.get("wt-1").unwrap();
    assert!(matches!(overlay.resolve("b.txt").unwrap(), Resolution::Lower { .. }));
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
fn set_basic_info_updates_upper_logical_mtime() {
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
