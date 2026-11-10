//! External subagent identity and terminal evidence.
//!
//! RFC-0001: 原生 Herdr subagent 的共享 wire 词汇。

use serde::{Deserialize, Serialize};

/// An isolated Git checkout requested for an external child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalSubagentWorktreeRequest {
    pub branch: Option<String>,
    pub base: Option<String>,
}

/// A Herdr-managed checkout retained after its child pane closes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalSubagentWorktree {
    pub path: String,
    pub branch: String,
    pub repo_root: String,
}

/// The external runtime behind one logical Prime Agent child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalSubagentInfo {
    pub agent: String,
    pub workspace_id: String,
    pub pane_id: String,
    pub terminal_id: String,
    pub agent_name: String,
    pub turn: u64,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<ExternalSubagentWorktree>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_path: Option<String>,
    pub answer_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<ExternalSubagentOutput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_error: Option<String>,
}

/// A terminal snapshot, distinct from the child's answer file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalSubagentOutput {
    pub text: String,
    pub source: String,
    pub revision: u64,
    pub truncated: bool,
}
