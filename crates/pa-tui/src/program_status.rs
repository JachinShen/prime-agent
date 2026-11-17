//! Herdr desktop status belongs to the attached terminal client, not the shared daemon.
//! OSC 7501 is consumed by herdr-core; headless and redirected output stay untouched.

use std::io::{self, IsTerminal, Write};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::{daemon_client::DaemonClientEvent, session_ui::SessionUi, view::AgentView};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    Working,
    Done,
    Error,
    Question,
    Auth,
}

pub(crate) struct ProgramStatus<W: Write = io::Stdout> {
    output: Option<W>,
    session_id: String,
    completion: State,
    cancelled: bool,
    last: Option<State>,
}

impl ProgramStatus {
    pub(crate) fn for_terminal() -> Option<Self> {
        let stdout = io::stdout();
        let enabled = stdout.is_terminal()
            && std::env::var_os("HC_TERMINAL_ID").is_some_and(|id| !id.is_empty());
        enabled.then(|| Self::with_output(Some(stdout)))
    }
}

impl<W: Write> ProgramStatus<W> {
    fn with_output(output: Option<W>) -> Self {
        Self {
            output,
            session_id: String::new(),
            completion: State::Idle,
            cancelled: false,
            last: None,
        }
    }

    pub(crate) fn apply_client_event(
        &mut self,
        session: &mut SessionUi,
        event: DaemonClientEvent,
        view: &mut AgentView,
    ) -> Result<()> {
        self.observe(&session.active_session_id, &event);
        session.apply_client_event(event, view);
        // Publish each event, even when several frames are drained in one UI batch. Otherwise a
        // fast working -> done turn disappears and the core cannot advance its turn counter.
        self.refresh(session, view)
    }

    fn observe(&mut self, active_session_id: &str, event: &DaemonClientEvent) {
        if self.session_id != active_session_id {
            active_session_id.clone_into(&mut self.session_id);
            self.completion = State::Idle;
            self.cancelled = false;
        }
        if let DaemonClientEvent::SessionEvent {
            active_session_id: event_session_id,
            event,
            ..
        } = event
        {
            if event_session_id != active_session_id {
                return;
            }
            match event.get("type").and_then(Value::as_str) {
                Some("agent_start" | "turn_start" | "auto_retry_start" | "compaction_start") => {
                    self.completion = State::Done;
                    self.cancelled = false;
                }
                Some("message_end") if event["message"]["role"].as_str() == Some("assistant") => {
                    match event["message"]["stopReason"].as_str() {
                        Some("aborted") => {
                            self.completion = State::Idle;
                            self.cancelled = true;
                        }
                        Some("error") => self.completion = State::Error,
                        _ => {}
                    }
                }
                Some("turn_end") => {
                    if self.cancelled {
                        self.completion = State::Idle;
                    } else if event.get("error").is_some_and(Value::is_string) {
                        self.completion = State::Error;
                    } else if self.completion != State::Error {
                        self.completion = State::Done;
                    }
                }
                Some("auto_retry_end") => {
                    if event.get("finalError").is_some_and(Value::is_string) {
                        self.completion = State::Error;
                    }
                }
                Some("compaction_end") => {
                    if event["aborted"].as_bool() == Some(true) {
                        self.completion = State::Idle;
                        self.cancelled = true;
                    } else if event.get("errorMessage").is_some_and(Value::is_string) {
                        self.completion = State::Error;
                    }
                }
                _ => {}
            }
        } else if let DaemonClientEvent::SessionClosed {
            active_session_id: event_session_id,
            ..
        } = event
        {
            if event_session_id == active_session_id {
                self.completion = State::Error;
            }
        }
    }

    pub(crate) fn refresh(&mut self, session: &SessionUi, view: &AgentView) -> Result<()> {
        if self.session_id != session.active_session_id {
            self.session_id.clone_from(&session.active_session_id);
            self.completion = State::Idle;
            self.cancelled = false;
        }
        self.refresh_view(session.turn_active, view)
    }

    fn refresh_view(&mut self, turn_active: bool, view: &AgentView) -> Result<()> {
        let state = if view.auth_panel.is_some() || view.provider_auth.is_some() {
            State::Auth
        } else if view.confirm.is_some() {
            State::Question
        } else if turn_active || view.retry.is_some() || view.compaction.is_some() {
            State::Working
        } else {
            self.completion
        };
        self.publish(state)
    }

    fn publish(&mut self, state: State) -> Result<()> {
        if self.last == Some(state) {
            return Ok(());
        }
        if let Some(output) = &mut self.output {
            let fields = match state {
                State::Idle => "state=idle:app=prime-agent",
                State::Working => "state=working:app=prime-agent",
                State::Done => "state=done:app=prime-agent",
                State::Error => "state=error:app=prime-agent",
                State::Question => "state=blocked:app=prime-agent:kind=question",
                State::Auth => "state=blocked:app=prime-agent:kind=auth",
            };
            write!(output, "\x1b]7501;{fields}\x1b\\")
                .and_then(|()| output.flush())
                .context("could not report Prime Agent status to Herdr")?;
            self.last = Some(state);
        }
        Ok(())
    }

    pub(crate) fn clear(&mut self) -> Result<()> {
        if self.last.is_some() {
            if let Some(output) = &mut self.output {
                output
                    .write_all(b"\x1b]7501;state=clear\x1b\\")
                    .and_then(|()| output.flush())
                    .context("could not clear Prime Agent status in Herdr")?;
            }
            self.last = None;
        }
        Ok(())
    }
}

impl<W: Write> Drop for ProgramStatus<W> {
    fn drop(&mut self) {
        // The explicit exit clears fallibly; this guard also covers early returns and unwinding.
        if let Err(error) = self.clear() {
            eprintln!("{error:#}");
        }
    }
}

#[cfg(test)]
mod tests;
