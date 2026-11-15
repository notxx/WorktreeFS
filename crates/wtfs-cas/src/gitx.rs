//! git 命令执行辅助（crate 内部使用，不对外承诺稳定）。
//!
//! 封装 `git -C <repo>` 子进程调用：manifest 装载（`ls-tree -r -l -z`）、
//! 索引种子（`read-tree` / `ls-files -z` → `update-index --stdin`）、
//! cat-file 常驻进程的拉起参数校验。stdout 保留原始字节（`-z` 输出含 NUL）。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::{Error, Result};

/// 一次 git 调用的结果；`stdout` 保留原始字节（`-z` 输出按 NUL 切分）。
pub(crate) struct GitOutput {
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl GitOutput {
    pub fn ok(&self) -> bool {
        self.status == Some(0)
    }

    pub fn stdout_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

/// `git -C <repo> <args>`；git 不在 PATH 时报 [`Error::Git`]。
pub(crate) fn run_git(repo: &Path, args: &[&str]) -> Result<GitOutput> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| Error::Git(format!("spawn git failed: {e}")))?;
    Ok(GitOutput {
        status: output.status.code(),
        stdout: output.stdout,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// `git -C <repo> <args>`，把 `input` 灌入 stdin（批量 `update-index --stdin`）。
pub(crate) fn run_git_with_stdin(repo: &Path, args: &[&str], input: &[u8]) -> Result<GitOutput> {
    use std::io::Write;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Git(format!("spawn git failed: {e}")))?;
    // 输入量级 = 路径列表（~KB/万文件），远小于管道缓冲，先写后读无死锁。
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(input)
            .map_err(|e| Error::Git(format!("write git stdin failed: {e}")))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| Error::Git(format!("wait git failed: {e}")))?;
    Ok(GitOutput {
        status: output.status.code(),
        stdout: output.stdout,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// 非 0 退出码转 [`Error::Git`]（带 stderr）。
pub(crate) fn require_ok(out: &GitOutput, what: &str) -> Result<()> {
    if out.ok() {
        Ok(())
    } else {
        Err(Error::Git(format!(
            "{what} failed (exit {:?}): {}",
            out.status,
            out.stderr.trim()
        )))
    }
}

/// 校验完整 40 位小写十六进制 sha1（MVP 仅支持 sha1 仓库）。
pub(crate) fn validate_oid(oid: &str) -> Result<()> {
    if oid.len() == 40 && oid.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(Error::NotFound(format!(
            "invalid object id {oid:?} (expected 40-char hex sha1)"
        )))
    }
}

/// 该 worktree/repo 的绝对 git dir（linked worktree 返回主仓库
/// `.git/worktrees/<name>`；`--absolute-git-dir` 需要 git ≥ 2.13）。
pub(crate) fn absolute_git_dir(repo: &Path) -> Result<PathBuf> {
    let out = run_git(repo, &["rev-parse", "--absolute-git-dir"])?;
    require_ok(&out, "git rev-parse --absolute-git-dir")?;
    let p = PathBuf::from(out.stdout_lossy().trim());
    if p.is_absolute() {
        Ok(p)
    } else {
        Err(Error::Git(format!("git dir not absolute: {p:?}")))
    }
}

/// 解析当前 HEAD commit（完整 sha）；manifest 刷新（M18）用它检测 HEAD
/// 前进。 unborn HEAD（空仓库）报 [`Error::Git`]。
pub(crate) fn head_commit(repo: &Path) -> Result<String> {
    let out = run_git(repo, &["rev-parse", "HEAD"])?;
    require_ok(&out, "git rev-parse HEAD")?;
    let s = out.stdout_lossy().trim().to_string();
    validate_oid(&s)?;
    Ok(s)
}
