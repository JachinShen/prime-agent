//! Primitive-only adoption events for external subagent lifecycles.
use crate::{Properties, TelemetryClient};

/// A bounded agent category; executable names and arguments never enter telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalAgentKind {
    Codex,
    Claude,
    Other,
}

impl ExternalAgentKind {
    #[must_use]
    pub fn from_agent(agent: &str) -> Self {
        match agent {
            "codex" => Self::Codex,
            "claude" => Self::Claude,
            _ => Self::Other,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Other => "other",
        }
    }
}

/// The settled outcome of one external turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalSubagentOutcome {
    Completed,
    Error,
    Cancelled,
}

impl ExternalSubagentOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Adoption events contain no task, answer, path, session id, or terminal content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalSubagentEvent {
    Started {
        agent_kind: ExternalAgentKind,
        worktree: bool,
    },
    Completed {
        agent_kind: ExternalAgentKind,
        outcome: ExternalSubagentOutcome,
        duration_ms: u64,
        answer_truncated: bool,
        output_truncated: bool,
    },
}

impl ExternalSubagentEvent {
    /// Queue delivery on the telemetry worker; never wait for a sink.
    pub fn track(self, client: &TelemetryClient) {
        let (name, properties) = self.properties();
        client.track(name, properties);
    }

    fn properties(self) -> (&'static str, Properties) {
        let mut properties = Properties::new();
        let (name, agent_kind) = match self {
            Self::Started {
                agent_kind,
                worktree,
            } => {
                properties.set("worktree", worktree.into());
                ("external subagent started", agent_kind)
            }
            Self::Completed {
                agent_kind,
                outcome,
                duration_ms,
                answer_truncated,
                output_truncated,
            } => {
                properties.set("outcome", outcome.as_str().into());
                properties.set("duration_ms", duration_ms.into());
                properties.set("answer_truncated", answer_truncated.into());
                properties.set("output_truncated", output_truncated.into());
                ("external subagent completed", agent_kind)
            }
        };
        properties.set("agent_kind", agent_kind.as_str().into());
        (name, properties)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn external_lifecycle_events_preserve_only_bounded_primitives() {
        let (name, mut properties) = ExternalSubagentEvent::Completed {
            agent_kind: ExternalAgentKind::from_agent("/secret/path/agent"),
            outcome: ExternalSubagentOutcome::Error,
            duration_ms: 42,
            answer_truncated: true,
            output_truncated: false,
        }
        .properties();
        assert_eq!(crate::sanitize(name, &mut properties), 0);
        assert_eq!(
            serde_json::to_value(properties).unwrap(),
            json!({
                "agent_kind": "other", "outcome": "error", "duration_ms": 42,
                "answer_truncated": true, "output_truncated": false,
            })
        );
    }
}
