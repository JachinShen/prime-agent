//! Authenticated notice retries, recovery evidence and failed admission rollback.

use super::*;
use crate::journal::WorkerRecoveryJournal;
use crate::worker::WorkerConfig;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

fn worker_in(root: &Path, transcript: &Path) -> Arc<Worker> {
    let worker = Arc::new(Worker::new(
        WorkerConfig {
            socket_path: root.join("worker.sock"),
            supervisor_socket_path: root.join("supervisor.sock"),
            token: "token".into(),
            worker_instance_id: String::new(),
            active_session_id: "parent".into(),
            agent_dir: root.join("agent"),
            recovery_journal_path: root.join("recovery.jsonl"),
            telemetry_disabled: Some(true),
            script: Some(json!({"responses": ["ack"]})),
        },
        None,
    ));
    let store = if transcript.exists() {
        SessionFile::open(transcript).unwrap()
    } else {
        let mut store = SessionFile::create("/tmp", None, 0);
        store.set_path(transcript.to_path_buf());
        store
            .persist_entry("session_info", json!({"name":"parent"}))
            .unwrap();
        store
    };
    {
        let mut core = worker.core.lock().unwrap();
        core.created = true;
        core.store = Some(store);
    }
    *worker.recovery.lock().unwrap() =
        Some(WorkerRecoveryJournal::open(&worker.config.recovery_journal_path).unwrap());
    worker
}

fn notice(id: &str) -> Value {
    json!({
        "role":"custom", "customType":"rlm_child_terminal_notice", "content":"external done",
        "display":true, "details":{"backend":"herdr", "externalNoticeId":id},
    })
}

fn deliver(worker: &Worker, row: &Value) -> DaemonResponse {
    worker.handle_queue(
        &json!({
            "message":"external done", "customMessage":row,
            "rlmNoticeNonce":crate::child_status_notices::mint(),
        }),
        Lane::FollowUp,
    )
}

fn wake_pending(worker: &Worker) -> bool {
    let mut notification = Box::pin(worker.work_notify.notified());
    matches!(
        notification
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    )
}

#[tokio::test]
async fn notice_replay_does_not_enqueue_or_wake_during_queue_handoff() {
    let root = tempfile::tempdir().unwrap();
    let worker = worker_in(root.path(), &root.path().join("session.jsonl"));
    assert!(deliver(&worker, &notice("sub-1:1:completed")).success);
    assert!(
        !wake_pending(&worker),
        "the first admission wakes the parent"
    );
    {
        let mut core = worker.core.lock().unwrap();
        core.follow_up.pop_front().unwrap();
        core.busy = true;
        core.queued_input_suspended = true;
    }
    let response = deliver(&worker, &notice("sub-1:1:completed"));
    assert!(response.success, "{response:?}");
    assert_eq!(response.data, Some(json!({"queued":true,"duplicate":true})));
    let core = worker.core.lock().unwrap();
    assert!(core.follow_up.is_empty());
    assert!(core.queued_input_suspended);
    drop(core);
    assert!(
        wake_pending(&worker),
        "a duplicate must not wake the parent"
    );
    assert!(deliver(&worker, &notice("sub-1:2:completed")).success);
    assert_eq!(worker.core.lock().unwrap().follow_up.len(), 1);
}

#[tokio::test]
async fn recovery_keeps_pending_notices_once_and_drops_already_delivered_notices() {
    let root = tempfile::tempdir().unwrap();
    let transcript = root.path().join("session.jsonl");
    let worker = worker_in(root.path(), &transcript);
    let completed = notice("sub-1:1:completed");
    assert!(deliver(&worker, &completed).success);
    assert!(deliver(&worker, &notice("sub-2:1:completed")).success);
    worker
        .core
        .lock()
        .unwrap()
        .store
        .as_mut()
        .unwrap()
        .persist_entry(
            "custom_message",
            json!({
                "customType":completed["customType"], "details":completed["details"],
                "content":completed["content"], "display":true,
            }),
        )
        .unwrap();
    *worker.recovery.lock().unwrap() = None;
    let restored = worker_in(root.path(), &transcript);
    let (mut steering, mut follow_up) = {
        let recovery = restored.recovery.lock().unwrap();
        super::super::queue::restore_queue_snapshot(recovery.as_ref().unwrap(), "parent")
    };
    restored
        .restore_external_notice_dedup(
            restored.core.lock().unwrap().store.as_ref().unwrap(),
            &mut steering,
            &mut follow_up,
        )
        .unwrap();
    assert_eq!(follow_up.len(), 1);
    assert_eq!(
        external_notice_id(follow_up[0].custom_message.as_ref().unwrap()).unwrap(),
        Some("sub-2:1:completed")
    );
    restored.core.lock().unwrap().follow_up = follow_up;
    assert!(deliver(&restored, &completed).success);
    assert_eq!(restored.core.lock().unwrap().follow_up.len(), 1);
    assert!(wake_pending(&restored));
}

#[tokio::test]
async fn failed_checkpoint_rolls_back_the_queue_and_allows_a_visible_retry() {
    let root = tempfile::tempdir().unwrap();
    let worker = worker_in(root.path(), &root.path().join("session.jsonl"));
    worker.core.lock().unwrap().queued_input_suspended = true;
    std::fs::create_dir(&worker.config.recovery_journal_path).unwrap();
    let failed = deliver(&worker, &notice("sub-1:1:completed"));
    assert!(!failed.success);
    assert!(failed
        .error
        .unwrap()
        .contains("persist external child notice admission"));
    {
        let core = worker.core.lock().unwrap();
        assert!(core.follow_up.is_empty());
        assert!(core.queued_input_suspended);
    }
    assert!(wake_pending(&worker));
    std::fs::remove_dir(&worker.config.recovery_journal_path).unwrap();
    assert!(deliver(&worker, &notice("sub-1:1:completed")).success);
    assert_eq!(worker.core.lock().unwrap().follow_up.len(), 1);
}

#[tokio::test]
async fn malformed_evidence_and_unauthenticated_notices_are_explicit_errors() {
    let root = tempfile::tempdir().unwrap();
    let transcript = root.path().join("session.jsonl");
    let worker = worker_in(root.path(), &transcript);
    let untrusted = worker.handle_queue(
        &json!({"customMessage":notice("sub-1:1:completed")}),
        Lane::FollowUp,
    );
    assert!(!untrusted.success);
    let mut malformed = notice("sub-1:1:completed");
    malformed["details"]
        .as_object_mut()
        .unwrap()
        .remove("externalNoticeId");
    assert!(!deliver(&worker, &malformed).success);
    std::fs::write(&transcript, b"broken-json\n").unwrap();
    let failed = deliver(&worker, &notice("sub-1:1:completed"));
    assert!(!failed.success);
    assert!(failed
        .error
        .unwrap()
        .contains("invalid external notice transcript evidence"));
    assert!(worker.core.lock().unwrap().follow_up.is_empty());
    assert!(wake_pending(&worker));
}

#[tokio::test]
async fn dedup_does_not_apply_to_native_notices_or_unrelated_custom_rows() {
    let root = tempfile::tempdir().unwrap();
    let worker = worker_in(root.path(), &root.path().join("session.jsonl"));
    let mut native = notice("sub-1:1:completed");
    native["details"].as_object_mut().unwrap().remove("backend");
    assert!(deliver(&worker, &native).success);
    assert!(deliver(&worker, &native).success);
    let mut unrelated = notice("sub-1:1:completed");
    unrelated["customType"] = json!("unrelated");
    assert!(deliver(&worker, &unrelated).success);
    assert!(deliver(&worker, &notice("sub-1:1:completed")).success);
    assert_eq!(worker.core.lock().unwrap().follow_up.len(), 4);
}
