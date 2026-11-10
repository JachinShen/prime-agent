//! External rows retain native roster behavior while actions address the parent worker.
use super::*;

fn external_mode() -> AgentsViewMode {
    let mut mode = mode_with_parent_and_child();
    let child = serde_json::json!({
        "id": "review", "rlmChildId": "review", "runtimeKind": "subagent",
        "sessionName": "API review", "backend": "herdr",
        "runtimeResident": true, "activity": "working", "taskState": "running",
        "activeSessionId": null, "sessionId": null,
        "parentActiveSessionId": "p-live", "parentSessionId": "p",
        "parentSessionPath": "/x/p.jsonl", "answerPreview": "Checked the API",
        "external": {"agent": "codex"}
    });
    mode.roster[1] = roster_entry("/x/p.jsonl#review", "running", &child);
    mode.expanded_parents.insert("file:/x/p.jsonl".to_string());
    mode.rebuild_rows();
    mode.selected = mode
        .rows
        .iter()
        .position(|row| row.summary["rlmChildId"] == "review" && row.kind == RowKind::Subagent)
        .expect("external child row");
    mode
}

#[test]
fn external_enter_focuses_via_real_parent_without_attaching_a_fake_session() {
    let mut mode = external_mode();
    mode.open_selected();
    assert_eq!(
        mode.pending_external_focus,
        Some(("p-live".to_string(), "review".to_string()))
    );
    assert!(mode.opened.is_none());
    assert!(mode.running);
}

#[test]
fn external_reply_arms_nested_row_and_targets_its_logical_child() {
    let mut mode = external_mode();
    mode.handle_key("space");
    let Composer::Reply(reply) = &mode.composer else {
        panic!("external reply composer");
    };
    assert_eq!(reply.target.key, "agent:/x/p.jsonl#review");
    assert!(mode.pending_headline.is_none());
    mode.disarm_reply_off_selected();
    assert!(matches!(mode.composer, Composer::Reply(_)));
    mode.handle_paste("Follow up on the errors");
    mode.handle_key("enter");
    let request = mode.pending_reply.take().expect("external reply dispatch");
    let (parent, child) = crate::agents_view::external::target(&request.summary)
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(crate::agents_view::external::command(
            parent,
            child,
            "send",
            Some(request.text)
        ))
        .unwrap(),
        serde_json::json!({"type":"control_external_subagent", "activeSessionId":"p-live",
            "target":"review", "action":"send", "message":"Follow up on the errors", "keys":[],"lines":80})
    );
}

#[test]
fn external_row_renders_agent_kind_without_inventing_model_or_cost() {
    let mut mode = external_mode();
    let (frame, _) = mode.render_frame(120, 24);
    let line = frame
        .iter()
        .map(flat)
        .find(|line| line.contains("[codex] API review"))
        .expect("external rendered label");
    assert!(!line.contains("$0.00"), "unknown external usage: {line}");
    assert_eq!(mode.rows[mode.selected].model, "-");
}

#[test]
fn external_idle_pane_stays_visible_in_the_dock_and_scoped_subagents_view() {
    let mut mode = external_mode();
    mode.roster[1]["status"] = serde_json::json!("idle");
    mode.roster[1]["summary"]["activity"] = serde_json::json!("idle");
    mode.roster[1]["summary"]["taskState"] = serde_json::json!("completed");
    let counts = crate::subagents::count_descendants(
        &mode.roster,
        &crate::subagents::SessionIdentity::new(
            Some("p-live".to_string()),
            Some("p".to_string()),
            Some("/x/p.jsonl".to_string()),
        ),
    );
    assert_eq!(
        counts,
        crate::subagents::SubagentCounts {
            total: 1,
            idle: 1,
            ..crate::subagents::SubagentCounts::default()
        }
    );
    let dock = crate::chrome::ActivityDock {
        subagents_running_direct: counts.running_direct,
        subagents_running_nested: counts.running_nested,
        subagents_idle: counts.idle,
        subagents_inactive: counts.inactive,
        ..crate::chrome::ActivityDock::default()
    };
    let (frame, segments) = crate::chrome::render_activity_dock_segments(&dock, &mode.theme, 100);
    let text = flat(&frame[1]);
    assert_eq!(text, " ◆ 1 subagent  ·  ◷ 0 heartbeats  ·  ▸ 0 shells");
    assert_eq!(
        text.chars()
            .skip(segments[0].cols.start)
            .take(segments[0].cols.len())
            .collect::<String>(),
        "◆ 1 subagent"
    );

    let mut scoped = scoped_mode(None, mode.roster);
    scoped.selected = scoped
        .rows
        .iter()
        .position(|row| row.summary["rlmChildId"] == "review")
        .expect("completed external pane in the scoped view");
    assert_eq!(scoped.rows[scoped.selected].kind, RowKind::Agent);
    let (frame, _) = scoped.render_frame(120, 24);
    assert!(frame
        .iter()
        .map(flat)
        .any(|line| line.contains("[codex] API review")));
    scoped.open_selected();
    assert_eq!(
        scoped.pending_external_focus,
        Some(("p-live".to_string(), "review".to_string()))
    );
}

#[test]
fn external_missing_parent_reports_focus_error() {
    let mut mode = external_mode();
    mode.rows[mode.selected].summary["parentActiveSessionId"] = Value::Null;
    mode.open_selected();
    assert!(mode.pending_external_focus.is_none());
    assert!(mode
        .status
        .as_ref()
        .unwrap()
        .text()
        .contains("parent runtime is unavailable"));
    assert!(mode.opened.is_none());
}

#[test]
fn same_named_external_children_keep_distinct_parent_qualified_identities() {
    let mode = external_mode();
    let mut other = mode.roster[1].clone();
    other["agentId"] = serde_json::json!("/x/other.jsonl#review");
    other["summary"]["parentActiveSessionId"] = serde_json::json!("other-live");
    other["summary"]["parentSessionId"] = serde_json::json!("other");
    other["summary"]["parentSessionPath"] = serde_json::json!("/x/other.jsonl");
    let mut roster = mode.roster;
    roster.push(other);
    let records = crate::agents_view_state::reconcile_unified_sessions(&roster, &[], None);
    assert_eq!(
        records.len(),
        3,
        "both logical review children survive reconciliation"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn hanging_external_focus_never_delays_the_next_frame() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let socket = dir.path().join("hanging.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (received_tx, received_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();
        writer.write_all(b"{\"type\":\"daemon_hello\",\"protocol\":{\"name\":\"prime-agent.daemon\",\"version\":7},\"clientId\":\"hanging-test\",\"serverCapabilities\":[]}\n").await.unwrap();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        assert!(reader.read_line(&mut line).await.unwrap() > 0);
        let envelope: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(envelope["command"]["action"], "focus");
        received_tx.send(()).unwrap();
        // Hold the real socket open forever without acknowledging the focus request.
        std::future::pending::<()>().await;
        drop(writer);
    });
    let (client, _events) = DaemonClient::connect(&socket).await.unwrap();
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
    let mut mode = external_mode();
    mode.open_selected();
    let focus = mode.pending_external_focus.take().unwrap();
    let dispatch = crate::agents_view::external::spawn_focus(&client, ui_tx, focus);
    tokio::time::timeout(Duration::from_secs(5), received_rx)
        .await
        .unwrap()
        .unwrap();
    assert!(!dispatch.is_finished(), "server has not answered");
    let (frame, _) = mode.render_frame(120, 24);
    assert!(frame
        .iter()
        .map(flat)
        .any(|line| line.contains("[codex] API review")));
    assert!(matches!(
        ui_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(
        !dispatch.is_finished(),
        "painting did not wait for the hanging response"
    );
    dispatch.abort();
    server.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn external_focus_and_reply_dispatch_use_parent_worker_daemon_wire() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let socket = dir.path().join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let hello = serde_json::json!({"type":"daemon_hello","protocol":{"name":"prime-agent.daemon","version":7},
            "clientId":"external-test","serverCapabilities":[]});
        writer
            .write_all(format!("{hello}\n").as_bytes())
            .await
            .unwrap();
        let mut commands = Vec::new();
        for _ in 0..2 {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).await.unwrap() > 0);
            let envelope: Value = serde_json::from_str(&line).unwrap();
            commands.push(envelope["command"].clone());
            let response = serde_json::json!({"type":"response","id":envelope["id"],
                "command":"control_external_subagent","success":true,"data":{"accepted":true}});
            writer
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
        }
        commands
    });
    let (client, _events) = DaemonClient::connect(&socket).await.unwrap();
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
    let mut mode = external_mode();
    mode.open_selected();
    let focus = mode.pending_external_focus.take().unwrap();
    crate::agents_view::external::spawn_focus(&client, ui_tx.clone(), focus)
        .await
        .unwrap();
    assert!(matches!(
        ui_rx.recv().await,
        Some(UiInput::ExternalResult(Ok(())))
    ));
    mode.handle_key("space");
    mode.handle_paste("Check the errors");
    mode.handle_key("enter");
    let reply = mode.pending_reply.take().unwrap();
    crate::agents_view::reply::spawn_reply_dispatch(&client, ui_tx, reply)
        .await
        .unwrap();
    assert!(matches!(
        ui_rx.recv().await,
        Some(UiInput::ReplyResult { outcome: Ok(_), .. })
    ));
    let commands = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        commands,
        vec![
            serde_json::json!({"type":"control_external_subagent","activeSessionId":"p-live","target":"review","action":"focus","keys":[],"lines":80}),
            serde_json::json!({"type":"control_external_subagent","activeSessionId":"p-live","target":"review","action":"send","keys":[],"lines":80,"message":"Check the errors"}),
        ]
    );
}
