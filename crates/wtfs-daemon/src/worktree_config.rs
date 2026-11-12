//! worktree git 配置写入（spec §6.3.5，M4 任务）。
//!
//! * `<common>/worktrees/<name>/config.worktree`：`core.checkStat=minimal`、
//!   `core.trustctime=false`、`core.untrackedCache=false`——index stat 与 FS
//!   固定 stat 对齐，并关闭 mtime/untracked 缓存，`git status` 不误报。
//! * `<common>/config`：`extensions.worktreeConfig = true`——git 只在扩展开启
//!   时读取 config.worktree（该键必须位于主仓库 common config）。
//!
//! 无 git CLI 依赖：直接做幂等的 INI 追加。git config 允许同名 section 重复
//! 出现（后者覆盖前者），因此“缺 key 就整段追加”始终安全；已有的自定义 key
//! 与其他 section 原样保留。

use std::io::Write as _;
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING};

/// config.worktree 必须包含的三项（key → 值）。
pub const REQUIRED_KEYS: [(&str, &str); 3] = [
    ("checkStat", "minimal"),
    ("trustctime", "false"),
    ("untrackedCache", "false"),
];

/// 确保 `gitdir/config.worktree`（gitdir = `<common>/worktrees/<name>`）
/// 含全部三项。幂等；返回是否发生了写入（新建或追加）。
pub fn write_worktree_config(gitdir: &Path) -> std::io::Result<bool> {
    let path = gitdir.join("config.worktree");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if has_all_required_keys(&existing) {
        return Ok(false);
    }
    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&core_section());
    write_atomic(&path, &content)?;
    Ok(true)
}

/// 确保 common config 开启 `extensions.worktreeConfig`。幂等；返回是否写入。
pub fn ensure_worktree_config_extension(common_config: &Path) -> std::io::Result<bool> {
    let existing = std::fs::read_to_string(common_config).unwrap_or_default();
    if contains_key(&existing, "extensions", "worktreeConfig") {
        return Ok(false);
    }
    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str("[extensions]\n\tworktreeConfig = true\n");
    write_atomic(common_config, &content)?;
    Ok(true)
}

/// 规范的 `[core]` 三项 section 文本。
fn core_section() -> String {
    let mut s = String::from("[core]\n");
    for (key, value) in REQUIRED_KEYS {
        s.push_str(&format!("\t{key} = {value}\n"));
    }
    s
}

/// 三个必需 key 是否都已出现（任意 section 下、大小写不敏感；M0 的 config.worktree
/// 只有 git 与本模块写入，跨 section 误判不会发生）。
fn has_all_required_keys(text: &str) -> bool {
    REQUIRED_KEYS
        .iter()
        .all(|(key, _)| contains_key_anywhere(text, key))
}

fn contains_key_anywhere(text: &str, key: &str) -> bool {
    text.lines().any(|line| line_has_key(line, key))
}

/// 行内是否为 `key = ...`（剥离 `;`/`#` 注释，key 大小写不敏感）。
fn line_has_key(line: &str, key: &str) -> bool {
    let code = strip_comment(line).trim();
    match code.split_once('=') {
        Some((k, _)) => k.trim().eq_ignore_ascii_case(key),
        None => false,
    }
}

/// `[section]`（含 `[section "sub"]`）内是否已有指定 key。
fn contains_key(text: &str, section: &str, key: &str) -> bool {
    let mut current: Option<String> = None;
    for line in text.lines() {
        let code = strip_comment(line).trim();
        if code.starts_with('[') {
            let name = code
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim();
            // section 名可能带子节：`extensions "media"` → 取首词。
            current = name
                .split_whitespace()
                .next()
                .map(|n| n.trim_matches('"').to_string());
        } else if current.as_deref() == Some(section) && line_has_key(code, key) {
            return true;
        }
    }
    false
}

fn strip_comment(line: &str) -> &str {
    match line.find([';', '#']) {
        Some(idx) => &line[..idx],
        None => line,
    }
}

/// 写临时文件后用 `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` 原子替换——
/// rename 语义直接覆盖目标，不存在“先删后改名”的中间丢文件窗口
/// （评审 P3；崩溃时最多残留 .wtfs-tmp 临时文件）。
fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("wtfs-tmp");
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(content.as_bytes())?;
        file.flush()?;
    }
    let from: Vec<u16> = tmp.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let replaced =
        unsafe { MoveFileExW(PCWSTR(from.as_ptr()), PCWSTR(to.as_ptr()), MOVEFILE_REPLACE_EXISTING) };
    if replaced.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn gitdir(dir: &Path) -> PathBuf {
        dir.join("worktrees").join("wt-1")
    }

    #[test]
    fn creates_canonical_config_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let git = gitdir(dir.path());
        std::fs::create_dir_all(&git).unwrap();

        assert!(write_worktree_config(&git).unwrap());
        let content = std::fs::read_to_string(git.join("config.worktree")).unwrap();
        for (key, value) in REQUIRED_KEYS {
            assert!(content.contains(&format!("{key} = {value}")), "missing {key}");
        }

        // 第二次调用不再写。
        assert!(!write_worktree_config(&git).unwrap());
        assert_eq!(
            std::fs::read_to_string(git.join("config.worktree")).unwrap(),
            content
        );
    }

    #[test]
    fn appends_missing_keys_and_preserves_existing() {
        let dir = tempfile::tempdir().unwrap();
        let git = gitdir(dir.path());
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("config.worktree"), "[core]\n\tbare = false\n").unwrap();

        write_worktree_config(&git).unwrap();
        let content = std::fs::read_to_string(git.join("config.worktree")).unwrap();
        assert!(content.contains("bare = false"), "已有 key 必须保留");
        for (key, value) in REQUIRED_KEYS {
            assert!(content.contains(&format!("{key} = {value}")));
        }
    }

    #[test]
    fn partial_keys_are_completed() {
        let dir = tempfile::tempdir().unwrap();
        let git = gitdir(dir.path());
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(
            git.join("config.worktree"),
            "; comment\n[core]\n\tcheckStat = minimal\n",
        )
        .unwrap();
        write_worktree_config(&git).unwrap();
        let content = std::fs::read_to_string(git.join("config.worktree")).unwrap();
        assert!(content.to_ascii_lowercase().contains("trustctime = false"));
        assert!(content.to_ascii_lowercase().contains("untrackedcache = false"));
    }

    #[test]
    fn extension_flag_written_once_and_scoped_to_section() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        assert!(ensure_worktree_config_extension(&config).unwrap());
        let content = std::fs::read_to_string(&config).unwrap();
        assert!(content.contains("worktreeConfig = true"));
        // 第二次调用幂等。
        assert!(!ensure_worktree_config_extension(&config).unwrap());

        // key 出现在别的 section 下不算数。
        let other = dir.path().join("config2");
        std::fs::write(&other, "[core]\n\tworktreeConfig = false\n").unwrap();
        assert!(ensure_worktree_config_extension(&other).unwrap());
        let content = std::fs::read_to_string(&other).unwrap();
        assert!(content.contains("[extensions]"));
    }

    #[test]
    fn extension_preserves_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        std::fs::write(
            &config,
            "[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\turl = x\n",
        )
        .unwrap();
        ensure_worktree_config_extension(&config).unwrap();
        let content = std::fs::read_to_string(&config).unwrap();
        assert!(content.contains("repositoryformatversion = 0"));
        assert!(content.contains("url = x"));
        assert!(content.contains("[extensions]"));
    }
}
