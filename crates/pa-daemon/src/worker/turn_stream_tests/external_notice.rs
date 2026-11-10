//! Failed external notice persistence remains pending and reports a failed turn.
use super::*;

#[tokio::test]
async fn an_external_notice_append_failure_is_visible_and_retains_recoverable_work() {
    let root = tempfile::tempdir().unwrap();
    let transcript = root.path().join("session.jsonl");
    let engine: Arc<dyn SessionEngine> = Arc::new(
        ScriptedEngine::from_value(&json!({"responses":["notice acknowledged"]})).unwrap(),
    );
    let runner = burst_runner(Arc::clone(&engine));
    let mut store = crate::session_store::SessionFile::create("/tmp", None, 0);
    store.set_path(transcript.clone());
    store
        .persist_entry("session_info", json!({"name":"parent"}))
        .unwrap();
    runner.core.lock().unwrap().store = Some(store);
    *runner.recovery.lock().unwrap() =
        Some(WorkerRecoveryJournal::open(&root.path().join("recovery.jsonl")).unwrap());
    let row = json!({
        "role":"custom", "customType":"rlm_child_terminal_notice", "content":"external done",
        "display":true,"details":{"backend":"herdr","externalNoticeId":"sub-1:1:completed"},
    });
    let (done_tx, done_rx) = oneshot::channel();
    runner.core.lock().unwrap().follow_up.push_back(QueuedItem {
        priority: QueuePriority::Background,
        preview: None,
        message: "external done".into(),
        custom_message: Some(row.clone()),
        agent_message: None,
        queue_key: None,
        admission_id: None,
        images: Vec::new(),
        done: Some(done_tx),
        queue_visible: true,
        policy: TurnPolicy::Queued,
        forced_batch: false,
    });
    checkpoint_queue_recovery(
        &runner.recovery,
        &runner.core,
        QueueCheckpoint::Admitted {
            operation: "external_notice_queued",
        },
    );
    let item = runner.core.lock().unwrap().follow_up.pop_front().unwrap();
    std::fs::remove_file(&transcript).unwrap();
    std::fs::create_dir(&transcript).unwrap();
    let mut subscription = runner.events.subscribe();
    runner.run_turn(Arc::clone(&engine), vec![item]).await;
    let failure = done_rx.await.unwrap();
    assert!(
        matches!(failure, TurnSettle::Failed(ref error) if error.contains("Persist external child notice")),
        "{failure:?}"
    );
    {
        let core = runner.core.lock().unwrap();
        assert!(core.queued_input_suspended);
        assert!(!core.busy);
        assert_eq!(core.follow_up.len(), 1);
        assert_eq!(core.follow_up[0].custom_message, Some(row.clone()));
    }
    let mut events = Vec::new();
    while let Ok(frame) = subscription.try_recv() {
        let outbound: Value = serde_json::from_slice(&frame.payload).unwrap();
        events.push(outbound["event"].clone());
    }
    assert!(events.iter().any(|event| event["type"] == "turn_end"
        && event["error"]
            .as_str()
            .is_some_and(|error| error.contains("Persist external child notice"))));
    assert!(!events
        .iter()
        .any(|event| event["message"]["role"] == "assistant"));
    let recovered = {
        let recovery = runner.recovery.lock().unwrap();
        restore_queue_snapshot(recovery.as_ref().unwrap(), "burst-session")
    };
    assert!(recovered.0.is_empty());
    assert_eq!(recovered.1.len(), 1);
    assert_eq!(recovered.1[0].custom_message, Some(row));

    // Repairing the transcript and explicitly resuming consumes the retained notice.
    std::fs::remove_dir(&transcript).unwrap();
    let item = {
        let mut core = runner.core.lock().unwrap();
        core.queued_input_suspended = false;
        core.follow_up.pop_front().unwrap()
    };
    runner.run_turn(engine, vec![item]).await;
    let recovery = runner.recovery.lock().unwrap();
    let recovered = restore_queue_snapshot(recovery.as_ref().unwrap(), "burst-session");
    assert!(recovered.0.is_empty() && recovered.1.is_empty());
}
