//! RFC-0001: Herdr owns worktree creation and its workspace; closing a child
//! never removes its checkout or branch.

use anyhow::{bail, Context, Result};
use pa_types::daemon::external_subagent::{
    ExternalSubagentWorktree, ExternalSubagentWorktreeRequest,
};
use serde_json::{json, Value};
use std::{path::Path, time::Duration};

use super::required_str;
use crate::herdr_client::HerdrClient;

pub(super) struct WorktreeLocation {
    pub(super) pane: Value,
    pub(super) workspace_id: String,
    pub(super) worktree: ExternalSubagentWorktree,
}

pub(super) async fn create_worktree(
    client: &HerdrClient,
    request: &ExternalSubagentWorktreeRequest,
    cwd: &str,
    child_id: &str,
    name: &str,
) -> Result<WorktreeLocation> {
    if !Path::new(cwd).is_absolute() {
        bail!("Worktree subagents require an absolute parent cwd");
    }
    let branch = request
        .branch
        .clone()
        .unwrap_or_else(|| format!("task/{child_id}"));
    let mut params = json!({"cwd":cwd,"branch":branch,"label":name,"focus":false});
    if let Some(base) = &request.base {
        params["base"] = json!(base);
    }
    let result = client
        .request("worktree.create", params, Duration::from_secs(120))
        .await
        .with_context(|| format!("Create external subagent worktree {branch} from {cwd}; inspect this branch after an uncertain response, which is never retried"))?;
    let parse = || -> Result<WorktreeLocation> {
        let workspace = result
            .get("workspace")
            .context("Herdr worktree response omitted workspace")?;
        let workspace_id = required_str(workspace, "workspace_id")?.to_string();
        let worktree = result
            .get("worktree")
            .context("Herdr worktree response omitted checkout")?;
        let path = required_str(worktree, "path")?;
        let actual_branch = required_str(worktree, "branch")?;
        let membership = workspace
            .get("worktree")
            .context("Herdr workspace omitted worktree membership")?;
        let repo_root = required_str(membership, "repo_root")?;
        if !Path::new(path).is_absolute()
            || !Path::new(repo_root).is_absolute()
            || actual_branch != branch
            || worktree.get("is_linked_worktree").and_then(Value::as_bool) != Some(true)
            || membership
                .get("is_linked_worktree")
                .and_then(Value::as_bool)
                != Some(true)
            || required_str(membership, "checkout_path")? != path
        {
            bail!("Herdr worktree identity differs from the requested linked checkout");
        }
        let pane = result
            .get("root_pane")
            .context("Herdr worktree response omitted root pane")?;
        if required_str(pane, "workspace_id")? != workspace_id {
            bail!("Herdr worktree root pane belongs to a different workspace");
        }
        required_str(pane, "pane_id")?;
        required_str(pane, "terminal_id")?;
        Ok(WorktreeLocation {
            pane: pane.clone(),
            workspace_id,
            worktree: ExternalSubagentWorktree {
                path: path.into(),
                branch: actual_branch.into(),
                repo_root: repo_root.into(),
            },
        })
    };
    parse().with_context(|| {
        format!(
            "Herdr created a worktree but returned invalid metadata; resources retained: {result}"
        )
    })
}
