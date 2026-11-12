//! git 命令执行辅助：模板创建/解析专用（crate 内部使用，不对外承诺稳定）。
//!
//! spec §6.2：模板 = 正常 checkout + `update-index --refresh` + index 快照；
//! 本模块只封装子进程调用与少量 rev-parse/ls-files 解析。

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Error, Result};

/// 一次 git 调用的结果；`stdout` 保留原始字节（`ls-files -z` 需要按字节切分）。
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

/// `git -C <repo> <args>`；git 不在 PATH 时报 [`Error::Template`]。
pub(crate) fn run_git(repo: &Path, args: &[&str]) -> Result<GitOutput> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| Error::Template(format!("spawn git failed: {e}")))?;
    Ok(GitOutput {
        status: output.status.code(),
        stdout: output.stdout,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// 非 0 退出码转 [`Error::Template`]（带 stderr）。
pub(crate) fn require_ok(out: &GitOutput, what: &str) -> Result<()> {
    if out.ok() {
        Ok(())
    } else {
        Err(Error::Template(format!(
            "{what} failed (exit {:?}): {}",
            out.status,
            out.stderr.trim()
        )))
    }
}

/// 把 ref（branch / tag / 裸 sha）解析为完整 commit sha。
pub(crate) fn rev_parse_commit(repo: &Path, base_ref: &str) -> Result<String> {
    let out = run_git(
        repo,
        &["rev-parse", "--verify", &format!("{base_ref}^{{commit}}")],
    )?;
    require_ok(&out, "git rev-parse")?;
    let sha = out.stdout_lossy().trim().to_string();
    if sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(sha)
    } else {
        Err(Error::Template(format!(
            "unexpected rev-parse output for {base_ref}: {sha:?}"
        )))
    }
}

/// 主仓库公共 git 目录（`<repo>/.git`）；linked worktree 场景返回绝对路径。
pub(crate) fn git_common_dir(repo: &Path) -> Result<PathBuf> {
    let out = run_git(repo, &["rev-parse", "--git-common-dir"])?;
    require_ok(&out, "git rev-parse --git-common-dir")?;
    let p = PathBuf::from(out.stdout_lossy().trim());
    if p.is_absolute() {
        Ok(p)
    } else {
        Ok(repo.join(p))
    }
}

/// linked worktree（模板树）的私有 gitdir，index 位于 `<gitdir>/index`。
pub(crate) fn worktree_git_dir(tree: &Path) -> Result<PathBuf> {
    let out = run_git(tree, &["rev-parse", "--git-dir"])?;
    require_ok(&out, "git rev-parse --git-dir")?;
    let p = PathBuf::from(out.stdout_lossy().trim());
    if p.is_absolute() {
        Ok(p)
    } else {
        Ok(tree.join(p))
    }
}

/// 传给 git 的路径字符串：统一正斜杠。
pub(crate) fn git_path(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}
