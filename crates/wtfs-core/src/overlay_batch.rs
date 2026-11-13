//! initializing 阶段的内存元数据缓冲（M8/B080100：批量 overlay 元数据写）。
//!
//! `git worktree add` 的 checkout 窗口内，worktree 对 readdir 隐藏
//! （[`crate::WorktreeState::Initializing`]），此时把 `file_ids` / `dedupe` /
//! `stat_cache` / `whiteouts` / `renames` 全部累积在本结构里，
//! **每个文件零事务、零 DB 读**；[`crate::Overlay::mark_ready`] 在一个
//! SQLite 事务内整批落库并把状态切到 ready（见
//! [`crate::overlay_store::SqliteOverlayStore::flush_init_batch`]）。
//!
//! * 缓冲是该 worktree 元数据的**权威视图**：fresh checkout 时从空构造；
//!   崩溃重开（reopen 到 initializing 行）时一次性从 DB 装载既有行，
//!   之后所有读不再回落 DB。
//! * 并发：外层由 [`crate::Overlay`] 的 `Mutex<InitMeta>` 保护，
//!   本结构自身不持锁，方法均为普通内存操作。
//! * 崩溃语义：ready 前崩溃丢缓冲（upper 文件是数据真相，不受影响），
//!   worktree 保持 initializing，daemon 可标记 failed / 清理（spec §6.3.6）。

use std::collections::{HashMap, HashSet};

use crate::{DedupeState, overlay_store::StatCacheEntry, overlay_store::derive_ino, WorktreeId};

/// initializing 期累积的 overlay 元数据（一个 worktree 一份）。
#[derive(Debug, Default)]
pub(crate) struct InitBatch {
    /// path → 稳定 ino（[`crate::overlay_store`] 的派生 + 线性探测）。
    file_ids: HashMap<String, u64>,
    /// ino → path（派生冲突探测的反向索引）。
    file_ids_by_id: HashMap<u64, String>,
    /// path → 三态 dedupe 决策。
    dedupe: HashMap<String, DedupeState>,
    /// path → stat 缓存（逻辑 mtime / size / flags）。
    stat: HashMap<String, StatCacheEntry>,
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
    pub(crate) fn load(store: &crate::SqliteOverlayStore, id: WorktreeId) -> crate::Result<Self> {
        let mut batch = Self::empty();
        for (path, ino) in store.file_id_rows(id)? {
            batch.insert_file_id(path, ino);
        }
        batch.dedupe = store.dedupe_states(id)?;
        batch.stat = store.stat_entries(id)?;
        batch.renames = store.renames(id)?;
        batch.whiteouts = store.whiteouts(id)?;
        Ok(batch)
    }

    fn insert_file_id(&mut self, path: String, ino: u64) {
        self.file_ids_by_id.insert(ino, path.clone());
        self.file_ids.insert(path, ino);
    }

    /// 稳定 file id：blake3 派生 + 反向索引线性探测（与 store 实现同语义），
    /// 全程内存、零 DB。
    pub(crate) fn file_id(&mut self, path: &str) -> u64 {
        if let Some(ino) = self.file_ids.get(path) {
            return *ino;
        }
        let mut candidate = derive_ino(path) as u64;
        while let Some(occupant) = self.file_ids_by_id.get(&candidate) {
            if occupant == path {
                // 同一路径已在（大小写不同的派生碰撞由小写派生消解）。
                return candidate;
            }
            candidate = if candidate as i64 >= MAX_INO { 2 } else { candidate + 1 };
        }
        let ino = candidate;
        self.insert_file_id(path.to_string(), ino);
        ino
    }

    pub(crate) fn record_dedupe(&mut self, path: &str, state: DedupeState) {
        self.dedupe.insert(path.to_string(), state);
    }

    pub(crate) fn remove_dedupe(&mut self, path: &str) {
        self.dedupe.remove(path);
    }

    pub(crate) fn set_stat_entry(&mut self, path: &str, entry: StatCacheEntry) {
        self.stat.insert(path.to_string(), entry);
    }

    /// 删除一条 stat 缓存；返回删除前是否存在（无条目 = 调用方跳过后续工作）。
    pub(crate) fn remove_stat_entry(&mut self, path: &str) -> bool {
        self.stat.remove(path).is_some()
    }

    pub(crate) fn stat_entry(&self, path: &str) -> Option<StatCacheEntry> {
        self.stat.get(path).copied()
    }

    /// `dir` 及其子树的 dedupe / stat 行（目录删除/改名时清理），返回受影响行数。
    pub(crate) fn remove_meta_subtree(&mut self, dir: &str) -> usize {
        let prefix = format!("{dir}/");
        let removed_dedupe: Vec<String> = self
            .dedupe
            .keys()
            .filter(|p| p.as_str() == dir || p.starts_with(&prefix))
            .cloned()
            .collect();
        for p in &removed_dedupe {
            self.dedupe.remove(p);
        }
        let removed_stat: Vec<String> = self
            .stat
            .keys()
            .filter(|p| p.as_str() == dir || p.starts_with(&prefix))
            .cloned()
            .collect();
        for p in &removed_stat {
            self.stat.remove(p);
        }
        removed_dedupe.len() + removed_stat.len()
    }

    /// upper 内容随 rename 物理移动：把 from（含子树）的 dedupe / stat 键改写为 to。
    pub(crate) fn rewrite_meta_prefix(&mut self, from: &str, to: &str) {
        let from_prefix = format!("{from}/");
        let map_key = |path: &str| -> String {
            if path == from {
                to.to_string()
            } else {
                format!("{to}/{}", &path[from_prefix.len()..])
            }
        };
        let moved_dedupe: Vec<(String, DedupeState)> = self
            .dedupe
            .iter()
            .filter(|(p, _)| p.as_str() == from || p.starts_with(&from_prefix))
            .map(|(p, s)| (map_key(p), *s))
            .collect();
        for old in self
            .dedupe
            .keys()
            .filter(|p| p.as_str() == from || p.starts_with(&from_prefix))
            .cloned()
            .collect::<Vec<_>>()
        {
            self.dedupe.remove(&old);
        }
        for (new_path, state) in moved_dedupe {
            self.dedupe.insert(new_path, state);
        }
        let moved_stat: Vec<(String, StatCacheEntry)> = self
            .stat
            .iter()
            .filter(|(p, _)| p.as_str() == from || p.starts_with(&from_prefix))
            .map(|(p, e)| (map_key(p), *e))
            .collect();
        for old in self
            .stat
            .keys()
            .filter(|p| p.as_str() == from || p.starts_with(&from_prefix))
            .cloned()
            .collect::<Vec<_>>()
        {
            self.stat.remove(&old);
        }
        for (new_path, entry) in moved_stat {
            self.stat.insert(new_path, entry);
        }
    }

    pub(crate) fn add_whiteout(&mut self, path: &str) {
        self.whiteouts.insert(path.to_string());
    }

    /// 撤销 whiteout；无条目时为天然 no-op（不产生任何后续工作）。
    pub(crate) fn remove_whiteout(&mut self, path: &str) {
        self.whiteouts.remove(path);
    }

    pub(crate) fn whiteouts(&self) -> HashSet<String> {
        self.whiteouts.clone()
    }

    pub(crate) fn record_rename(&mut self, from: &str, to: &str) {
        if let Some(pair) = self.renames.iter_mut().find(|(f, _)| f == from) {
            pair.1 = to.to_string();
        } else {
            self.renames.push((from.to_string(), to.to_string()));
        }
    }

    pub(crate) fn renames(&self) -> Vec<(String, String)> {
        self.renames.clone()
    }

    /// 落库行视图（只读借用，flush 用；避免整批拷贝）。
    pub(crate) fn rows(&self) -> BatchRows<'_> {
        BatchRows {
            file_ids: &self.file_ids,
            dedupe: &self.dedupe,
            stat: &self.stat,
            renames: &self.renames,
            whiteouts: &self.whiteouts,
        }
    }
}

/// [`InitBatch::rows`] 的只读行视图。
pub(crate) struct BatchRows<'a> {
    pub file_ids: &'a HashMap<String, u64>,
    pub dedupe: &'a HashMap<String, DedupeState>,
    pub stat: &'a HashMap<String, StatCacheEntry>,
    pub renames: &'a [(String, String)],
    pub whiteouts: &'a HashSet<String>,
}

const MAX_INO: i64 = 0x3FFF_FFFF_FFFF_FFFF;

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
    fn file_id_is_stable_and_probes_collisions_like_store() {
        let mut batch = InitBatch::empty();
        let a = batch.file_id("a/b.txt");
        assert_eq!(a, batch.file_id("a/b.txt"), "同路径幂等");
        assert_ne!(a, batch.file_id("a/c.txt"));
        assert_ne!(a, batch.file_id("A/B.TXT"), "大小写派生不同路径");
        // 人为占据派生位 → 线性探测 +1
        let derived = derive_ino("new.txt");
        batch.insert_file_id("squatter.txt".to_string(), derived as u64);
        let n = batch.file_id("new.txt");
        assert_eq!(n, (derived + 1) as u64, "派生被占 → 探测 +1");
        // 大小写不敏感路径派生一致：SQUATTER.TXT 与 squatter.txt 同 id，
        // file_id("new.txt") 不会误判自我。
        assert_ne!(n, batch.file_id("other.txt"));
    }

    #[test]
    fn dedupe_stat_subtree_and_prefix_rewrite() {
        let mut batch = InitBatch::empty();
        batch.record_dedupe("a/f.txt", DedupeState::Lower);
        batch.record_dedupe("keep.txt", DedupeState::Upper);
        batch.set_stat_entry("a/f.txt", StatCacheEntry { mtime_ns: 10, size: 1, flags: 0 });
        batch.set_stat_entry("a/g.txt", StatCacheEntry { mtime_ns: 20, size: 2, flags: 0 });
        batch.set_stat_entry("keep.txt", StatCacheEntry { mtime_ns: 30, size: 3, flags: 0 });
        assert_eq!(batch.stat_entry("a/g.txt").unwrap().mtime_ns, 20);

        assert!(batch.remove_stat_entry("a/f.txt"), "存在 → true");
        assert!(!batch.remove_stat_entry("a/f.txt"), "无条目 → false（跳过）");
        assert_eq!(batch.stat_entry("a/f.txt"), None);

        batch.rewrite_meta_prefix("keep.txt", "moved.txt");
        assert_eq!(batch.stat_entry("moved.txt").unwrap().mtime_ns, 30);
        assert_eq!(batch.dedupe["moved.txt"], DedupeState::Upper);

        batch.record_dedupe("a/f.txt", DedupeState::New);
        batch.set_stat_entry("a/f.txt", StatCacheEntry { mtime_ns: 40, size: 4, flags: 0 });
        // 子树内：dedupe a/f.txt + stat a/f.txt + stat a/g.txt
        assert_eq!(batch.remove_meta_subtree("a"), 3, "1 dedupe + 2 stat");
        assert_eq!(batch.stat_entry("a/g.txt"), None);
        assert_eq!(batch.stat_entry("moved.txt").unwrap().mtime_ns, 30);
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
            vec![("src".to_string(), "dst2".to_string())]
        );
    }

    #[test]
    fn load_from_store_roundtrips_rows_and_flushes() {
        let (store, cfg, _guard) = store();
        let id = cfg.worktree_id;

        let mut batch = InitBatch::empty();
        let ino = batch.file_id("a/f.txt");
        batch.record_dedupe("a/f.txt", DedupeState::Lower);
        batch.set_stat_entry("a/f.txt", StatCacheEntry { mtime_ns: 77, size: 9, flags: 0 });
        batch.add_whiteout("gone.txt");
        batch.record_rename("src", "dst");

        // flush（单事务 + ready）
        store.flush_init_batch(id, &batch, WorktreeState::Ready).unwrap();
        assert_eq!(store.state(id).unwrap(), Some(WorktreeState::Ready));

        // 行已落库
        assert_eq!(store.file_id(id, "a/f.txt").unwrap(), ino);
        assert_eq!(store.dedupe_states(id).unwrap()["a/f.txt"], DedupeState::Lower);
        assert_eq!(store.stat_entry(id, "a/f.txt").unwrap().unwrap().mtime_ns, 77);
        assert!(store.whiteouts(id).unwrap().contains("gone.txt"));
        assert_eq!(store.renames(id).unwrap(), vec![("src".into(), "dst".into())]);
        assert_eq!(store.max_stat_mtime(id).unwrap(), Some(77));

        // 崩溃重开装载 → 权威视图一致
        let mut reloaded = InitBatch::load(&store, id).unwrap();
        assert_eq!(reloaded.file_id("a/f.txt"), ino);
        assert_eq!(reloaded.stat_entry("a/f.txt").unwrap().mtime_ns, 77);
        assert!(reloaded.whiteouts().contains("gone.txt"));
        assert_eq!(reloaded.renames(), vec![("src".into(), "dst".into())]);

        // flush 幂等收敛：重放同批数据后行数不变
        store.flush_init_batch(id, &reloaded, WorktreeState::Ready).unwrap();
        assert_eq!(store.stat_entries(id).unwrap().len(), 1);
    }
}
