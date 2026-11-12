//! wtfsd 配置（spec §5.3/§8.1）：路径布局、轮询节奏与 gc 参数。
//!
//! 存储布局（spec §8.1）：`%LOCALAPPDATA%\wtfs\repos\<repo-id>\`（模板与
//! overlay 元数据）与 `%LOCALAPPDATA%\wtfs\logs\`。`<repo-id>` 取规范化仓库
//! 路径 blake3 的前 16 hex，保证文件系统安全且跨进程稳定。

use std::path::{Path, PathBuf};
use std::time::Duration;

use wtfs_core::FileHash;

/// 默认控制管道名（spec §5.3：`\\.\pipe\wtfs`）。
pub const DEFAULT_PIPE_NAME: &str = r"\\.\pipe\wtfs";

/// index watcher 默认轮询周期（毫秒）。
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 250;

/// gc 认定“旧模板”的默认年龄（天，spec §6.6）。
pub const DEFAULT_TEMPLATE_MAX_AGE_DAYS: u64 = 7;

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// 主仓库根（绝对路径；模板 resolve 与 state.json 的基准）。
    pub repo_root: PathBuf,
    /// overlay/模板存储根（spec §8.1 `<repo-id>/` 目录）。
    pub store_root: PathBuf,
    /// 挂载点（M0 固定 `<repo>/.tower/worktrees`，spec §6.1）。
    pub mount_point: PathBuf,
    /// 控制管道名。
    pub pipe_name: String,
    /// index watcher 轮询周期。
    pub poll_interval: Duration,
    /// gc 旧模板阈值。
    pub template_max_age: Duration,
    /// dedupe 开关（关闭 = 全量 copy 兜底，spec §6.3.6）。
    pub dedupe_enabled: bool,
    /// 日志目录（spec §8.1 `wtfs/logs`）。
    pub log_dir: PathBuf,
    /// RUST_LOG 未设置时的默认级别。
    pub log_level: String,
}

impl DaemonConfig {
    /// 以仓库根派生完整配置；`wtfsd`/CLI 在此基础上按命令行覆盖。
    pub fn for_repo(repo_root: impl Into<PathBuf>) -> Self {
        let repo_root = to_absolute(repo_root.into());
        let local = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                // 环境异常的兜底：落到仓库本地目录，功能不受影响。
                repo_root.join(".wtfs-store")
            });
        let wtfs_root = local.join("wtfs");
        Self {
            store_root: wtfs_root.join("repos").join(repo_id(&repo_root)),
            mount_point: repo_root.join(".tower").join("worktrees"),
            log_dir: wtfs_root.join("logs"),
            pipe_name: DEFAULT_PIPE_NAME.to_string(),
            poll_interval: Duration::from_millis(DEFAULT_POLL_INTERVAL_MS),
            template_max_age: Duration::from_secs(DEFAULT_TEMPLATE_MAX_AGE_DAYS * 24 * 3600),
            dedupe_enabled: true,
            repo_root,
            log_level: "info".to_string(),
        }
    }

    /// 主仓库公共 git 目录（linked worktree 元数据所在；spec §6.3 的
    /// `<main>/.git`）。daemon 启动时会先用 `git rev-parse --git-common-dir`
    /// 探测修正，此处只是无子进程开销的缺省值。
    pub fn common_git_dir(&self) -> PathBuf {
        self.repo_root.join(".git")
    }
}

/// 仓库稳定短 id：blake3(规范化路径) 前 16 hex（store 目录名）。
pub fn repo_id(repo_root: &Path) -> String {
    let hex = FileHash::of_bytes(normalize_root(repo_root).as_bytes()).to_hex();
    hex[..16].to_string()
}

/// 与 wtfs-core `normalize_root` 同规则：统一分隔符、去尾斜杠、盘符小写。
/// （wtfs-core 的实现是 `pub(crate)`，daemon 侧复制这一小段纯函数。）
pub fn normalize_root(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/');
    let mut owned = s.to_string();
    if owned.len() >= 2 && owned.as_bytes()[1] == b':' {
        owned[..1].make_ascii_lowercase();
    }
    owned
}

/// 相对路径按当前目录补全（不引入 `\\?\` UNC 前缀，避免干扰 git 与显示）。
pub fn to_absolute(p: PathBuf) -> PathBuf {
    if p.is_absolute() {
        p
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_id_is_stable_and_safe() {
        let id = repo_id(Path::new(r"C:\Users\notxx\Documents\workspace\dashboard"));
        let again = repo_id(Path::new("c:/Users/notxx/Documents/workspace/dashboard"));
        assert_eq!(id, again, "分隔符/盘符大小写需归一");
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn normalize_root_unifies_separators() {
        assert_eq!(normalize_root(Path::new("C:\\a\\b\\")), "c:/a/b");
        assert_eq!(normalize_root(Path::new("d:/x")), "d:/x");
    }

    #[test]
    fn for_repo_derives_spec_layout() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = DaemonConfig::for_repo(dir.path());
        assert_eq!(
            cfg.mount_point,
            dir.path().join(".tower").join("worktrees")
        );
        assert_eq!(cfg.common_git_dir(), dir.path().join(".git"));
        assert!(cfg.store_root.starts_with(local_wtfs_root().join("repos")));
        assert_eq!(cfg.pipe_name, DEFAULT_PIPE_NAME);
        assert!(cfg.dedupe_enabled);
    }

    fn local_wtfs_root() -> PathBuf {
        let local = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir());
        local.join("wtfs")
    }
}
