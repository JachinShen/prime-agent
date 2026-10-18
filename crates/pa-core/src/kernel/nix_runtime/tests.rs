use super::evaluator::{string, valid_name, Evaluator};
use serde_json::json;
use std::sync::{atomic::AtomicU32, Arc};

#[test]
fn escapes_interpolation_and_rejects_reserved_names() {
    assert_eq!(string("中文\n${oops}\\\""), "\"中文\\n\\${oops}\\\\\\\"\"");
    for name in ["", "if", "builtins", "x.y", "x;", "中文", "0x"] {
        assert!(!valid_name(name), "{name}");
    }
    for name in ["result", "a-b", "_private", "x'"] {
        assert!(valid_name(name), "{name}");
    }
}

// Requires the same Nix installation as the runtime itself. No Python is involved.
#[tokio::test]
async fn real_nix_live_bindings_unicode_errors_and_safe_snapshot() {
    let pid = Arc::new(AtomicU32::new(0));
    let mut evaluator = Evaluator::start(&pid).await.unwrap();
    evaluator
        .bind("result", &json!({"stdout":"中文🌸${literal}","exitCode":0}))
        .await
        .unwrap();
    let parsed = evaluator.parse("result.stdout").await.unwrap();
    assert_eq!(
        evaluator.evaluate(&parsed).await.unwrap(),
        "\"中文🌸\\${literal}\""
    );
    assert!(evaluator.parse("[").await.is_err());
    assert!(evaluator.evaluate("throw \"expected\"").await.is_err());
    assert_eq!(evaluator.evaluate("6 * 7").await.unwrap(), "42");
    evaluator.bind("scalar", &json!(17)).await.unwrap();
    evaluator.literals.insert("scalar".into(), json!(17));
    evaluator
        .evaluate("lazy = throw \"do not force\"")
        .await
        .unwrap();
    evaluator.names.insert("lazy".into());
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("snapshot.json");
    let snapshot = evaluator.state(&json!({"type":"snapshot","path":path,"manifest_path":temp.path().join("manifest.json")})).await.unwrap();
    assert_eq!(snapshot["saved"], json!(["scalar"]));
    assert_eq!(crate::platform::perms::file_mode(&path), Some(0o600));
    assert_eq!(
        crate::platform::perms::file_mode(&temp.path().join("manifest.json")),
        Some(0o600)
    );
    let payload: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        payload,
        json!({"format":"prime-agent-nix-literals-v1","values":{"scalar":17}})
    );
    evaluator.bind("scalar", &json!(0)).await.unwrap();
    evaluator
        .state(&json!({"type":"restore","path":path}))
        .await
        .unwrap();
    assert_eq!(evaluator.evaluate("scalar").await.unwrap(), "17");
    evaluator.close().await.unwrap();
}

#[tokio::test]
async fn named_host_results_reuse_and_failed_commands_bind_before_error() {
    use super::runtime::{execute, Control};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::{mpsc, Mutex};
    let pid = Arc::new(AtomicU32::new(0));
    let mut evaluator = Evaluator::start(&pid).await.unwrap();
    let control = Control {
        pid: AtomicU32::new(pid.load(Ordering::SeqCst)),
        interrupted: AtomicBool::new(false),
        changed: tokio::sync::Notify::new(),
        host: Mutex::new(None),
        requests: std::sync::Mutex::default(),
    };
    let output = Arc::new(Mutex::new(tokio::io::stdout()));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let invalid = execute(
        &mut evaluator,
        ":run if \"touch /should-not-exist\"",
        &control,
        &output,
        &mut rx,
    )
    .await;
    assert!(invalid.is_err());
    assert!(control.host.lock().await.is_none());
    let operation = execute(
        &mut evaluator,
        ":run captured \"printf test\"",
        &control,
        &output,
        &mut rx,
    );
    let reply = async {
        loop {
            if let Some(id) = control.host.lock().await.clone() {
                tx.send(json!({"id":id,"data":{"status":"ok","result":{"stdout":"partial🌸","stderr":"bad","exitCode":7}}})).unwrap();
                break;
            }
            tokio::task::yield_now().await;
        }
    };
    let (result, ()) = tokio::join!(operation, reply);
    assert!(result.is_err());
    assert_eq!(evaluator.evaluate("captured.exitCode").await.unwrap(), "7");
    assert_eq!(
        evaluator.evaluate("captured.stdout").await.unwrap(),
        "\"partial🌸\""
    );
    for reply_value in [
        json!({"stdout":"partial","stderr":"","exitCode":null}),
        json!({"stdout":"","stderr":""}),
        json!({"exitCode":"0"}),
    ] {
        let operation = execute(
            &mut evaluator,
            r#":run captured "test""#,
            &control,
            &output,
            &mut rx,
        );
        let reply = async {
            loop {
                if let Some(id) = control.host.lock().await.clone() {
                    tx.send(json!({"id":id,"data":{"status":"ok","result":reply_value}}))
                        .unwrap();
                    break;
                }
                tokio::task::yield_now().await;
            }
        };
        let (result, ()) = tokio::join!(operation, reply);
        assert!(result.is_err());
        let actual = evaluator
            .evaluate(":p (builtins.toJSON captured)")
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&actual).unwrap(),
            reply_value
        );
    }
    let operation = execute(
        &mut evaluator,
        r#":run first "printf first""#,
        &control,
        &output,
        &mut rx,
    );
    let reply = async {
        loop {
            if let Some(id) = control.host.lock().await.clone() {
                tx.send(json!({"id":id,"data":{"status":"ok","result":{"stdout":"preserved","stderr":"","exitCode":0}}})).unwrap();
                break;
            }
            tokio::task::yield_now().await;
        }
    };
    let (result, ()) = tokio::join!(operation, reply);
    assert!(result.is_ok());
    let operation = execute(
        &mut evaluator,
        ":run second first.stdout",
        &control,
        &output,
        &mut rx,
    );
    let reply = async {
        loop {
            if let Some(id) = control.host.lock().await.clone() {
                tx.send(json!({"id":id,"data":{"status":"ok","result":{"stdout":"replacement","stderr":"","exitCode":0}}})).unwrap();
                break;
            }
            tokio::task::yield_now().await;
        }
    };
    let (result, ()) = tokio::join!(operation, reply);
    assert!(result.is_ok());
    let names = evaluator
        .evaluate(":p (builtins.toJSON [ first.stdout second.stdout lastHostReply.result.stdout ])")
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&names).unwrap(),
        json!(["preserved", "replacement", "replacement"])
    );
    evaluator.close().await.unwrap();
}

#[tokio::test]
async fn write_binds_metadata_and_preserves_binding_on_failure() {
    use super::runtime::{execute, Control};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::{mpsc, Mutex};

    let pid = Arc::new(AtomicU32::new(0));
    let mut evaluator = Evaluator::start(&pid).await.unwrap();
    let control = Control {
        pid: AtomicU32::new(pid.load(Ordering::SeqCst)),
        interrupted: AtomicBool::new(false),
        changed: tokio::sync::Notify::new(),
        host: Mutex::new(None),
        requests: std::sync::Mutex::default(),
    };
    let output = Arc::new(Mutex::new(tokio::io::stdout()));
    let (tx, mut rx) = mpsc::unbounded_channel();
    for (host_reply, succeeds) in [
        (
            json!({"status":"ok","result":{"path":"/tmp/new.txt","bytes":6}}),
            true,
        ),
        (json!({"status":"error","error":"File exists"}), false),
    ] {
        let operation = execute(
            &mut evaluator,
            r#":write created { path = "new.txt"; text = "中文"; }"#,
            &control,
            &output,
            &mut rx,
        );
        let reply = async {
            loop {
                if let Some(id) = control.host.lock().await.clone() {
                    tx.send(json!({"id":id,"data":host_reply})).unwrap();
                    break;
                }
                tokio::task::yield_now().await;
            }
        };
        let (result, ()) = tokio::join!(operation, reply);
        if succeeds {
            assert_eq!(
                result.unwrap(),
                json!({"output":"Created /tmp/new.txt (6 bytes)"})
            );
        } else {
            assert!(result.is_err());
        }
        let bound = evaluator
            .evaluate(":p (builtins.toJSON created)")
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&bound).unwrap(),
            json!({"path":"/tmp/new.txt","bytes":6})
        );
    }
    evaluator.close().await.unwrap();
}
