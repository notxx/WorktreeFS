//! CAS 视图写路径集成测试（M18/B180100）：git blob sha1 回查 manifest oid
//! 的 dedupe 回收、copy-up 的 mtime 归一、文件级 rename（目录 EXDEV）、
//! prepare_created_file、refresh_head（HEAD 前进重载 manifest + 剪枝失效
//! whiteout）。不挂载。

#![cfg(windows)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use tempfile::TempDir;
use wtfs_cas::{
    CasResolution, CasView, CasViewConfig, CatFile, Error, HydrationCache, Manifest,
    StagedSource, WriteState, file_id,
};

struct Fixture {
    _dir: TempDir,
    repo: PathBuf,
    commit: String,
    view: CasView,
    upper_root: PathBuf,
    store: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_demo_repo(dir.path());
    let commit = {
        let (ok, out) = common::git(&repo, &["rev-parse", "HEAD"]);
        assert!(ok, "rev-parse HEAD: {out}");
        out.trim().to_string()
    };
    let upper_root = dir.path().join("upper");
    std::fs::create_dir_all(&upper_root).unwrap();
    let store = dir.path().join("store");
    let manifest = Arc::new(Manifest::load(&repo, &commit).unwrap());
    let cat = Arc::new(CatFile::new(&repo));
    let cache = Arc::new(HydrationCache::open(&store));
    let config = CasViewConfig::new(&upper_root, dir.path().join("meta"));
    let view = CasView::open(config, manifest, cat, cache).unwrap();
    Fixture {
        _dir: dir,
        repo,
        commit,
        view,
        upper_root,
        store,
    }
}

fn manifest_file(fx: &Fixture, rel: &str) -> wtfs_cas::ManifestFile {
    fx.view.manifest().file(rel).unwrap().clone()
}

/// `git hash-object` 等价的 git blob oid（测试侧对齐基准）。
fn blob_oid(bytes: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(format!("blob {}\0", bytes.len()).into_bytes());
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn write_same_content_reclaims_to_lower_with_zero_upper() {
    let mut fx = fixture();
    let f = manifest_file(&fx, "README.md");
    let content = fx.view.read("README.md").unwrap().unwrap();
    assert_eq!(blob_oid(&content), f.oid, "read 回读内容必须对齐 manifest oid");

    let out = fx.view.write("README.md", &content).unwrap();
    assert_eq!(out.state, WriteState::Lower, "一致内容必须回落 manifest");
    assert_eq!(out.bytes, content.len() as u64);
    assert!(
        !fx.upper_root.join("README.md").exists(),
        "回收后 upper 不得残留"
    );
    // stat 保持 manifest 固定值（index 对齐不被写路径破坏）。
    let st = fx.view.stat("README.md").unwrap().unwrap();
    assert_eq!(st.ino, file_id("README.md"));
    assert_eq!(st.size, f.size);
}

#[test]
fn write_different_content_materializes_upper_once() {
    let mut fx = fixture();
    let original = fx.view.read("README.md").unwrap().unwrap();
    let changed = b"# changed\n";
    let out = fx.view.write("README.md", changed).unwrap();
    assert_eq!(out.state, WriteState::Upper, "覆盖 manifest 路径 → Upper");
    assert_eq!(fx.view.read("README.md").unwrap().unwrap(), changed);
    assert!(fx.upper_root.join("README.md").is_file());

    // 改回原内容：copy-up 后的回收语义（write_from 同路径）。
    let out = fx
        .view
        .write_from("README.md", &mut std::io::Cursor::new(&original))
        .unwrap();
    assert_eq!(out.state, WriteState::Lower);
    assert!(!fx.upper_root.join("README.md").exists(), "回收后零 upper");
}

#[test]
fn copy_up_normalizes_mtime_to_epoch() {
    let fx = fixture();
    let copied = fx.view.copy_up("README.md").unwrap().expect("manifest hit");
    assert_eq!(copied, fx.upper_root.join("README.md"));
    let md = std::fs::symlink_metadata(&copied).unwrap();
    let mtime_ns = md
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64;
    assert_eq!(
        mtime_ns,
        wtfs_cas::FIXED_STAT_EPOCH_UNIX_NS,
        "copy-up 不打脏 index：mtime 归一到固定纪元"
    );
    // 内容与 manifest 一致（cache 灌入后复制）。
    let cached = fx.view.cache().stats();
    assert!(cached.hydrated_files >= 1, "copy-up 触发一次 hydrate");
}

#[test]
fn copy_up_upper_hit_returns_existing_path() {
    let fx = fixture();
    std::fs::create_dir_all(fx.upper_root.join("docs")).unwrap();
    std::fs::write(fx.upper_root.join("docs/a.md"), b"upper\n").unwrap();
    let copied = fx.view.copy_up("docs/a.md").unwrap().expect("upper hit");
    assert_eq!(copied, fx.upper_root.join("docs/a.md"));
    // whiteout/not-found → None
    assert!(fx.view.copy_up("no/such.txt").unwrap().is_none());
}

#[test]
fn finish_staged_memory_dedupes_and_materializes() {
    let mut fx = fixture();
    // 一致 → Lower 零 upper。
    let content = fx.view.read("README.md").unwrap().unwrap();
    let out = fx
        .view
        .finish_staged("README.md", content.len() as u64, StagedSource::Memory(&content))
        .unwrap();
    assert_eq!(out.state, WriteState::Lower);
    assert!(!fx.upper_root.join("README.md").exists());

    // 不一致 → Upper，恰好一次落盘。
    let out = fx
        .view
        .finish_staged("README.md", 5, StagedSource::Memory(b"other"))
        .unwrap();
    assert_eq!(out.state, WriteState::Upper);
    assert_eq!(fx.view.read("README.md").unwrap().unwrap(), b"other");
}

#[test]
fn finish_staged_spill_materializes_and_reclaims() {
    let mut fx = fixture();
    // 溢写文件路径：upper 目录内 .wtfs-tmp 前缀（视图隐藏）。
    let spill = fx.upper_root.join(".wtfs-tmp-w-test-1");
    std::fs::write(&spill, b"spilled content\n").unwrap();
    let out = fx
        .view
        .finish_staged("README.md", 17, StagedSource::Spill(&spill))
        .unwrap();
    assert_eq!(out.state, WriteState::Upper, "与 manifest 不同 → 物化");
    assert!(!spill.exists(), "溢写文件被改名消费");
    assert_eq!(
        fx.view.read("README.md").unwrap().unwrap(),
        b"spilled content\n"
    );

    // 与 manifest 一致 → 回收（删溢写 + 既有 upper，零 fsync）。
    let content = fx.view.read("src/lib.rs").unwrap().unwrap();
    let spill = fx.upper_root.join(".wtfs-tmp-w-test-2");
    std::fs::write(&spill, &content).unwrap();
    let out = fx
        .view
        .finish_staged("src/lib.rs", content.len() as u64, StagedSource::Spill(&spill))
        .unwrap();
    assert_eq!(out.state, WriteState::Lower);
    assert!(!spill.exists(), "回收路径溢写文件被删除");
    assert!(!fx.upper_root.join("src/lib.rs").exists());
}

#[test]
fn prepare_created_file_unwhiteouts_and_placeholders() {
    let mut fx = fixture();
    // whiteout → 撤销后视图回落 manifest。
    fx.view.remove("README.md").unwrap();
    assert!(matches!(
        fx.view.resolve("README.md").unwrap(),
        CasResolution::Whiteout
    ));
    fx.view.prepare_created_file("README.md").unwrap();
    assert!(
        matches!(
            fx.view.resolve("README.md").unwrap(),
            CasResolution::CasFile
        ),
        "撤销 whiteout 后回落 manifest"
    );

    // NotFound → 空 upper 占位。
    fx.view.prepare_created_file("new/created.txt").unwrap();
    assert!(fx.upper_root.join("new/created.txt").is_file());
    assert!(fx.view.stat("new/created.txt").unwrap().is_some());
}

#[test]
fn rename_file_moves_content_and_records_whiteout() {
    let mut fx = fixture();
    fx.view.rename("README.md", "docs/moved.md").unwrap();
    // 源：manifest 有内容 → whiteout 隐藏。
    assert!(fx.view.stat("README.md").unwrap().is_none());
    // 目标：内容已搬移（hydrate 复制 + mtime 归一）。
    let st = fx.view.stat("docs/moved.md").unwrap().unwrap();
    assert_eq!(st.size, manifest_file(&fx, "README.md").size);
    assert_eq!(
        std::fs::symlink_metadata(fx.upper_root.join("docs/moved.md"))
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64,
        wtfs_cas::FIXED_STAT_EPOCH_UNIX_NS,
        "lower 经改名落 upper：mtime 归一"
    );
    // upper-only 文件改名：纯物理 rename，零标记。
    std::fs::write(fx.upper_root.join("docs/moved.md"), b"edited\n").unwrap();
    fx.view.rename("docs/moved.md", "docs/renamed.md").unwrap();
    assert_eq!(fx.view.read("docs/renamed.md").unwrap().unwrap(), b"edited\n");
    assert!(!fx.upper_root.join("docs/moved.md").exists());

    // 目标是目录 → 报错。
    assert!(fx.view.rename("docs/renamed.md", "docs").is_err());
    // NotFound 源 → 报错。
    assert!(fx.view.rename("no/such.txt", "x.txt").is_err());
}

#[test]
fn rename_directory_with_lower_content_is_exdev() {
    let mut fx = fixture();
    // manifest 有内容的目录 → EXDEV（M12 语义；VFS 映射 STATUS_NOT_SAME_DEVICE）。
    let err = fx.view.rename("docs", "docs2").unwrap_err();
    assert!(matches!(err, Error::CrossDevice(_)), "got: {err:?}");
    // upper-only 目录 → 纯物理改名。
    std::fs::create_dir_all(fx.upper_root.join("upper-only")).unwrap();
    std::fs::write(fx.upper_root.join("upper-only/f.txt"), b"u\n").unwrap();
    fx.view.rename("upper-only", "upper-only2").unwrap();
    assert!(fx.upper_root.join("upper-only2/f.txt").is_file());
    assert!(!fx.upper_root.join("upper-only").exists());
}

#[test]
fn delete_tracked_file_whiteout_survives_reopen_and_refresh_prunes() {
    let mut fx = fixture();
    fx.view.remove("README.md").unwrap();
    assert!(fx.view.stat("README.md").unwrap().is_none());

    // M12 标记日志：重开视图（重放日志）删除仍生效。
    let manifest = Arc::new(Manifest::load(&fx.repo, &fx.commit).unwrap());
    let reopened = CasView::open(
        CasViewConfig::new(
            &fx.upper_root,
            fx._dir.path().join("meta"),
        ),
        manifest,
        Arc::new(CatFile::new(&fx.repo)),
        Arc::new(HydrationCache::open(&fx.store)),
    )
    .unwrap();
    assert!(reopened.stat("README.md").unwrap().is_none(), "标记日志重放后仍隐藏");

    // refresh_head：HEAD 未动 → no-op。
    assert!(!fx.view.refresh_head(&fx.repo).unwrap());

    // HEAD 前进（提交一个新文件）：manifest 重载 + 失效 whiteout 剪枝——
    // README.md 未被提交删除（删除未提交）→ 标记保留。
    std::fs::write(fx.repo.join("added.txt"), b"added\n").unwrap();
    common::assert_git(&fx.repo, &["add", "added.txt"]);
    common::assert_git(&fx.repo, &["commit", "-m", "add file", "-q"]);
    assert!(fx.view.refresh_head(&fx.repo).unwrap(), "HEAD 前进需重载");
    assert_eq!(
        fx.view.manifest().file("added.txt").map(|f| f.size),
        Some(6),
        "新 manifest 可见"
    );
    assert!(fx.view.stat("README.md").unwrap().is_none(), "未提交的删除继续生效");

    // 提交删除：新 manifest 无 README.md → 标记剪枝（视图即新 HEAD）。
    common::assert_git(&fx.repo, &["rm", "-q", "README.md"]);
    common::assert_git(&fx.repo, &["commit", "-m", "remove file", "-q"]);
    assert!(fx.view.refresh_head(&fx.repo).unwrap());
    assert!(fx.view.manifest().file("README.md").is_none());
    assert!(fx.view.whites().is_empty(), "失效 whiteout 已剪枝");
}
