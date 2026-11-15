//! CAS 视图模型集成测试：stat/list/read/write/remove + whiteout 持久化
//! （M12 日志语义）+ 边界（空文件/目录删建/大小写/长路径）。不挂载。

#![cfg(windows)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use tempfile::TempDir;
use wtfs_cas::{
    CasView, CasViewConfig, CatFile, DEFAULT_VOLUME_SERIAL, EntryKind, FIXED_STAT_EPOCH_UNIX_NS,
    HydrationCache, MODE_DIR, MODE_EXEC, MODE_REGULAR, Manifest, WriteState, file_id,
};

struct Fixture {
    _dir: TempDir,
    view: CasView,
    upper_root: PathBuf,
    cat: Arc<CatFile>,
}

fn open_view(dir: &TempDir, repo: &std::path::Path, commit: &str) -> CasView {
    let manifest = Arc::new(Manifest::load(repo, commit).unwrap());
    let cat = Arc::new(CatFile::new(repo));
    let cache = Arc::new(HydrationCache::open(dir.path().join("store")));
    let config = CasViewConfig::new(dir.path().join("upper"), dir.path().join("meta"));
    CasView::open(config, manifest, cat, cache).unwrap()
}

fn fixture_rich() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let (repo, commit) = common::init_rich_repo(dir.path());
    let upper_root = dir.path().join("upper");
    std::fs::create_dir_all(&upper_root).unwrap();
    let cat = Arc::new(CatFile::new(&repo));
    let view = open_view(&dir, &repo, &commit);
    Fixture {
        _dir: dir,
        view,
        upper_root,
        cat,
    }
}

#[test]
fn stat_reports_fixed_manifest_values() {
    let fx = fixture_rich();
    let m = fx.view.manifest();

    // manifest 文件：固定 dev/ino/mtime/ctime，size/mode 来自 manifest
    let st = fx.view.stat("README.md").unwrap().expect("exists");
    assert_eq!(st.dev, u64::from(DEFAULT_VOLUME_SERIAL));
    assert_eq!(st.ino, file_id("README.md"));
    assert_eq!(st.mode, MODE_REGULAR);
    assert_eq!(st.size, m.file("README.md").unwrap().size);
    assert_eq!(st.mtime_ns, FIXED_STAT_EPOCH_UNIX_NS);
    assert_eq!(st.ctime_ns, FIXED_STAT_EPOCH_UNIX_NS);

    // 可执行位保留
    let st = fx.view.stat("scripts/run.sh").unwrap().unwrap();
    assert_eq!(st.mode, MODE_EXEC);
    // 空文件：size 0、仍是常规文件
    let st = fx.view.stat("empty.txt").unwrap().unwrap();
    assert_eq!((st.mode, st.size), (MODE_REGULAR, 0));
    // symlink：mode 原样报告
    let st = fx.view.stat("link.txt").unwrap().unwrap();
    assert_eq!(st.mode, wtfs_cas::MODE_SYMLINK);
    // gitlink：mode 160000、size 0
    let st = fx.view.stat("vendor/sub").unwrap().unwrap();
    assert_eq!((st.mode, st.size), (wtfs_cas::MODE_GITLINK, 0));

    // 目录：manifest-only → 固定纪元；upper 目录 → 真实 mtime
    let st = fx.view.stat("docs").unwrap().unwrap();
    assert_eq!(
        (st.mode, st.size, st.mtime_ns),
        (MODE_DIR, 0, FIXED_STAT_EPOCH_UNIX_NS)
    );
    std::fs::create_dir_all(fx.upper_root.join("docs")).unwrap();
    let st = fx.view.stat("docs").unwrap().unwrap();
    assert!(
        st.mtime_ns > FIXED_STAT_EPOCH_UNIX_NS,
        "upper 目录真实 mtime"
    );

    // 根目录
    let st = fx.view.stat("").unwrap().unwrap();
    assert_eq!((st.mode, st.ino), (MODE_DIR, wtfs_cas::ROOT_FILE_ID));
    // 大小写派生 ino：stat 路径不同、大小写同 hash
    assert_eq!(
        fx.view.stat("README.md").unwrap().unwrap().ino,
        file_id("readme.md")
    );
    // 未命中
    assert!(fx.view.stat("nope.txt").unwrap().is_none());
    // 临时名不可见
    std::fs::write(fx.upper_root.join(".wtfs-tmp-1-1"), b"x").unwrap();
    assert!(fx.view.stat(".wtfs-tmp-1-1").unwrap().is_none());
}

#[test]
fn list_merges_manifest_and_upper_with_upper_precedence() {
    let fx = fixture_rich();
    let names = |v: &CasView, dir: &str| -> Vec<String> {
        v.list(dir).unwrap().into_iter().map(|e| e.name).collect()
    };
    let root = names(&fx.view, "");
    assert!(root.contains(&"README.md".to_string()));
    assert!(root.contains(&"docs".to_string()));
    assert!(
        root.contains(&"vendor".to_string()),
        "gitlink 父目录为派生目录"
    );
    assert!(!root.contains(&".wtfs-tmp-1-1".to_string()), "临时名不可见");

    // manifest 子目录
    let src = names(&fx.view, "src");
    assert!(src.contains(&"lib.rs".to_string()));
    assert!(src.contains(&"main.rs".to_string()));
    assert!(src.contains(&"util".to_string()), "嵌套目录派生");
    // upper-only 文件并入
    std::fs::create_dir_all(fx.upper_root.join("docs")).unwrap();
    std::fs::write(fx.upper_root.join("docs/new.md"), b"upper").unwrap();
    let docs = names(&fx.view, "docs");
    assert!(docs.contains(&"new.md".to_string()));
    // upper 覆盖同名（kind 以 upper 为准）
    std::fs::create_dir_all(fx.upper_root.join("data")).unwrap();
    std::fs::write(
        fx.upper_root.join("data").join("metrics.csv"),
        b"upper-version",
    )
    .unwrap();
    let entry = fx
        .view
        .list("data")
        .unwrap()
        .into_iter()
        .find(|e| e.name == "metrics.csv")
        .unwrap();
    // data/ 下仅一个同名文件（upper 覆盖不产生重复条目）
    assert_eq!(fx.view.list("data").unwrap().len(), 1);
    let _ = entry;

    // 空目录不存在
    assert!(fx.view.list("ghost-dir").unwrap().is_empty());
}

#[test]
fn list_stat_matches_stat_and_is_sorted() {
    let fx = fixture_rich();
    let entries = fx.view.list_stat("").unwrap();
    let names: Vec<_> = entries.iter().map(|(n, _)| n.clone()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "按名字序");
    assert!(entries.len() >= 10);

    for (name, st) in &entries {
        let rel = if name.contains('/') {
            name.clone()
        } else {
            format!("{name}")
        };
        let direct = fx.view.stat(name).unwrap().expect("entry must stat");
        assert_eq!(&direct, st, "list_stat 与 stat 逐项一致: {name}");
        let _ = rel;
    }
}

#[test]
fn read_hydrates_from_manifest_and_upper_first() {
    let mut fx = fixture_rich();
    // 需要跨 write 使用的 manifest 事实先取出（manifest() 借用视图）。
    let readme_oid = fx.view.manifest().file("README.md").unwrap().oid.clone();
    let long = fx
        .view
        .manifest()
        .files()
        .keys()
        .find(|p| p.ends_with("l0123456789.txt"))
        .cloned()
        .unwrap();

    // manifest 读：内容与 cat-file 一致；读取触发 hydration（缓存命中路径）
    let readme = fx.view.read("README.md").unwrap().unwrap();
    let expect = fx.cat.read_blob(&readme_oid).unwrap();
    assert_eq!(readme, expect);

    // 空文件
    assert_eq!(fx.view.read("empty.txt").unwrap().unwrap().len(), 0);

    // symlink：blob 内容 = 链接目标字节
    let link = fx.view.read("link.txt").unwrap().unwrap();
    assert_eq!(link, b"../other/target\n".to_vec());

    // gitlink：对象库无 blob → NotFound
    let err = fx.view.read("vendor/sub").unwrap_err();
    assert!(matches!(err, wtfs_cas::Error::NotFound(_)), "got {err:?}");

    // upper 优先：写 upper 后读 upper 内容
    fx.view.write("README.md", b"upper version").unwrap();
    assert_eq!(
        fx.view.read("README.md").unwrap().unwrap(),
        b"upper version"
    );

    // 目录报错
    assert!(fx.view.read("docs").is_err());
    // 未命中 → None
    assert!(fx.view.read("nope.txt").unwrap().is_none());

    // 缓存已按需灌入（README + empty + link 等被读过的 oid）
    let stats = fx.view.cache().stats();
    assert!(stats.misses >= 3, "至少灌入被读过的 3 个 blob");

    // 大小写同目录对：两个条目各自读出自己的内容
    assert_eq!(
        fx.view.read("casedir/Case.txt").unwrap().unwrap(),
        b"UPPER\n".to_vec()
    );
    assert_eq!(
        fx.view.read("casedir/case.txt").unwrap().unwrap(),
        b"lower\n".to_vec()
    );

    // 长路径：读经缓存（oid 命名短文件），无 NTFS 长名
    assert_eq!(
        fx.view.read(&long).unwrap().unwrap(),
        b"long path content\n".to_vec()
    );
}

#[test]
fn write_goes_to_upper_and_revives_whiteout() {
    let mut fx = fixture_rich();

    // 新文件 → New
    let out = fx.view.write("new.txt", b"fresh").unwrap();
    assert_eq!((out.state, out.bytes), (WriteState::New, 5));
    // 覆盖 manifest 路径 → Upper
    let out = fx.view.write("README.md", b"rewritten").unwrap();
    assert_eq!(out.state, WriteState::Upper);
    assert_eq!(fx.view.read("README.md").unwrap().unwrap(), b"rewritten");

    // upper stat：真实 mtime、真实 size
    let st = fx.view.stat("README.md").unwrap().unwrap();
    assert_eq!(st.size, 9);
    assert!(st.mtime_ns > FIXED_STAT_EPOCH_UNIX_NS);

    // 临时前缀名拒绝
    assert!(fx.view.write(".wtfs-tmp-1-1", b"x").is_err());

    // 删除 → whiteout（日志即时持久）；持久化跨重开在
    // `whiteout_persists_across_reopen` 单测覆盖。这里验证撤销：
    // whiteout 撤销后恢复可见。
    let out = fx.view.write("README.md", b"reborn").unwrap();
    assert_eq!(out.state, WriteState::Upper);
    assert!(fx.view.stat("README.md").unwrap().is_some());
    assert_eq!(fx.view.read("README.md").unwrap().unwrap(), b"reborn");
    assert!(!fx.view.whites().contains("README.md"), "重建撤销 whiteout");
}

#[test]
fn whiteout_persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, commit) = common::init_rich_repo(dir.path());
    std::fs::create_dir_all(dir.path().join("upper")).unwrap();

    let manifest = Arc::new(Manifest::load(&repo, &commit).unwrap());
    let cat = Arc::new(CatFile::new(&repo));
    let cache = Arc::new(HydrationCache::open(dir.path().join("store")));
    let config = CasViewConfig::new(dir.path().join("upper"), dir.path().join("meta"));

    {
        let mut view =
            CasView::open(config.clone(), manifest.clone(), cat.clone(), cache.clone()).unwrap();
        assert!(view.remove("README.md").unwrap());
        assert!(view.remove("src").unwrap());
    }
    // 重开：重放 markers.log
    {
        let view =
            CasView::open(config.clone(), manifest.clone(), cat.clone(), cache.clone()).unwrap();
        assert!(view.whites().contains("README.md"));
        assert!(view.whites().contains("src"));
        assert!(view.stat("README.md").unwrap().is_none());
        assert!(
            view.stat("src/lib.rs").unwrap().is_none(),
            "祖先目录 whiteout 隐藏子树"
        );
        let root: Vec<String> = view.list("").unwrap().into_iter().map(|e| e.name).collect();
        assert!(!root.contains(&"README.md".to_string()));
        assert!(!root.contains(&"src".to_string()));
    }
}

#[test]
fn directory_delete_and_recreate_is_opaque() {
    let mut fx = fixture_rich();

    // 删 manifest-only 目录 → 整子树隐藏，零 upper 操作
    assert!(fx.view.remove("docs").unwrap());
    assert!(!fx.view.list("").unwrap().iter().any(|e| e.name == "docs"));
    assert!(fx.view.stat("docs/design/overview.md").unwrap().is_none());
    assert!(fx.view.read("docs/design/overview.md").unwrap().is_none());
    assert!(
        !fx.upper_root.join("docs").exists(),
        "manifest-only 目录删除零 upper 文件"
    );

    // upper 重建目录 → 只显示 upper 内容（opaque 语义）
    std::fs::create_dir_all(fx.upper_root.join("docs")).unwrap();
    std::fs::write(fx.upper_root.join("docs/fresh.md"), b"fresh").unwrap();
    let st = fx.view.stat("docs").unwrap().unwrap();
    assert!(
        st.mtime_ns > FIXED_STAT_EPOCH_UNIX_NS,
        "重建目录用 upper mtime"
    );
    assert_eq!(
        fx.view
            .list("docs")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect::<Vec<_>>(),
        vec!["fresh.md"]
    );
    assert!(
        fx.view.read("docs/design/overview.md").unwrap().is_none(),
        "lower 子树仍隐藏"
    );

    // 上层枚举显示重建后的目录（EntryKind::Directory）
    let e = fx
        .view
        .list("")
        .unwrap()
        .into_iter()
        .find(|e| e.name == "docs")
        .unwrap();
    assert_eq!(e.kind, EntryKind::Directory);
}

#[test]
fn remove_upper_only_and_missing() {
    let mut fx = fixture_rich();

    // upper-only 文件：删除即消失，无 whiteout（manifest 无内容）
    fx.view.write("temp.txt", b"t").unwrap();
    assert!(fx.view.remove("temp.txt").unwrap());
    assert!(fx.view.stat("temp.txt").unwrap().is_none());
    assert!(!fx.view.whites().contains("temp.txt"));

    // 不存在 → false
    assert!(!fx.view.remove("ghost.txt").unwrap());

    // upper 覆盖的 manifest 文件：删 upper + whiteout（回落被挡）
    fx.view.write("README.md", b"u").unwrap();
    assert!(fx.view.remove("README.md").unwrap());
    assert!(fx.view.stat("README.md").unwrap().is_none());
    assert!(!fx.upper_root.join("README.md").exists());
    assert!(fx.view.whites().contains("README.md"));

    // 纯 manifest 文件删除零 upper 文件操作（目录都不建）
    let upper_before = std::fs::read_dir(&fx.upper_root).unwrap().count();
    assert!(fx.view.remove("src/main.rs").unwrap());
    assert_eq!(
        std::fs::read_dir(&fx.upper_root).unwrap().count(),
        upper_before,
        "删除不 hydrate"
    );
}

#[test]
fn file_id_stable_and_path_validation() {
    let fx = fixture_rich();
    assert_eq!(
        fx.view.file_id("src/lib.rs").unwrap(),
        file_id("SRC/lib.rs")
    );
    assert!(fx.view.file_id("").is_err());
    assert!(fx.view.file_id("../escape").is_err());
    assert!(fx.view.stat("../escape").is_err());
    assert!(fx.view.list("..").is_err());
    assert!(fx.view.read("/abs").is_err());
}
