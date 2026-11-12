//! wtfs-vfs —— WinFSP 文件系统层。
//!
//! 最终形态是一个 WinFSP volume 覆盖整个 workspace（M2+，含 passthrough 后端）；
//! M0 只挂载 `<repo>/.tower/worktrees`，根下 `wt-*` 全部走 overlay 后端：
//!
//! * lower = 模板 checkout（只读），upper = worktree 私有目录，只存改动。
//! * 初始化期（[`wtfs_core::WorktreeState::Initializing`]）`readdir` 返回空，
//!   让 `git worktree add` 通过"目录必须为空"检查；Git 写完 worktree index 后
//!   由 daemon 切到 `Ready`。
//! * 写入路径 dedupe：逐文件与模板比对（[`wtfs_core::DedupeDecision`]），
//!   一致则丢弃写入（记 lower），不同才写 upper；失败退化为全量 copy。
//! * stat：lower 报告 [`wtfs_core::Stat::fixed_lower`] 的固定值，与 Git index 对齐。
//!
//! 本文件是占位入口；WinFSP `FileSystemContext` 回调（`get_file_info`/`read`/`write`/
//! `read_directory`/`rename`/`cleanup`/`flush` 等）与通知在里程碑 M2 实现。

/// M0 挂载形态描述（占位 API，用于冒烟验证与文档；不是稳定契约）。
pub fn describe() -> &'static str {
    "M0 dedupe-only: mount <repo>/.tower/worktrees; lower=template, upper=per-worktree; no hook, no shim"
}

#[cfg(test)]
mod tests {
    #[test]
    fn describe_mentions_m0_scope() {
        let d = crate::describe();
        assert!(d.contains("M0"));
        assert!(d.contains(".tower/worktrees"));
        assert!(d.contains("no shim"));
    }
}
