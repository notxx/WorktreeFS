//! initializing 阶段的内存元数据缓冲（M8/B080100 批量写；M11/B110100 收窄）。
//!
//! `git worktree add` 的 checkout 窗口内，worktree 对 readdir 隐藏
//! （[`crate::WorktreeState::Initializing`]），此时把 `whiteouts` /
//! `renames` 累积在本结构里，**每个文件零事务、零 DB 读**；
//! [`crate::Overlay::mark_ready`] 在一个 SQLite 事务内整批落库并把状态切到
//! ready（见 [`crate::overlay_store::SqliteOverlayStore::flush_init_batch`]）。
//!
//! M11 起 `file_ids` / `dedupe` / `stat_cache` 不再存在于任何阶段：三类状态
//! 全部由 upper 物理文件 + 模板清单推导（ino = 路径哈希、mtime = 真实文件
//! mtime），无需缓冲。
//!
//! * 缓冲是该 worktree 元数据的**权威视图**：fresh checkout 时从空构造；
//!   崩溃重开（reopen 到 initializing 行）时一次性从 DB 装载既有行，
//!   之后所有读不再回落 DB。
//! * 并发：外层由 [`crate::Overlay`] 的 `Mutex<InitMeta>` 保护，
//!   本结构自身不持锁，方法均为普通内存操作。
//! * 崩溃语义：ready 前崩溃丢缓冲（upper 文件是数据真相，不受影响），
//!   worktree 保持 initializing，daemon 可标记 failed / 清理（spec §6.3.6）。

use std::collections::HashSet;

/// initializing 期累积的 overlay 元数据（一个 worktree 一份）。
#[derive(Debug, Default)]
pub(crate) struct InitBatch {
    /// 目录级重命名 (from → to)，保持插入序（与 store 读取序一致）。
    renames: Vec<(String, String)>,
    /// lower 命名空间 whiteout 集合。
    whiteouts: HashSet<String>,
}

impl InitBatch {
    /// fresh checkout：空缓冲（DB 侧该 worktree 行刚建，无既有元数据）。
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    /// 崩溃重开：从 DB 一次性装载该 worktree 的既有元数据行。
    /// 装载后缓冲即权威视图，后续读写不再触 DB。
    pub(crate) fn load(store: &crate::SqliteOverlayStore, id: crate::WorktreeId) -> crate::Result<Self> {
        Ok(Self {
            renames: store.renames(id)?,
            whiteouts: store.whiteouts(id)?,
        })
    }

    pub(crate) fn add_whiteout(&mut self, path: &str) {
        self.whiteouts.insert(path.to_string());
    }

    /// 撤销 whiteout；无条目时为天然 no-op（不产生任何后续工作）。
    pub(crate) fn remove_whiteout(&mut self, path: &str) {
        self.whiteouts.remove(path);
    }

    pub(crate) fn whiteouts(&self) -> &HashSet<String> {
        &self.whiteouts
    }

    pub(crate) fn record_rename(&mut self, from: &str, to: &str) {
        if let Some(pair) = self.renames.iter_mut().find(|(f, _)| f == from) {
            pair.1 = to.to_string();
        } else {
            self.renames.push((from.to_string(), to.to_string()));
        }
    }

    pub(crate) fn renames(&self) -> &[(String, String)] {
        &self.renames
    }

    /// 落库行视图（只读借用，flush 用；避免整批拷贝）。
    pub(crate) fn rows(&self) -> BatchRows<'_> {
        BatchRows {
            renames: &self.renames,
            whiteouts: &self.whiteouts,
        }
    }
}

/// [`InitBatch::rows`] 的只读行视图。
pub(crate) struct BatchRows<'a> {
    pub renames: &'a [(String, String)],
    pub whiteouts: &'a HashSet<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Db, OverlayStore, SqliteOverlayStore, WorktreeState};

    /// 返回 (store, cfg, TempDir 保活)；cfg 已注册为 WorktreeId(1)。
    fn store() -> (SqliteOverlayStore, crate::OverlayConfig, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let upper = dir.path().join("upper");
        std::fs::create_dir_all(&upper).unwrap();
        let mut cfg = crate::OverlayConfig::new(crate::WorktreeId(1), &upper);
        cfg.dedupe_enabled = false;
        let st = SqliteOverlayStore::new(Db::open_in_memory().unwrap());
        st.register(&cfg).unwrap();
        (st, cfg, dir)
    }

    #[test]
    fn whiteouts_and_renames_upsert() {
        let mut batch = InitBatch::empty();
        batch.add_whiteout("gone.txt");
        assert!(batch.whiteouts().contains("gone.txt"));
        batch.remove_whiteout("gone.txt");
        batch.remove_whiteout("gone.txt"); // 二次撤销 = no-op
        assert!(!batch.whiteouts().contains("gone.txt"));

        batch.record_rename("src", "dst");
        batch.record_rename("src", "dst2"); // upsert 同 from
        assert_eq!(
            batch.renames(),
            &[("src".to_string(), "dst2".to_string())]
        );
    }

    #[test]
    fn load_from_store_roundtrips_rows_and_flushes() {
        let (store, cfg, _guard) = store();
        let id = cfg.worktree_id;

        let mut batch = InitBatch::empty();
        batch.add_whiteout("gone.txt");
        batch.record_rename("src", "dst");

        // flush（单事务 + ready）
        store.flush_init_batch(id, &batch, WorktreeState::Ready).unwrap();
        assert_eq!(store.state(id).unwrap(), Some(WorktreeState::Ready));

        // 行已落库
        assert!(store.whiteouts(id).unwrap().contains("gone.txt"));
        assert_eq!(
            store.renames(id).unwrap(),
            vec![("src".into(), "dst".into())]
        );

        // 崩溃重开装载 → 权威视图一致
        let reloaded = InitBatch::load(&store, id).unwrap();
        assert!(reloaded.whiteouts().contains("gone.txt"));
        assert_eq!(
            reloaded.renames(),
            &[("src".into(), "dst".into())]
        );

        // flush 幂等收敛：重放同批数据后行数不变
        store.flush_init_batch(id, &reloaded, WorktreeState::Ready).unwrap();
        assert_eq!(store.whiteouts(id).unwrap().len(), 1);
        assert_eq!(store.renames(id).unwrap().len(), 1);
    }
}
