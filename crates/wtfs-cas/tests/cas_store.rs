//! manifest + cat-file + hydration 缓存的集成测试（真实 git 仓库）。

#![cfg(windows)]

mod common;

use std::sync::Arc;

use wtfs_cas::{CatFile, HydrationCache, Manifest};

#[test]
fn manifest_loads_demo_repo_tree() {
    let base = tempfile::tempdir().unwrap();
    let repo = common::init_demo_repo(base.path());
    let m = Manifest::load(&repo, "HEAD").unwrap();

    assert!(m.len() >= 10, "demo fixture has ≥10 files, got {}", m.len());
    assert!(m.file("README.md").is_some());
    assert!(m.file("src/lib.rs").is_some());
    let readme = m.file("README.md").unwrap();
    assert_eq!(readme.mode, wtfs_cas::MODE_REGULAR);
    assert!(readme.size > 0);
    assert_eq!(
        readme.oid, "3a622b1d64318483fd3377f29de6f7e8153b31fe",
        "demo repo README blob oid 固定"
    );
    assert!(m.has_dir("src"));
    assert!(m.has_dir("docs"));
    assert!(!m.has_dir("README.md"));
    assert_eq!(
        m.children_of("")
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![
            ".gitattributes",
            ".gitignore",
            "README.md",
            "data",
            "docs",
            "src"
        ]
    );
    assert!(m.total_size() > 0);
    assert!(!m.commit().is_empty() || m.commit() == "HEAD");
}

#[test]
fn manifest_loads_rich_repo_edge_cases() {
    let base = tempfile::tempdir().unwrap();
    let (repo, commit) = common::init_rich_repo(base.path());
    let m = Manifest::load(&repo, &commit).unwrap();

    // 可执行位
    assert_eq!(m.file("scripts/run.sh").unwrap().mode, wtfs_cas::MODE_EXEC);
    // 空文件：size 0
    let empty = m.file("empty.txt").unwrap();
    assert_eq!(empty.size, 0);
    assert_eq!(
        empty.oid, "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        "git 空 blob 固定 oid"
    );
    // symlink：mode 120000，size = 链接目标字节数
    let link = m.file("link.txt").unwrap();
    assert_eq!(link.mode, wtfs_cas::MODE_SYMLINK);
    assert_eq!(link.size, "../other/target\n".len() as u64);
    // gitlink：mode 160000，size 0（`-` 列）
    let gl = m.file("vendor/sub").unwrap();
    assert_eq!(gl.mode, wtfs_cas::MODE_GITLINK);
    assert_eq!(gl.size, 0);
    // 大小写同目录对：manifest 按字节原样区分
    assert!(m.file("casedir/Case.txt").is_some());
    assert!(m.file("casedir/case.txt").is_some());
    assert_ne!(
        m.file("casedir/Case.txt").unwrap().oid,
        m.file("casedir/case.txt").unwrap().oid
    );
    // 长路径：240+ 字符名 + 8 层目录
    let long = m
        .files()
        .keys()
        .find(|p| p.ends_with("l0123456789.txt"))
        .expect("long path entry");
    assert!(long.len() > 240, "long path total {}", long.len());
    assert_eq!(long.matches('/').count(), 9);
    assert_eq!(m.file(long).unwrap().size, 18);
}

#[test]
fn cat_file_reads_blobs_via_persistent_process() {
    let base = tempfile::tempdir().unwrap();
    let (repo, commit) = common::init_rich_repo(base.path());
    let m = Manifest::load(&repo, &commit).unwrap();
    let cat = CatFile::new(&repo);

    // 单进程连读全部 blob（含空文件；gitlink 无 blob，读报 NotFound）
    let before = cat.stats().requests;
    for (i, (path, f)) in m.files().iter().enumerate() {
        if f.mode == wtfs_cas::MODE_GITLINK {
            assert!(cat.read_blob(&f.oid).is_err(), "{path} gitlink 无 blob");
            continue;
        }
        let bytes = cat
            .read_blob(&f.oid)
            .unwrap_or_else(|e| panic!("{path}: {e}"));
        assert_eq!(bytes.len() as u64, f.size, "{path} size mismatch");
        if path == "empty.txt" {
            assert!(bytes.is_empty());
        }
        if path == "README.md" {
            assert!(bytes.starts_with(b"# "), "README content sanity");
        }
        if i == 0 {
            // 第一次读后进程已拉起，后续请求零进程启动
        }
    }
    let stats = cat.stats();
    assert_eq!(stats.requests, before + m.len() as u64);
    assert_eq!(stats.blobs, m.len() as u64 - 1, "gitlink 不产生 blob 读");
    assert_eq!(stats.missing, 1);
}

#[test]
fn cat_file_reports_missing_and_non_blob() {
    let base = tempfile::tempdir().unwrap();
    let repo = common::init_demo_repo(base.path());
    let cat = CatFile::new(&repo);

    // missing：40 位 hex 但对象不存在（exists + read 各计一次）
    let ghost = "0123456789abcdef0123456789abcdef01234567";
    assert!(!cat.exists(ghost).unwrap());
    assert!(cat.read_blob(ghost).is_err());
    assert_eq!(cat.stats().missing, 2);

    // 非 blob：commit oid
    let commit = common::assert_git(&repo, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    assert!(cat.exists(&commit).unwrap());
    let err = cat.read_blob(&commit).unwrap_err();
    assert!(err.to_string().contains("not a blob"), "got {err}");

    // 非法 oid 格式
    assert!(cat.read_blob("xyz").is_err());
    assert!(cat.read_blob("0123").is_err());

    // 坏管道自愈：missing 后继续读正常 blob（同管道复用）
    let readme = "3a622b1d64318483fd3377f29de6f7e8153b31fe";
    let bytes = cat.read_blob(readme).unwrap();
    assert!(!bytes.is_empty());
}

#[test]
fn cache_ensures_sharded_files_with_stats_and_gc() {
    let base = tempfile::tempdir().unwrap();
    let (repo, commit) = common::init_rich_repo(base.path());
    let m = Manifest::load(&repo, &commit).unwrap();
    let cat = CatFile::new(&repo);
    let store = tempfile::tempdir().unwrap();
    let cache = HydrationCache::open(store.path());

    let mut hydrated = 0u64;
    for (path, f) in m.files().iter() {
        if f.mode == wtfs_cas::MODE_GITLINK {
            continue; // gitlink 无 blob，不可 hydrate（视图读路径同报错）
        }
        let p = cache
            .ensure(&cat, &f.oid, f.size)
            .unwrap_or_else(|e| panic!("{path}: {e}"));
        assert!(p.starts_with(store.path().join("cas-cache")));
        // 分片 = oid 前 2 位
        let shard = p.parent().unwrap().file_name().unwrap().to_str().unwrap();
        assert_eq!(shard, &f.oid[..2]);
        assert_eq!(std::fs::read(&p).unwrap().len() as u64, f.size, "{path}");
        hydrated += 1;
    }
    let stats = cache.stats();
    assert_eq!(stats.misses, hydrated);
    assert_eq!(stats.hits, 0);
    assert_eq!(stats.hydrated_files, hydrated);
    assert_eq!(
        stats.hydrated_bytes,
        m.total_size() - m.file("vendor/sub").unwrap().size,
        "gitlink size 0 不参与"
    );

    // 全部命中：零 cat-file 请求、零重灌
    let cat_requests = cat.stats().requests;
    for (path, f) in m.files().iter() {
        if f.mode == wtfs_cas::MODE_GITLINK {
            continue;
        }
        let p = cache.ensure(&cat, &f.oid, f.size).unwrap();
        assert!(p.is_file(), "{path}");
    }
    assert_eq!(cat.stats().requests, cat_requests, "命中零对象读取");
    let stats = cache.stats();
    assert_eq!(stats.hits, hydrated);
    assert_eq!(stats.misses, hydrated);
    assert_eq!(
        cache.cached_bytes(),
        m.total_size() - m.file("vendor/sub").unwrap().size
    );

    // 大小不符 = 损坏 → 删除重灌
    let (path, f) = m.files().iter().find(|(_, f)| f.size > 0).unwrap();
    let p = cache.blob_path(&f.oid).unwrap();
    std::fs::write(&p, b"corrupted").unwrap();
    let hits_before = cache.stats().hits;
    let misses_before = cache.stats().misses;
    cache.ensure(&cat, &f.oid, f.size).unwrap();
    assert_eq!(
        std::fs::read(&p).unwrap().len() as u64,
        f.size,
        "{path} 自愈重灌"
    );
    assert_eq!(cache.stats().hits, hits_before, "损坏路径不计命中");
    assert_eq!(cache.stats().misses, misses_before + 1);

    // manifest size 与对象不符 → 报错且不留半截
    let err = cache.ensure(&cat, &f.oid, f.size + 999).unwrap_err();
    assert!(err.to_string().contains("size"), "got {err}");

    // GC：预算 0 → 全部逐出；.tmp-* 残留清扫。注意上一段 size 不符的
    // ensure 已把该文件删掉（损坏自愈路径），所以正式文件少一个。
    let tmp = cache
        .blob_path(&f.oid)
        .unwrap()
        .parent()
        .unwrap()
        .join(".tmp-1-1");
    std::fs::write(&tmp, b"torn").unwrap();
    let evicted_before = cache.stats().evicted_files;
    let report = cache.gc(0).unwrap();
    assert_eq!(report.removed_files, hydrated - 1);
    assert_eq!(report.removed_tmp, 1);
    assert_eq!(cache.cached_bytes(), 0);
    assert_eq!(
        cache.stats().evicted_files - evicted_before,
        report.removed_files + report.removed_tmp
    );
    // 预算内不逐出
    cache.ensure(&cat, &f.oid, f.size).unwrap();
    let report = cache.gc(u64::MAX).unwrap();
    assert_eq!(report.removed_files, 0);
    assert_eq!(cache.cached_bytes(), f.size);
}

#[test]
fn cat_file_concurrent_reads_share_process() {
    let base = tempfile::tempdir().unwrap();
    let repo = common::init_demo_repo(base.path());
    let cat = Arc::new(CatFile::new(&repo));
    let m = Arc::new(Manifest::load(&repo, "HEAD").unwrap());

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let cat = cat.clone();
            let m = m.clone();
            std::thread::spawn(move || {
                for (path, f) in m.files().iter() {
                    let bytes = cat
                        .read_blob(&f.oid)
                        .unwrap_or_else(|e| panic!("{path}: {e}"));
                    assert_eq!(bytes.len() as u64, f.size);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("thread");
    }
    let stats = cat.stats();
    assert_eq!(
        stats.blobs,
        (m.len() * 8) as u64,
        "demo 夹具无 gitlink，8 线程全量读"
    );
    assert_eq!(stats.missing, 0);
}
