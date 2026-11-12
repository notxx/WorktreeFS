//! `wtfs` 命令行（骨架）。
//!
//! 目标命令面（spec §5.4，随里程碑落地）：`mount` / `unmount` /
//! `template create|update` / `worktree-add`（M1+） / `worktree-remove` / `gc` / `status`。
//! 当前只报告版本；实现由 wtfs-daemon 承载。

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "wtfs",
    version,
    about = "WorktreeFS —— Tower worktree 的 COW overlay 文件系统"
)]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
    println!("wtfs {}", env!("CARGO_PKG_VERSION"));
}
