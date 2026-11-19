//! `wtfs` 命令行（spec §5.4）。
//!
//! 命令面（M0/dedupe-only）：`mount` / `unmount` / `template create|update` /
//! `gc [--dry-run]` / `status`。`mount` 拉起同目录的 `wtfsd.exe`（detached）
//! 并等待就绪；`unmount/status/gc` 经控制管道 `\\.\pipe\wtfs` 与 daemon 通信，
//! daemon 不在时 `gc` 可离线直连存储执行（挂载中则拒绝，防误删 FS 视图）。
//! `template create/update` 直接驱动 wtfs-core 的模板存储。
//!
//! 实现放在 lib（`src/main.rs` 只是薄入口）：`tests/` 下的 B070100 回归
//! 测试需要直接驱动 [`spawn_detached`] 的生产 spawn 路径。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context};
use clap::Parser;
use wtfs_core::{SqliteTemplateStore, TemplateSpec, TemplateStore};
use wtfs_daemon::config::{self, DaemonConfig, DEFAULT_PIPE_NAME};
use wtfs_daemon::gc::{self, GcContext};
use wtfs_daemon::pipe::{self, PipeRequest};
use wtfs_daemon::{StatusReport, DEFAULT_TEMPLATE_MAX_AGE_DAYS};

/// wtfsd 启动后等待挂载就绪的默认时长。
const MOUNT_WAIT_SECS_DEFAULT: u64 = 30;
/// 管道默认超时。
const PIPE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Parser)]
#[command(
    name = "wtfs",
    version,
    about = "WorktreeFS —— Tower worktree 的 COW overlay 文件系统"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,

    /// 仓库根（mount/template/gc 离线模式必需；status/unmount 可省略）。
    #[arg(long, global = true)]
    repo: Option<PathBuf>,

    /// overlay/模板存储根（默认 %LOCALAPPDATA%\wtfs\repos\<repo-id>）。
    #[arg(long, global = true)]
    store_root: Option<PathBuf>,

    /// 控制管道名（默认 \\.\pipe\wtfs）。
    #[arg(long, global = true)]
    pipe: Option<String>,

    /// 原样打印协议 JSON。
    #[arg(long, global = true)]
    json: bool,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// 挂载 <repo>/.tower/worktrees 并启动 daemon（wtfsd）。
    Mount {
        /// 模板基线 ref（缺模板时先创建，spec §6.2）。
        #[arg(long)]
        base: Option<String>,
        /// 等待挂载就绪的秒数。
        #[arg(long, default_value_t = MOUNT_WAIT_SECS_DEFAULT)]
        wait_secs: u64,
    },
    /// 探测当前项目是否可交给 wtfs 后端（不挂载、不改文件）。
    ///
    /// `--repo` 省略时从当前目录向上找 git 根；输出 JSON（`--json`）或
    /// 人读文本。Kimi Code 的 worktree backend 用它决定走 wtfs 还是 git。
    Detect,
    /// 停止 daemon 并卸载。
    Unmount,
    /// daemon 与 worktree 状态。
    Status,
    /// 模板管理。
    Template {
        #[command(subcommand)]
        command: TemplateCmd,
    },
    /// 清理 dead 目录 / 孤儿 upper / 旧模板；daemon 在线则经管道执行。
    Gc {
        /// 只打印计划，不删除。
        #[arg(long)]
        dry_run: bool,
        /// 旧模板年龄阈值（天）。
        #[arg(long, default_value_t = DEFAULT_TEMPLATE_MAX_AGE_DAYS)]
        max_age_days: u64,
    },
}

#[derive(clap::Subcommand)]
enum TemplateCmd {
    /// 创建模板（已存在则幂等返回）。
    Create {
        /// 模板基线 ref（branch/tag/sha）。
        #[arg(long)]
        base: String,
    },
    /// 更新模板到 base 最新 commit。
    Update {
        #[arg(long)]
        base: String,
    },
}

/// CLI 入口：解析 argv 并执行（main.rs 薄壳调用）。
pub fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    run_parsed(cli)
}

fn run_parsed(cli: Cli) -> anyhow::Result<()> {
    let pipe_name = cli.pipe.clone().unwrap_or_else(|| DEFAULT_PIPE_NAME.to_string());
    match cli.command {
        Cmd::Mount { base, wait_secs } => {
            let repo = require_repo(cli.repo.as_ref())?;
            let mut cfg = DaemonConfig::for_repo(&repo);
            if let Some(store_root) = &cli.store_root {
                cfg.store_root = store_root.clone();
            }
            cfg.pipe_name = pipe_name.clone();
            cmd_mount(&cfg, base.as_deref(), wait_secs, cli.json)
        }
        Cmd::Detect => cmd_detect(cli.repo.as_deref(), cli.store_root.as_deref(), cli.json),
        Cmd::Unmount => cmd_unmount(&pipe_name, cli.json),
        Cmd::Status => cmd_status(&pipe_name, cli.json),
        Cmd::Template { command } => {
            let repo = require_repo(cli.repo.as_ref())?;
            let (base, updating) = match command {
                TemplateCmd::Create { base } => (base, false),
                TemplateCmd::Update { base } => (base, true),
            };
            cmd_template(&repo, &base, cli.store_root.as_deref(), updating)
        }
        Cmd::Gc { dry_run, max_age_days } => {
            cmd_gc(&pipe_name, cli.repo.as_ref(), cli.store_root.as_deref(), dry_run, max_age_days, cli.json)
        }
    }
}

fn require_repo(repo: Option<&PathBuf>) -> anyhow::Result<PathBuf> {
    let repo = repo.with_context(|| "--repo <path> is required")?;
    let repo = config::to_absolute(repo.clone());
    if !repo.is_dir() {
        bail!("repo root not found: {}", repo.display());
    }
    Ok(repo)
}

/// `wtfs detect`：只读探测，不 mount、不创建目录。
///
/// 仓库路径来源：`--repo`，缺省用当前目录；如果当前目录在 linked worktree
/// 内，用 `git rev-parse --git-common-dir` 找到主仓库根，再检查主仓库的
/// `.tower/`（Tower workspace）。输出给 Kimi Code 的 worktree backend 用。
fn cmd_detect(
    repo: Option<&Path>,
    store_root: Option<&Path>,
    as_json: bool,
) -> anyhow::Result<()> {
    let cwd = match repo {
        Some(p) => config::to_absolute(p.to_path_buf()),
        None => std::env::current_dir().context("current dir unavailable")?,
    };

    let mut supported = false;
    let mut reason: Option<&str> = None;
    let mut repo_root: Option<PathBuf> = None;
    let mut common_git_dir: Option<PathBuf> = None;
    let mut tower_dir: Option<PathBuf> = None;
    let mut store: Option<PathBuf> = store_root.map(Path::to_path_buf);
    let mut mount_point: Option<PathBuf> = None;

    if !cfg!(windows) {
        reason = Some("not-windows");
    } else if !cwd.is_dir() {
        reason = Some("repo-path-not-found");
    } else {
        let top = git_output(&cwd, &["rev-parse", "--show-toplevel"]).ok();
        let common = git_output(
            &cwd,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .ok()
        .or_else(|| git_output(&cwd, &["rev-parse", "--git-common-dir"]).ok());

        match (top, common) {
            (Some(top), Some(common)) => {
                let top = config::to_absolute(PathBuf::from(top.trim()));
                let common = PathBuf::from(common.trim());
                let common = if common.is_absolute() {
                    common
                } else {
                    config::to_absolute(top.join(common))
                };
                let main_root = if common
                    .file_name()
                    .is_some_and(|name| name == ".git")
                {
                    common
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| top.clone())
                } else {
                    top.clone()
                };
                let tower = main_root.join(".tower");
                repo_root = Some(main_root.clone());
                common_git_dir = Some(common);
                tower_dir = Some(tower.clone());
                mount_point = Some(tower.join("worktrees"));
                if store.is_none() {
                    store = Some(DaemonConfig::for_repo(&main_root).store_root);
                }
                // `.tower` 可能是基准/临时目录残留；真正的 Tower workspace
                // 至少有 comms（state.json）或 worktrees 挂载点。
                if tower.join("comms").is_dir() || tower.join("worktrees").is_dir() {
                    supported = true;
                } else {
                    reason = Some("tower-workspace-missing");
                }
            }
            _ => reason = Some("not-a-git-repo"),
        }
    }

    let out = serde_json::json!({
        "supported": supported,
        "reason": reason,
        "repo_root": repo_root.as_ref().map(|p| p.display().to_string()),
        "git_common_dir": common_git_dir.as_ref().map(|p| p.display().to_string()),
        "tower_dir": tower_dir.as_ref().map(|p| p.display().to_string()),
        "store_root": store.as_ref().map(|p| p.display().to_string()),
        "mount_point": mount_point.as_ref().map(|p| p.display().to_string()),
        "version": env!("CARGO_PKG_VERSION"),
    });

    if as_json {
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else if supported {
        println!(
            "supported: wtfs\nrepo: {}\nstore: {}\nmount: {}",
            out["repo_root"].as_str().unwrap_or("-"),
            out["store_root"].as_str().unwrap_or("-"),
            out["mount_point"].as_str().unwrap_or("-"),
        );
    } else {
        println!(
            "supported: git\nreason: {}",
            reason.unwrap_or("unknown")
        );
    }
    Ok(())
}

/// 在 `dir` 下跑只读 git 命令，成功返回 stdout。
fn git_output(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| format!("git {args:?} failed"))?;
    if !out.status.success() {
        bail!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

// ---- mount / unmount / status ----

/// 拉起 detached 守护进程（B070100）。
///
/// 仅 `Stdio::null()` 切不断句柄泄漏：Windows 上 Rust 的 `Command` 经
/// `CreateProcess(bInheritHandles=TRUE)` 拉起子进程，会把本进程**所有**
/// 可继承句柄原样带入——包括 caller（PowerShell/管道重定向脚本）交给
/// wtfs.exe 的管道写端。管道写端若被 wtfsd 持有，调用方等的是 EOF 而
/// 非进程退出，wtfs.exe 退出后 EOF 永不到来（M5 实测两次复现，且
/// `tests/spawn_stdio_regression.rs` 验证过 null-only 方案不够）。
/// 因此：
///
/// 1. stdio 全部置 `Stdio::null()`（wtfsd 日志走
///    `%LOCALAPPDATA%\wtfs\logs\wtfsd.log`，不依赖 caller 的 stdio）；
/// 2. Windows 下 spawn 期间临时清掉自身 stdio 句柄的 inherit 标志，
///    spawn 完成后恢复（wtfs-cli 单线程，窗口内无并发 spawn）；
/// 3. Windows 下另加 `DETACHED_PROCESS|CREATE_NO_WINDOW`（脱离控制台、
///    不弹窗）。
///
/// 回归测试：`tests/spawn_stdio_regression.rs`（三进程自举，等 EOF）、
/// `tests/mount_stdio.rs`（ignored，真实挂载端到端）。
pub fn spawn_detached(cmd: &mut Command) -> std::io::Result<Child> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
        // spawn 窗口内清 inherit 标志：阻断 bInheritHandles=TRUE 的句柄泄漏。
        let guard = DetachedStdioGuard::clear_own_stdio();
        let child = cmd.spawn();
        drop(guard);
        child
    }
    #[cfg(not(windows))]
    {
        cmd.spawn()
    }
}

/// spawn detached 子进程期间清掉自身 stdio 句柄 inherit 标志的守卫
/// （B070100）。drop 时恢复原状；无 stdio（NULL/INVALID）或 console 伪
/// 句柄（查询失败）跳过且不影响恢复。
#[cfg(windows)]
struct DetachedStdioGuard {
    /// 已清除 inherit 标志的句柄及其原标志。
    saved: Vec<(HANDLE, u32)>,
}

#[cfg(windows)]
impl DetachedStdioGuard {
    fn clear_own_stdio() -> Self {
        use windows::Win32::Foundation::{
            GetHandleInformation, HANDLE_FLAGS, HANDLE_FLAG_INHERIT, SetHandleInformation,
        };
        use windows::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        let mut saved = Vec::new();
        for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // 无 stdio（NULL/INVALID_HANDLE_VALUE）→ 没有可泄漏的句柄，跳过。
            let Ok(handle) = (unsafe { GetStdHandle(which) }) else {
                continue;
            };
            if handle.is_invalid() {
                continue;
            }
            let mut flags = 0u32;
            // console 伪句柄等查询失败 → 不是管道/文件句柄，跳过。
            if unsafe { GetHandleInformation(handle, &mut flags) }.is_err() {
                continue;
            }
            if flags & HANDLE_FLAG_INHERIT.0 == 0 {
                continue;
            }
            // 清 inherit 标志；失败（罕见）则不记入恢复表，泄漏窗口退化为旧行为。
            if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) }
                .is_ok()
            {
                saved.push((handle, flags));
            }
        }
        Self { saved }
    }
}

#[cfg(windows)]
impl Drop for DetachedStdioGuard {
    fn drop(&mut self) {
        use windows::Win32::Foundation::{
            HANDLE_FLAGS, HANDLE_FLAG_INHERIT, SetHandleInformation,
        };
        for (handle, flags) in &self.saved {
            unsafe {
                let _ = SetHandleInformation(*handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(*flags));
            }
        }
    }
}

#[cfg(windows)]
use windows::Win32::Foundation::HANDLE;

fn cmd_mount(cfg: &DaemonConfig, base: Option<&str>, wait_secs: u64, json: bool) -> anyhow::Result<()> {
    // 已有 daemon 在跑：先确认它服务的正是请求的 repo——全局管道名下，
    // 另一个 repo 的 daemon 在位时绝不能报“已挂载”假成功（评审 P2-1）。
    if let Ok(value) = pipe::call(&cfg.pipe_name, &PipeRequest::Status, Duration::from_millis(800)) {
        let status_repo = value["status"]["repo_root"].as_str().unwrap_or_default();
        let same_repo = !status_repo.is_empty()
            && config::normalize_root(Path::new(status_repo)) == config::normalize_root(&cfg.repo_root);
        if !same_repo {
            print_pipe_value(&value, json);
            bail!(
                "pipe {} is already served by a daemon for repo {:?}, not {:?}; \
                 stop that daemon first (wtfs unmount) or pass --pipe to choose another pipe",
                cfg.pipe_name,
                status_repo,
                cfg.repo_root.display()
            );
        }
        print_pipe_value(&value, json);
        println!("wtfsd already running (see status above); nothing to mount");
        return Ok(());
    }

    // 模板预建（缺模板不阻塞 mount：FS 会走全量 copy 兜底，但显式建一次
    // 能让首个 worktree 直接 dedupe）。
    if let Some(base_ref) = base {
        let store = SqliteTemplateStore::open(&cfg.store_root)
            .with_context(|| format!("open template store {}", cfg.store_root.display()))?;
        let template = store.create_or_update(&TemplateSpec::new(&cfg.repo_root, base_ref))?;
        println!(
            "template ready: {} @ {} ({})",
            template.id, template.base_commit, template.tree_path.display()
        );
    }

    let exe = std::env::current_exe()?
        .with_file_name("wtfsd.exe");
    if !exe.is_file() {
        bail!(
            "wtfsd not found at {}; build the workspace first (cargo build) so wtfs and wtfsd sit side by side",
            exe.display()
        );
    }
    let mut cmd = Command::new(&exe);
    cmd.arg("--repo").arg(&cfg.repo_root)
        .arg("--store-root").arg(&cfg.store_root)
        .arg("--pipe").arg(&cfg.pipe_name);
    let mut child = spawn_detached(&mut cmd)
        .with_context(|| format!("spawn {}", exe.display()))?;
    println!("wtfsd spawned (pid {})", child.id());

    // 等挂载就绪（轮询 status.mounted）。bail 路径顺带清理失败的 detached
    // 进程，避免留下无人管的 wtfsd（评审 P3）。
    let deadline = std::time::Instant::now() + Duration::from_secs(wait_secs);
    loop {
        match pipe::call(&cfg.pipe_name, &PipeRequest::Status, Duration::from_millis(500)) {
            Ok(value) => {
                let mounted = value["status"]["mounted"] == serde_json::Value::Bool(true);
                print_pipe_value(&value, json);
                if mounted {
                    println!("mounted at {}", cfg.mount_point.display());
                    return Ok(());
                }
                let _ = child.kill();
                let _ = child.wait();
                bail!("wtfsd is running but not mounted; check {}", cfg.log_dir.join("wtfsd.log").display());
            }
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "wtfsd did not become reachable within {wait_secs}s: {e}; check {}",
                    cfg.log_dir.join("wtfsd.log").display()
                );
            }
        }
    }
}

fn cmd_unmount(pipe_name: &str, json: bool) -> anyhow::Result<()> {
    let value = pipe::call(pipe_name, &PipeRequest::Stop, PIPE_TIMEOUT)?;
    print_pipe_value(&value, json);
    println!("wtfsd stopped; unmounting");
    Ok(())
}

fn cmd_status(pipe_name: &str, json: bool) -> anyhow::Result<()> {
    let value = pipe::call(pipe_name, &PipeRequest::Status, PIPE_TIMEOUT)?;
    print_pipe_value(&value, json);
    if !json {
        let report: StatusReport = serde_json::from_value(value["status"].clone())
            .context("decode status report")?;
        println!(
            "wtfsd pid {} up {}ms; mounted={} at {}",
            report.pid, report.uptime_ms, report.mounted, report.mount_point
        );
        println!(
            "repo={} store={} dedupe={}",
            report.repo_root, report.store_root, report.dedupe_enabled
        );
        if report.worktrees.is_empty() {
            println!("worktrees: (none)");
        } else {
            println!("worktrees:");
            for wt in &report.worktrees {
                println!("  {}\t{}\t{}", wt.name, wt.state, wt.worktree_id);
            }
        }
    }
    Ok(())
}

// ---- template ----

fn cmd_template(
    repo: &Path,
    base: &str,
    store_root: Option<&Path>,
    updating: bool,
) -> anyhow::Result<()> {
    let cfg = DaemonConfig::for_repo(repo);
    let store_root = store_root.map(Path::to_path_buf).unwrap_or(cfg.store_root);
    let store = SqliteTemplateStore::open(&store_root)
        .with_context(|| format!("open template store {}", store_root.display()))?;
    let template = store.create_or_update(&TemplateSpec::new(repo, base))?;
    let action = if updating { "updated" } else { "created" };
    println!(
        "template {} {}: {} @ {}",
        action,
        template.id,
        base,
        template.base_commit
    );
    println!("tree: {}", template.tree_path.display());
    println!("index: {}", template.index_path.display());
    Ok(())
}

// ---- gc ----

fn cmd_gc(
    pipe_name: &str,
    repo: Option<&PathBuf>,
    store_root: Option<&Path>,
    dry_run: bool,
    max_age_days: u64,
    json: bool,
) -> anyhow::Result<()> {
    // daemon 在线：经管道（含注销路由与 FS 视图感知）。
    if let Ok(value) = pipe::call(
        pipe_name,
        &PipeRequest::Gc { dry_run, max_age_days: Some(max_age_days) },
        PIPE_TIMEOUT,
    ) {
        print_pipe_value(&value, json);
        if !json {
            let summary = value["gc"]["summary"].as_str().unwrap_or("(no summary)");
            println!("gc (via daemon): {summary}");
            if !dry_run {
                println!("{}", value["gc"]["report_summary"].as_str().unwrap_or(""));
            }
        }
        return Ok(());
    }

    // 离线：直连存储。挂载中的 junction 绝不能按真实目录删。
    let repo = require_repo(repo)?;
    let cfg = DaemonConfig::for_repo(repo);
    let store_root = store_root.map(Path::to_path_buf).unwrap_or(cfg.store_root);
    if is_reparse_point(&cfg.mount_point) {
        bail!(
            "{} is an active FS mount but the daemon pipe is unreachable (pipe: {}); \
             stop the daemon (wtfs unmount) or fix the pipe name before gc",
            cfg.mount_point.display(),
            pipe_name
        );
    }
    let ctx = GcContext {
        mount_root: &cfg.mount_point,
        store_root: &store_root,
        registry: None,
        mounted: false,
        max_age: Duration::from_secs(max_age_days * 24 * 3600),
    };
    let gc_plan = gc::plan(&ctx)?;
    if json {
        println!("{}", serde_json::json!({ "dry_run": dry_run, "plan": gc_plan }));
    } else {
        println!("gc (offline): {}", gc_plan.summary());
        for dead in &gc_plan.dead_dirs {
            println!("  dead dir   {} ({})", dead.name, dead.reason);
        }
        for upper in &gc_plan.orphan_uppers {
            println!("  orphan upper wt-{} ({})", upper.worktree_id, upper.dir);
        }
        for template in &gc_plan.old_templates {
            println!(
                "  old template {} @ {} ({}d, {})",
                template.id, template.base_commit, template.age_days, template.state
            );
        }
    }
    if gc_plan.is_empty() {
        return Ok(());
    }
    if dry_run {
        if !json {
            println!("dry run: nothing removed (re-run without --dry-run to apply)");
        }
        return Ok(());
    }
    let report = gc::apply(&ctx, &gc_plan)?;
    if json {
        println!("{}", serde_json::json!({ "report": report }));
    } else {
        println!("{}", report.summary());
        for err in &report.errors {
            println!("  error: {err}");
        }
    }
    Ok(())
}

/// junction/挂载点检测（FS 挂载目录带 reparse point）。
fn is_reparse_point(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}

fn print_pipe_value(value: &serde_json::Value, json: bool) {
    if json {
        println!("{value}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_parses_all_documented_commands() {
        let cli = Cli::command();
        for args in [
            vec!["wtfs", "mount", "--repo", "C:/x", "--base", "master"],
            vec!["wtfs", "unmount"],
            vec!["wtfs", "status", "--json"],
            vec!["wtfs", "template", "create", "--repo", "C:/x", "--base", "master"],
            vec!["wtfs", "template", "update", "--repo", "C:/x", "--base", "master"],
            vec!["wtfs", "gc", "--dry-run", "--max-age-days", "3", "--repo", "C:/x"],
            vec!["wtfs", "gc", "--repo", "C:/x", "--store-root", "C:/s"],
            vec!["wtfs", "mount", "--repo", "C:/x", "--pipe", r"\\.\pipe\wtfs2"],
        ] {
            let _ = &cli; // 冒烟：命令定义本身可构造。
            Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("parse {args:?}: {e}"));
        }
    }

    #[test]
    fn gc_defaults_to_apply_and_dry_run_flag_works() {
        let cli = Cli::try_parse_from(["wtfs", "gc", "--repo", "C:/x"]).unwrap();
        let Cmd::Gc { dry_run, .. } = cli.command else {
            panic!("expected gc");
        };
        assert!(!dry_run, "默认 apply（spec §5.4：--dry-run 显式开关）");
        let cli = Cli::try_parse_from(["wtfs", "gc", "--repo", "C:/x", "--dry-run"]).unwrap();
        let Cmd::Gc { dry_run, .. } = cli.command else {
            panic!("expected gc");
        };
        assert!(dry_run);
    }

    #[test]
    fn require_repo_missing_is_an_error() {
        assert!(require_repo(None).is_err());
    }
}
