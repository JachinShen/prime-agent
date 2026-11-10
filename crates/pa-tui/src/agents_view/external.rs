//! External children are addressed through the parent worker's daemon route.
use super::{DaemonClient, DaemonCommand, StatusTone, UiInput, Value};
use tokio::sync::mpsc;

/// Return the real parent runtime and logical child id, never a synthetic session id.
pub(super) fn target(summary: &Value) -> Result<Option<(String, String)>, String> {
    if summary.get("backend").and_then(Value::as_str) != Some("herdr") {
        return Ok(None);
    }
    let field = |key| {
        summary
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    };
    let parent = field("parentActiveSessionId")
        .ok_or_else(|| "External subagent's parent runtime is unavailable".to_string())?;
    let child = field("rlmChildId")
        .ok_or_else(|| "External subagent has no logical child id".to_string())?;
    Ok(Some((parent.to_string(), child.to_string())))
}

pub(super) fn command(
    parent: String,
    target: String,
    action: &str,
    message: Option<String>,
) -> DaemonCommand {
    DaemonCommand::ControlExternalSubagent {
        id: None,
        active_session_id: parent,
        target,
        action: action.to_string(),
        keys: Vec::new(),
        lines: 80,
        message,
        rest: serde_json::Map::default(),
    }
}

pub(super) fn spawn_focus(
    client: &DaemonClient,
    ui_tx: mpsc::UnboundedSender<UiInput>,
    (parent, child): (String, String),
) -> tokio::task::JoinHandle<()> {
    let client = client.clone();
    tokio::spawn(async move {
        let result = match client.request(command(parent, child, "focus", None)).await {
            Ok(response) if response.success => Ok(()),
            Ok(response) => Err(response
                .error
                .unwrap_or_else(|| "External focus failed".to_string())),
            Err(error) => Err(format!("{error:#}")),
        };
        let _ = ui_tx.send(UiInput::ExternalResult(result));
    })
}

impl super::AgentsViewMode {
    pub(super) fn focus_external(&mut self, summary: &Value) -> bool {
        match target(summary) {
            Ok(Some(target)) => {
                self.pending_external_focus = Some(target);
                self.set_status("Focusing external subagent...");
                true
            }
            Ok(None) => false,
            Err(error) => {
                self.set_status_tone(&error, StatusTone::Error);
                true
            }
        }
    }
}
