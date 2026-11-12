//! `.tower/comms/state.json` 解析：为 worktree 定位模板 base（spec §6.3.4）。
//!
//! 文件缺失/损坏/字段缺失一律返回 `None`/空结果，调用方回退：
//! 主仓库当前 base branch 的模板，或全部写 upper（全量 copy）——正确性优先。
//!
//! 结构（只取需要的字段，未知字段忽略，向前兼容）：
//! `version`、顶层 `base`、`missions[].{id, worktree, branch, status}`。

use std::path::Path;

use serde::Deserialize;

use crate::{Error, Result, TemplateSpec};

/// state.json 顶层结构。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TowerStateFile {
    #[serde(default)]
    pub version: Option<u64>,
    /// Tower 会话的 base ref（模板基线）。
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub missions: Vec<TowerMissionEntry>,
}

/// `missions[]` 条目。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TowerMissionEntry {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub worktree: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

impl TowerStateFile {
    /// 解析 JSON；失败报 [`Error::Template`]。
    pub fn parse(json: &str) -> Result<Self> {
        serde_json::from_str(json)
            .map_err(|e| Error::Template(format!("parse .tower/comms/state.json: {e}")))
    }

    /// `mission.worktree == worktree` 的条目。
    pub fn mission_for_worktree(&self, worktree: &str) -> Option<&TowerMissionEntry> {
        self.missions
            .iter()
            .find(|m| m.worktree.as_deref() == Some(worktree))
    }

    /// 该 worktree 的模板 base ref（顶层 `base`）；mission 不匹配或缺 base → None。
    pub fn base_ref_for_worktree(&self, worktree: &str) -> Option<&str> {
        self.mission_for_worktree(worktree)?;
        self.base.as_deref()
    }

    /// 该 worktree 的 mission branch（信息用途，daemon/shim 可能需要）。
    pub fn branch_for_worktree(&self, worktree: &str) -> Option<&str> {
        self.mission_for_worktree(worktree)
            .and_then(|m| m.branch.as_deref())
    }
}

/// 读取 `<repo>/.tower/comms/state.json`；缺失/不可解析 → None（走兜底）。
pub fn read_state_file(repo_root: &Path) -> Option<TowerStateFile> {
    let path = repo_root.join(".tower").join("comms").join("state.json");
    match std::fs::read_to_string(&path) {
        Ok(json) => match TowerStateFile::parse(&json) {
            Ok(state) => Some(state),
            Err(e) => {
                tracing::warn!(error = %e, "ignore unreadable tower state");
                None
            }
        },
        Err(_) => None,
    }
}

/// spec §6.3.4：`mission.worktree == wt-N` → base ref 的 [`TemplateSpec`]。
/// 找不到 → None；调用方回退主仓库 base branch 模板或全量 copy。
pub fn template_spec_for_worktree(repo_root: &Path, worktree: &str) -> Option<TemplateSpec> {
    let state = read_state_file(repo_root)?;
    let base = state.base_ref_for_worktree(worktree)?;
    Some(TemplateSpec::new(repo_root, base.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const SAMPLE: &str = r#"{
      "version": 1,
      "base": "master",
      "mode": "branch",
      "roster": {"agents": []},
      "missions": [
        {"id": "M1", "worktree": "wt-1", "branch": "feat/one", "status": "merged"},
        {"id": "M2", "worktree": "wt-2", "branch": "feat/two", "status": "active"}
      ],
      "extra_unknown": {"ignored": true}
    }"#;

    #[test]
    fn parse_and_lookup_worktree() {
        let state = TowerStateFile::parse(SAMPLE).unwrap();
        assert_eq!(state.base.as_deref(), Some("master"));
        let m2 = state.mission_for_worktree("wt-2").unwrap();
        assert_eq!(m2.branch.as_deref(), Some("feat/two"));
        assert_eq!(state.branch_for_worktree("wt-2"), Some("feat/two"));
        assert_eq!(state.base_ref_for_worktree("wt-1"), Some("master"));
        assert_eq!(state.base_ref_for_worktree("wt-99"), None);
        assert_eq!(state.branch_for_worktree("wt-99"), None);
        // 未知字段被忽略：parse 成功即是证明
        assert_eq!(state.missions.len(), 2);
    }

    #[test]
    fn missing_base_or_broken_json_yield_none() {
        let state = TowerStateFile::parse(r#"{"missions":[{"worktree":"wt-1"}]}"#).unwrap();
        assert_eq!(state.base_ref_for_worktree("wt-1"), None);
        assert!(TowerStateFile::parse("{broken").is_err());
    }

    #[test]
    fn read_state_file_and_template_spec() {
        let dir = TempDir::new().unwrap();
        // 缺失 → None
        assert!(read_state_file(dir.path()).is_none());
        assert!(template_spec_for_worktree(dir.path(), "wt-2").is_none());
        // 写入后命中
        let comms = dir.path().join(".tower").join("comms");
        std::fs::create_dir_all(&comms).unwrap();
        std::fs::write(comms.join("state.json"), SAMPLE).unwrap();
        let spec = template_spec_for_worktree(dir.path(), "wt-2").unwrap();
        assert_eq!(spec.base_ref, "master");
        assert_eq!(spec.repo_root, dir.path());
        assert!(template_spec_for_worktree(dir.path(), "wt-42").is_none());
    }
}
