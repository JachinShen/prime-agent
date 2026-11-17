use super::*;
use serde_json::json;

fn event(session: &str, event: Value) -> DaemonClientEvent {
    DaemonClientEvent::SessionEvent {
        active_session_id: session.to_owned(),
        event,
        meta_sequence: 1,
    }
}

#[test]
fn fast_turn_reports_working_and_done_once_then_clears() {
    let mut status = ProgramStatus::with_output(Some(Vec::new()));
    status.publish(State::Idle).unwrap();
    status.observe("s1", &event("s1", json!({"type":"turn_start"})));
    status.publish(State::Working).unwrap();
    status.publish(State::Working).unwrap();
    status.observe("s1", &event("s1", json!({"type":"turn_end"})));
    status.publish(status.completion).unwrap();
    status.observe("s1", &event("s1", json!({"type":"agent_end"})));
    status.publish(status.completion).unwrap();
    status.clear().unwrap();
    status.clear().unwrap();
    assert_eq!(
        status.output.as_deref().unwrap(),
        concat!(
            "\x1b]7501;state=idle:app=prime-agent\x1b\\",
            "\x1b]7501;state=working:app=prime-agent\x1b\\",
            "\x1b]7501;state=done:app=prime-agent\x1b\\",
            "\x1b]7501;state=clear\x1b\\"
        )
        .as_bytes()
    );
}

#[test]
fn cancellation_and_provider_errors_survive_turn_and_agent_end() {
    for (stop_reason, expected) in [("aborted", State::Idle), ("error", State::Error)] {
        let mut status = ProgramStatus::with_output(Some(Vec::new()));
        for payload in [
            json!({"type":"turn_start"}),
            json!({"type":"message_end", "message":{"role":"assistant", "stopReason":stop_reason}}),
            json!({"type":"turn_end", "error":"turn stopped"}),
            json!({"type":"agent_end"}),
        ] {
            status.observe("s1", &event("s1", payload));
        }
        assert_eq!(status.completion, expected);
    }
}

#[test]
fn retry_and_compaction_failures_clear_on_next_run() {
    for payload in [
        json!({"type":"turn_end", "error":"provider failed"}),
        json!({"type":"auto_retry_end", "finalError":"exhausted"}),
        json!({"type":"compaction_end", "errorMessage":"failed"}),
    ] {
        let mut status = ProgramStatus::with_output(Some(Vec::new()));
        status.observe("s1", &event("s1", json!({"type":"turn_start"})));
        status.observe("s1", &event("s1", payload));
        assert_eq!(status.completion, State::Error);
        status.observe("s1", &event("s1", json!({"type":"auto_retry_start"})));
        assert_eq!(status.completion, State::Done);
    }
}

#[test]
fn shared_daemon_events_do_not_change_another_attached_session() {
    let mut status = ProgramStatus::with_output(Some(Vec::new()));
    status.observe("s1", &event("s1", json!({"type":"turn_start"})));
    status.observe(
        "s1",
        &event("s2", json!({"type":"turn_end", "error":"other session"})),
    );
    assert_eq!(status.completion, State::Done);
    status.observe("s2", &event("s1", json!({"type":"turn_start"})));
    assert_eq!(status.completion, State::Idle);
}

#[test]
fn disabled_output_never_writes_even_on_exit() {
    let mut status = ProgramStatus::<Vec<u8>>::with_output(None);
    status.publish(State::Working).unwrap();
    status.clear().unwrap();
    assert_eq!((status.output.as_ref(), status.last), (None, None));
}

#[test]
fn write_failures_are_returned_without_advancing_the_reported_state() {
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("closed terminal"))
        }
        fn flush(&mut self) -> io::Result<()> {
            panic!("cannot flush after failed write")
        }
    }
    let mut status = ProgramStatus::with_output(Some(Broken));
    let error = status.publish(State::Working).unwrap_err();
    assert!(format!("{error:#}").contains("closed terminal"));
    assert_eq!(status.last, None);
}

#[test]
fn flush_failures_are_returned_without_advancing_the_reported_state() {
    struct BrokenFlush;
    impl Write for BrokenFlush {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("terminal flush failed"))
        }
    }
    let mut status = ProgramStatus::with_output(Some(BrokenFlush));
    let error = status.publish(State::Working).unwrap_err();
    assert!(format!("{error:#}").contains("terminal flush failed"));
    assert_eq!(status.last, None);
}

#[test]
fn an_early_return_clears_the_terminal_report() {
    let mut output = Vec::new();
    {
        let mut status = ProgramStatus::with_output(Some(&mut output));
        status.publish(State::Working).unwrap();
    }
    assert_eq!(
        output,
        b"\x1b]7501;state=working:app=prime-agent\x1b\\\x1b]7501;state=clear\x1b\\"
    );
}

#[test]
fn confirmation_and_authentication_block_then_restore_the_active_turn() {
    use crate::{
        auth_panel::AuthPanel,
        confirm::ConfirmPanel,
        theme::{ColorMode, Theme},
    };

    let mut view = AgentView::new(Theme::builtin("prime", ColorMode::TrueColor));
    let mut status = ProgramStatus::with_output(Some(Vec::new()));
    let turn_active = true;
    status.refresh_view(turn_active, &view).unwrap();
    view.confirm = Some(ConfirmPanel::yes_no("Confirm", "Continue?"));
    status.refresh_view(turn_active, &view).unwrap();
    view.confirm = None;
    status.refresh_view(turn_active, &view).unwrap();
    view.auth_panel = Some(AuthPanel::new("Sign in"));
    status.refresh_view(turn_active, &view).unwrap();
    view.auth_panel = None;
    status.refresh_view(turn_active, &view).unwrap();
    assert_eq!(
        status.output.as_deref().unwrap(),
        concat!(
            "\x1b]7501;state=working:app=prime-agent\x1b\\",
            "\x1b]7501;state=blocked:app=prime-agent:kind=question\x1b\\",
            "\x1b]7501;state=working:app=prime-agent\x1b\\",
            "\x1b]7501;state=blocked:app=prime-agent:kind=auth\x1b\\",
            "\x1b]7501;state=working:app=prime-agent\x1b\\"
        )
        .as_bytes()
    );
}
