//! wtfs-daemon —— wtfsd：挂载生命周期 / named pipe API（`\\.\pipe\wtfs`）/
//! 模板与 overlay 注册表 / dedupe·index 协调 / gc / 崩溃恢复。
//!
//! M0 职责（spec §5.3/§6.3）：
//!
//! 1. 确认并挂载 `<repo>/.tower/worktrees`（主仓库保持原生）。
//! 2. 加载模板/overlay 注册表；恢复未完成的 worktree。
//! 3. 监听 `<main>/.git/worktrees/<name>/index` 的写入/关闭，完成
//!    `initializing → ready` 切换。
//! 4. 创建后写 `config.worktree`：`core.checkStat=minimal`、
//!    `core.trustctime=false`、`core.untrackedCache=false`。
//!
//! 本文件是占位入口；挂载编排、named pipe、index 监听与 gc 在里程碑 M2 实现。

/// daemon 当前里程碑描述（占位 API；不是稳定契约）。
pub fn describe() -> &'static str {
    "wtfsd M0 placeholder: mount lifecycle / pipe API / index watcher / gc not implemented yet"
}

#[cfg(test)]
mod tests {
    #[test]
    fn describe_flags_unimplemented_pieces() {
        let d = crate::describe();
        assert!(d.contains("M0"));
        assert!(d.contains("not implemented yet"));
    }
}
