//! 集成测试共享夹具：demo-repo 复制 + git plumbing 富夹具构建。
//! 每个测试目标单独编译本模块，未用到的辅助函数按 dead_code 静默。
#![allow(dead_code)]
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::Command;

pub fn git(repo: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("git")
        .arg("-c")
        .arg("user.name=wtfs-cas-test")
        .arg("-c")
        .arg("user.email=wtfs-cas-test@example.com")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git CLI must be available");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

pub fn assert_git(repo: &Path, args: &[&str]) -> String {
    let (ok, out) = git(repo, args);
    assert!(ok, "git {args:?} failed: {out}");
    out
}

pub fn git_stdin(repo: &Path, args: &[&str], input: &[u8]) -> (bool, String) {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input)
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait git");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// 把 `tests/fixtures/demo-repo` 复制到 `<base>/repo` 并 init + 首次提交
/// （与 wtfs-daemon 测试同一做法，不碰原夹具）。
pub fn init_demo_repo(base: &Path) -> PathBuf {
    let repo = base.join("repo");
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("demo-repo")
        .canonicalize()
        .expect("fixture dir must exist");
    copy_tree(&src, &repo);
    assert_git(&repo, &["init", "-b", "master"]);
    assert_git(&repo, &["config", "core.autocrlf", "false"]);
    assert_git(&repo, &["config", "core.longpaths", "true"]);
    assert_git(&repo, &["add", "-A"]);
    assert_git(&repo, &["commit", "-m", "demo fixture", "-q"]);
    repo
}

fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create copy root");
    for entry in std::fs::read_dir(src).expect("read source") {
        let entry = entry.expect("source entry");
        let file_type = entry.file_type().expect("file type");
        let target = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy file");
        }
    }
}

/// 富夹具：常规文件 + 可执行位 + 空文件 + symlink + gitlink + 大小写同目录
/// 对 + 240 字符长路径。长路径 blob 经 hash-object plumbing 注入，不落
/// NTFS 长文件名。返回（repo 路径，commit sha）。
pub fn init_rich_repo(base: &Path) -> (PathBuf, String) {
    let repo = init_demo_repo(base);
    // 可执行位（demo-repo 无执行文件）
    std::fs::create_dir_all(repo.join("scripts")).unwrap();
    std::fs::write(repo.join("scripts/run.sh"), b"#!/bin/sh\necho run\n").unwrap();
    assert_git(&repo, &["add", "scripts/run.sh"]);
    assert_git(&repo, &["update-index", "--chmod=+x", "scripts/run.sh"]);
    // 空文件
    std::fs::write(repo.join("empty.txt"), b"").unwrap();
    assert_git(&repo, &["add", "empty.txt"]);

    // symlink（120000）与 gitlink（160000）走 plumbing，不依赖 NTFS symlink
    let link_oid = {
        let (ok, out) = git_stdin(
            &repo,
            &["hash-object", "-w", "--stdin"],
            b"../other/target\n",
        );
        assert!(ok, "hash-object link: {out}");
        out.trim().to_string()
    };
    assert_git(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("120000,{link_oid},link.txt"),
        ],
    );
    let gitlink_target = "0123456789abcdef0123456789abcdef01234567";
    assert_git(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{gitlink_target},vendor/sub"),
        ],
    );

    // 大小写同目录对（git 命名空间合法，NTFS 同目录只能存其一——视图必须
    // 按 manifest 字节原样区分）
    let upper = {
        let (ok, out) = git_stdin(&repo, &["hash-object", "-w", "--stdin"], b"UPPER\n");
        assert!(ok, "hash-object upper: {out}");
        out.trim().to_string()
    };
    let lower = {
        let (ok, out) = git_stdin(&repo, &["hash-object", "-w", "--stdin"], b"lower\n");
        assert!(ok, "hash-object lower: {out}");
        out.trim().to_string()
    };
    assert_git(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{upper},casedir/Case.txt"),
        ],
    );
    assert_git(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{lower},casedir/case.txt"),
        ],
    );

    // 长路径：8 层目录 + 240 字符文件名（plumbing 注入）
    let long_name = format!("{}{}.txt", "l".repeat(230), "0123456789");
    let long_path = format!(
        "deep/{}/{}",
        "d/".repeat(8).trim_end_matches('/'),
        long_name
    );
    let long_content = b"long path content\n".to_vec();
    let long_oid = {
        let (ok, out) = git_stdin(&repo, &["hash-object", "-w", "--stdin"], &long_content);
        assert!(ok, "hash-object long: {out}");
        out.trim().to_string()
    };
    assert_git(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{long_oid},{long_path}"),
        ],
    );

    let tree = assert_git(&repo, &["write-tree"]).trim().to_string();
    let commit = {
        let out = assert_git(&repo, &["commit-tree", &tree, "-m", "rich fixture"]);
        out.trim().to_string()
    };
    assert_git(&repo, &["update-ref", "refs/heads/rich", &commit]);
    (repo, commit)
}

/// `git status --porcelain` 输出行数（空 = 干净）。
pub fn status_lines(repo: &Path) -> usize {
    let out = assert_git(repo, &["status", "--porcelain"]);
    out.lines().count()
}
