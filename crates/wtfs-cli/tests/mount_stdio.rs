//! B070100 回归（真实挂载端到端）：`wtfs mount` 的输出被管道捕获时，caller
//! 必须在 wtfsd 继续服务的情况下拿到 EOF 并正常返回——修复前 detached wtfsd
//! 继承 caller 的管道写端，PowerShell/脚本调用方等 EOF 永久挂起（M5 实测
//! 两次复现，基准 harness 卡死在 mount 调用上）。
//!
//! 需要本机 WinFSP 运行时 + side-by-side 的 wtfsd.exe（workspace 构建后
//! 位于同一 target profile 目录，如 `cargo test -p wtfs-daemon -p wtfs-cli`
//! 会先构建两者）。运行：
//! `cargo test -p wtfs-cli --test mount_stdio -- --ignored`

#![cfg(windows)]

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// wtfsd 必须与 wtfs 同目录（cmd_mount 的 side-by-side 约定）。
fn require_wtfsd() {
    let wtfsd = Path::new(env!("CARGO_BIN_EXE_wtfs")).with_file_name("wtfsd.exe");
    assert!(
        wtfsd.is_file(),
        "wtfsd not found at {}; build the workspace first so wtfs and wtfsd sit side by side",
        wtfsd.display()
    );
}

fn run_wtfs(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_wtfs"))
        .args(args)
        .output()
        .expect("run wtfs");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

#[test]
#[ignore = "requires real WinFSP runtime and side-by-side wtfsd.exe"]
fn mount_with_piped_output_returns_while_daemon_keeps_serving() {
    require_wtfsd();

    let base = tempfile::tempdir().expect("tempdir");
    let repo = base.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let pipe = format!(r"\\.\pipe\wtfs-stdio-test-{}", std::process::id());

    // caller：管道捕获 stdout/stderr（PowerShell/脚本场景的等价物）。
    let mut caller = Command::new(env!("CARGO_BIN_EXE_wtfs"))
        .args([
            "mount",
            "--repo",
            repo.to_str().unwrap(),
            "--store-root",
            base.path().join("store").to_str().unwrap(),
            "--pipe",
            &pipe,
            "--wait-secs",
            "30",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wtfs mount");

    // 等 EOF——修复前：wtfsd 持有管道写端，EOF 永不到来（挂起）。
    let mut out = caller.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = out.read_to_string(&mut buf);
        let _ = tx.send(buf);
    });
    let started = Instant::now();
    let output = rx
        .recv_timeout(Duration::from_secs(45))
        .expect("EOF never arrived: detached wtfsd still holds the caller's pipe write end (B070100 regression)");
    let status = caller.wait().expect("wait wtfs mount");
    assert!(status.success(), "wtfs mount failed: {status}\noutput: {output}");
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "mount must return well within the wait budget, took {:?}",
        started.elapsed()
    );

    // EOF 之后 daemon 必须仍在服务——证明 EOF 来自句柄断开而非 wtfsd 退出。
    let (ok, out) = run_wtfs(&["status", "--pipe", &pipe, "--json"]);
    assert!(ok, "wtfs status after EOF failed: {out}");
    let value: serde_json::Value =
        serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("decode status: {e}; raw {out:?}"));
    assert_eq!(
        value["status"]["mounted"],
        serde_json::Value::Bool(true),
        "daemon must keep serving after the caller got EOF: {value}"
    );

    // 清理：卸载（顺带覆盖 CLI unmount 路径）。Stop 响应可能先于 shutdown
    // 完成，等 pipe 不可达 + junction 消失后再让 tempdir 清理，避免 TempDir
    // 删除撞上卸载竞态。
    let (ok, out) = run_wtfs(&["unmount", "--pipe", &pipe]);
    assert!(ok, "wtfs unmount failed: {out}");
    let mount_point = repo.join(".tower").join("worktrees");
    let deadline = Instant::now() + Duration::from_secs(15);
    while mount_point.exists() || run_wtfs(&["status", "--pipe", &pipe, "--json"]).0 {
        assert!(
            Instant::now() < deadline,
            "daemon/junction did not go away after unmount: junction exists={}",
            mount_point.exists()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}
