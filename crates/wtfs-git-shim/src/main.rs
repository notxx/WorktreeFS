//! wtfs-git-shim —— `git.exe` 包装器（安装时重命名/前置 PATH 为 `git.exe`）。
//!
//! **M0 不启用 shim**（dedupe-only，spec §5.5/§9）：`git worktree add` 正常执行，
//! 由 wtfs-vfs 在写入路径 dedupe。本占位二进制不拦截、不转发任何命令；
//! M1 起才会解析 argv：
//!
//! * `worktree add` 且目标在 `.tower/worktrees/` 下 → 调 `wtfs worktree-add`，
//!   失败则 exec 真实 git（全量 checkout 兜底）。
//! * `worktree remove` → 先 `wtfs worktree-remove`，再 exec 真实 git。
//! * 其他命令 → exec 真实 git（`WTFS_REAL_GIT` 指定，防递归）。

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    eprintln!(
        "wtfs-git-shim: shim 在 M0 未启用（dedupe-only），不能代替 git 执行: git {args}"
    );
    std::process::exit(2);
}
