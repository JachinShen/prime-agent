//! Exercise the shipped native adapter, not a Python substitute.
#![cfg(unix)]
use serde_json::{json, Value};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
};

struct Runtime {
    child: Child,
    input: Option<ChildStdin>,
    lines: Lines<BufReader<ChildStdout>>,
}
impl Runtime {
    async fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_prime-agent"))
            .arg("--prime-agent-nix-runtime")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut runtime = Self {
            child,
            input,
            lines,
        };
        assert_eq!(runtime.next().await["event"], "ready");
        runtime
    }
    async fn send(&mut self, request: Value) {
        self.input
            .as_mut()
            .unwrap()
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        self.input.as_mut().unwrap().flush().await.unwrap();
    }
    async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(15), self.lines.next_line())
            .await
            .expect("native runtime did not settle")
            .unwrap()
            .expect("native runtime exited");
        serde_json::from_str(&line).unwrap()
    }
    async fn done(&mut self, id: &str) -> Vec<Value> {
        let mut events = Vec::new();
        loop {
            let event = self.next().await;
            let done = event["event"] == "done" && event["id"] == id;
            events.push(event);
            if done {
                return events;
            }
        }
    }
    async fn shutdown(mut self) {
        self.send(json!({"type":"shutdown","id":"shutdown"})).await;
        self.done("shutdown").await;
        assert!(
            tokio::time::timeout(Duration::from_secs(15), self.child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
}

#[tokio::test]
async fn named_aliases_bind_structured_results_and_reject_names_before_dispatch() {
    let mut runtime = Runtime::start().await;
    runtime
        .send(json!({"type":"execute","id":"invalid","code":":run builtins \"false\""}))
        .await;
    let events = runtime.done("invalid").await;
    assert!(events.iter().all(|e| e["event"] != "host_request"));
    assert_eq!(events.last().unwrap()["status"], "error");
    runtime
        .send(json!({"type":"execute","id":"run","code":":run result \"printf hello\""}))
        .await;
    let host = runtime.next().await;
    assert_eq!(
        host["data"],
        json!({"type":"tool.run","command":"printf hello"})
    );
    runtime.send(json!({"type":"host_reply","id":host["id"],"data":{"status":"ok","result":{"stdout":"你好🌸","stderr":"","exitCode":0}}})).await;
    let events = runtime.done("run").await;
    assert_eq!(events[0]["text"], "你好🌸");
    runtime
        .send(json!({"type":"execute","id":"reuse","code":"result.stdout"}))
        .await;
    assert_eq!(runtime.done("reuse").await[0]["text"], "\"你好🌸\"");
    runtime.send(json!({"type":"execute","id":"edit","code":":edit change { path = \"a\"; old = \"x\"; new = \"y\"; }"})).await;
    let host = runtime.next().await;
    assert_eq!(
        host["data"],
        json!({"type":"tool.edit","path":"a","edits":[{"oldText":"x","newText":"y"}]})
    );
    runtime.send(json!({"type":"host_reply","id":host["id"],"data":{"status":"ok","result":{"details":{"diff":"-x\n+y"}}}})).await;
    assert_eq!(runtime.done("edit").await[0]["text"], "-x\n+y");
    runtime
        .send(json!({"type":"execute","id":"compose","code":":run second result.stdout"}))
        .await;
    let host = runtime.next().await;
    assert_eq!(host["data"], json!({"type":"tool.run","command":"你好🌸"}));
    runtime.send(json!({"type":"host_reply","id":host["id"],"data":{"status":"ok","result":{"stdout":"second","stderr":"","exitCode":0}}})).await;
    assert_eq!(
        runtime.done("compose").await.last().unwrap()["status"],
        "ok"
    );
    runtime
        .send(json!({"type":"execute","id":"retained","code":"result.stdout + second.stdout"}))
        .await;
    assert_eq!(
        runtime.done("retained").await[0]["text"],
        "\"你好🌸second\""
    );
    runtime.shutdown().await;
}

#[tokio::test]
async fn repeated_host_interrupts_recover_and_shutdown_cancels_host() {
    let mut runtime = Runtime::start().await;
    for round in 0..3 {
        let id = format!("host-{round}");
        runtime
            .send(
                json!({"type":"execute","id":id,"code":":host receipt { type = \"test.wait\"; }"}),
            )
            .await;
        let host = runtime.next().await;
        assert_eq!(host["event"], "host_request");
        runtime.send(json!({"type":"interrupt","id":id})).await;
        assert_eq!(
            runtime.next().await,
            json!({"event":"host_cancel","id":host["id"]})
        );
        assert_eq!(runtime.done(&id).await.last().unwrap()["status"], "error");
        runtime
            .send(json!({"type":"execute","id":"recovery","code":"6 * 7"}))
            .await;
        assert_eq!(runtime.done("recovery").await[0]["text"], "42");
    }
    runtime
        .send(json!({"type":"execute","id":"pending","code":":run pending \"sleep 99\""}))
        .await;
    let host = runtime.next().await;
    runtime
        .send(json!({"type":"shutdown","id":"shutdown"}))
        .await;
    assert_eq!(
        runtime.next().await,
        json!({"event":"host_cancel","id":host["id"]})
    );
    runtime.done("shutdown").await;
    assert!(
        tokio::time::timeout(Duration::from_secs(15), runtime.child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

#[tokio::test]
async fn eof_cancels_waiting_host_and_exits() {
    let mut runtime = Runtime::start().await;
    runtime
        .send(json!({"type":"execute","id":"waiting","code":":run waiting \"sleep 99\""}))
        .await;
    let host = runtime.next().await;
    runtime.input.take();
    assert_eq!(
        runtime.next().await,
        json!({"event":"host_cancel","id":host["id"]})
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(15), runtime.child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}
