//! Socket-level regression tests for RFC-0001's Herdr transport.

use super::*;
use tokio::io::AsyncBufReadExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;

fn endpoint() -> (tempfile::TempDir, UnixListener, HerdrClient) {
    let dir = tempfile::Builder::new()
        .prefix("pa-herdr-")
        .tempdir_in("/tmp")
        .unwrap();
    let path = dir.path().join("api.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let client = HerdrClient::new(path.to_string_lossy().into_owned());
    (dir, listener, client)
}

async fn incoming(stream: &mut UnixStream) -> Value {
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await.unwrap();
    serde_json::from_str(&line).unwrap()
}

#[tokio::test]
async fn reads_fragmented_response_and_preserves_result() {
    let (_dir, listener, client) = endpoint();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = incoming(&mut stream).await;
        assert_eq!(request["method"], "pane.split");
        assert_eq!(
            request["params"],
            json!({ "workspace_id": "w1", "focus": false })
        );
        let response = serde_json::to_vec(&json!({
            "id": request["id"],
            "result": { "type": "pane_split", "pane": { "pane_id": "w1:p2" } },
        }))
        .unwrap();
        for part in response.chunks(7) {
            stream.write_all(part).await.unwrap();
            tokio::task::yield_now().await;
        }
        stream.write_all(b"\n").await.unwrap();
    });
    let result = client
        .request(
            "pane.split",
            json!({ "workspace_id": "w1", "focus": false }),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(
        result,
        json!({ "type": "pane_split", "pane": { "pane_id": "w1:p2" } })
    );
    server.await.unwrap();
}

#[tokio::test]
async fn api_refusals_remain_typed_through_context() {
    let (_dir, listener, client) = endpoint();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = incoming(&mut stream).await;
        let response = json!({
            "id": request["id"],
            "error": { "code": "agent_blocked", "message": "approval required" },
        });
        stream
            .write_all(format!("{response}\n").as_bytes())
            .await
            .unwrap();
    });
    let error = client
        .request("agent.prompt", json!({}), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<HerdrApiError>(),
        Some(&HerdrApiError {
            code: "agent_blocked".into(),
            message: "approval required".into(),
        })
    );
    server.await.unwrap();
}

#[tokio::test]
async fn rejects_wrong_id_invalid_json_and_truncated_frames() {
    for response in [
        b"{\"id\":\"different\",\"result\":{}}\n".as_slice(),
        b"{invalid}\n".as_slice(),
        b"{\"id\":\"truncated\"}".as_slice(),
        b"[]\n".as_slice(),
    ] {
        let (_dir, listener, client) = endpoint();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            incoming(&mut stream).await;
            stream.write_all(response).await.unwrap();
        });
        assert!(client
            .request("agent.get", json!({}), Duration::from_secs(5))
            .await
            .is_err());
        server.await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn request_timeout_covers_a_server_that_never_replies() {
    let (_dir, listener, client) = endpoint();
    let (ready_tx, ready_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        incoming(&mut stream).await;
        ready_tx.send(()).unwrap();
        release_rx.await.unwrap();
        drop(stream);
    });
    let request = tokio::spawn(async move {
        client
            .request("agent.get", json!({}), Duration::from_secs(30))
            .await
    });
    ready_rx.await.unwrap();
    tokio::time::advance(Duration::from_secs(31)).await;
    let error = request.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("timed out"));
    release_tx.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn subscription_timeout_retains_partial_event_for_the_next_read() {
    let (_dir, listener, client) = endpoint();
    let (release_tx, release_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = incoming(&mut stream).await;
        assert_eq!(request["method"], "events.subscribe");
        let acknowledgement = json!({
            "id": request["id"],
            "result": { "type": "subscription_started" },
        });
        stream
            .write_all(format!("{acknowledgement}\n").as_bytes())
            .await
            .unwrap();
        stream.write_all(b"{\"event\":\"pane.agent_").await.unwrap();
        release_rx.await.unwrap();
        stream
            .write_all(
                b"status_changed\",\"data\":{\"pane_id\":\"w1:p2\",\"agent_status\":\"done\"}}\n",
            )
            .await
            .unwrap();
    });
    let mut subscription = client
        .subscribe(
            vec![json!({ "type": "pane.agent_status_changed", "pane_id": "w1:p2" })],
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(subscription.next(Duration::from_secs(5)).await.is_err());
    release_tx.send(()).unwrap();
    assert_eq!(
        subscription.next(Duration::from_secs(5)).await.unwrap(),
        json!({
            "event": "pane.agent_status_changed",
            "data": { "pane_id": "w1:p2", "agent_status": "done" },
        })
    );
    server.await.unwrap();
}

#[test]
fn validates_the_explicit_session_context() {
    let mut env = BTreeMap::from([
        ("HERDR_ENV".into(), "1".into()),
        ("HERDR_SOCKET_PATH".into(), "/tmp/herdr.sock".into()),
        ("HERDR_PANE_ID".into(), "w1:p1".into()),
        ("HERDR_WORKSPACE_ID".into(), "w1".into()),
    ]);
    let client = HerdrClient::from_env(&env).unwrap();
    assert_eq!(
        (client.pane_id(), client.workspace_id()),
        (Some("w1:p1"), Some("w1"))
    );
    env.remove("HERDR_WORKSPACE_ID");
    assert!(HerdrClient::from_env(&env).is_err());
    env.insert("HERDR_ENV".into(), "0".into());
    assert!(HerdrClient::from_env(&env).is_err());
}
