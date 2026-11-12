//! B070100 回归：detached wtfsd 不得持有 caller 的管道写端。
//!
//! 复现链（与 M5 finding 一致）：caller 以管道捕获 `wtfs mount` 的输出 →
//! wtfs.exe 拉起 detached wtfsd 后退出 → caller 等 EOF。两段泄漏路径都已
//! 堵住（见 [`wtfs_cli::spawn_detached`]）：
//!
//! * stdio 置 `Stdio::null()`；
//! * spawn 窗口内清自身 stdio 句柄的 inherit 标志——Rust 的
//!   `CreateProcess(bInheritHandles=TRUE)` 会把本进程所有可继承句柄带入
//!   子进程，仅 null stdio 切不断这条路径（本测试曾以 null-only 方案跑出
//!   EOF 迟到 3s 的失败，实锤该泄漏）。
//!
//! 修复后（inherit 标志被清）wtfs.exe 一退出 caller 立即拿到 EOF。
//!
//! 本测试三进程自举（`harness = false`，按 `WTFS_TEST_ROLE` 分饰）：
//! parent（本进程，模拟 caller）→ child（模拟 wtfs mount，走生产
//! [`wtfs_cli::spawn_detached`] 路径）→ grandchild（模拟长驻 wtfsd，
//! 存活 3s）。parent 的 EOF 预算 2s：修复生效时 <1s 返回；若写端仍被
//! grandchild 持有，EOF 只能等它退出（3s）→ 超时判定失败。

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

fn main() {
    match std::env::var("WTFS_TEST_ROLE").ok().as_deref() {
        Some("grandchild") => grandchild_role(),
        Some("child") => child_role(),
        _ => parent(),
    }
}

/// 模拟长驻 wtfsd：存活期必须明显长于 parent 的 EOF 预算（2s），保证
/// “写端被继承”时 EOF 晚于预算到来（3s > 2s → 判定失败）。
fn grandchild_role() {
    std::thread::sleep(Duration::from_secs(3));
}

/// 模拟 `wtfs mount`：用生产 spawn 路径拉起 grandchild 后立即退出。
/// caller 能否拿到 EOF，只取决于 grandchild 是否继承了 stdout 写端。
fn child_role() {
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = Command::new(exe);
    // env() 覆盖继承值：grandchild 必须进入 sleep 角色，不得递归 child。
    cmd.env("WTFS_TEST_ROLE", "grandchild");
    wtfs_cli::spawn_detached(&mut cmd)
        .expect("spawn grandchild (detached) must succeed");
}

/// caller：以管道捕获 child 的 stdout 等 EOF——修复前这里会超时挂起。
fn parent() {
    let exe = std::env::current_exe().expect("current_exe");
    let mut child = Command::new(exe)
        .env("WTFS_TEST_ROLE", "child")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn child must succeed");

    let mut stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let ok = stdout.read_to_end(&mut buf).is_ok();
        let _ = tx.send(ok);
    });

    match rx.recv_timeout(Duration::from_secs(2)) {
        Ok(true) => {}
        Ok(false) => panic!("reading child stdout failed"),
        Err(_) => panic!(
            "caller pipe stayed open after wtfs exited: detached daemon \
             still holds the caller's pipe write end (B070100 regression)"
        ),
    }
    let status = child.wait().expect("wait child");
    assert!(status.success(), "child role exited with {status}");
}
