use super::evaluator::{signal, valid_name, Evaluator};
use anyhow::{bail, Context};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Mutex};

pub(super) type Output = Arc<Mutex<tokio::io::Stdout>>;
async fn emit(output: &Output, event: Value) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec(&event)?;
    bytes.push(b'\n');
    let mut out = output.lock().await;
    out.write_all(&bytes).await?;
    out.flush().await?;
    Ok(())
}
pub(super) struct Control {
    pub(super) pid: AtomicU32,
    pub(super) interrupted: AtomicBool,
    pub(super) changed: tokio::sync::Notify,
    pub(super) host: Mutex<Option<String>>,
    pub(super) requests: std::sync::Mutex<RequestState>,
}
#[derive(Default)]
pub(super) struct RequestState {
    pending: std::collections::VecDeque<String>,
    cancelled: std::collections::HashSet<String>,
    active: Option<String>,
    closing: bool,
}

pub(super) async fn serve() -> anyhow::Result<()> {
    let pid = Arc::new(AtomicU32::new(0));
    let mut evaluator = Evaluator::start(&pid).await?;
    let control = Arc::new(Control {
        pid: AtomicU32::new(pid.load(Ordering::SeqCst)),
        interrupted: AtomicBool::new(false),
        changed: tokio::sync::Notify::new(),
        host: Mutex::new(None),
        requests: std::sync::Mutex::new(RequestState::default()),
    });
    let output = Arc::new(Mutex::new(tokio::io::stdout()));
    emit(
        &output,
        json!({"event":"ready","protocol":3,"runtime":"nix"}),
    )
    .await?;
    let (requests_tx, mut requests) = mpsc::unbounded_channel();
    let (replies_tx, mut replies) = mpsc::unbounded_channel();
    let reader_control = control.clone();
    let reader_output = output.clone();
    let reader = tokio::spawn(async move {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        while let Some(line) = lines.next_line().await? {
            let request: Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(error) => {
                    emit(&reader_output, json!({"event":"error","ename":"ProtocolError","evalue":error.to_string(),"traceback":[]})).await?;
                    break;
                }
            };
            let kind = request["type"].as_str().unwrap_or("");
            if kind == "host_reply" {
                let _ = replies_tx.send(request);
                continue;
            }
            if kind == "interrupt" || kind == "shutdown" {
                let cancel_active = {
                    let mut state = reader_control
                        .requests
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let target = (kind == "interrupt")
                        .then(|| request["id"].as_str().map(str::to_owned))
                        .flatten()
                        .or_else(|| state.active.clone())
                        .or_else(|| state.pending.front().cloned());
                    let active = target.is_some() && target == state.active;
                    if let Some(target) = target {
                        state.cancelled.insert(target);
                    }
                    if kind == "shutdown" {
                        state.closing = true;
                    }
                    if active {
                        reader_control.interrupted.store(true, Ordering::SeqCst);
                        reader_control.changed.notify_waiters();
                    }
                    active
                };
                if kind == "shutdown" {
                    signal(
                        reader_control.pid.load(Ordering::SeqCst),
                        nix::sys::signal::Signal::SIGKILL,
                    );
                } else if cancel_active {
                    signal(
                        reader_control.pid.load(Ordering::SeqCst),
                        nix::sys::signal::Signal::SIGINT,
                    );
                }
                if cancel_active {
                    if let Some(id) = reader_control.host.lock().await.take() {
                        emit(&reader_output, json!({"event":"host_cancel","id":id})).await?;
                        let _ = replies_tx
                            .send(json!({"id":id,"data":{"status":"error","error":"Interrupted"}}));
                    }
                }
                if kind == "interrupt" {
                    continue;
                }
            }
            let shutdown = kind == "shutdown";
            if !shutdown {
                let Some(id) = request["id"].as_str().filter(|id| !id.is_empty()) else {
                    emit(&reader_output, json!({"event":"error","ename":"ProtocolError","evalue":"Request needs non-empty string id","traceback":[]})).await?;
                    break;
                };
                reader_control
                    .requests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pending
                    .push_back(id.to_owned());
            }
            if requests_tx.send(request).is_err() || shutdown {
                break;
            }
        }
        reader_control
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closing = true;
        reader_control.interrupted.store(true, Ordering::SeqCst);
        reader_control.changed.notify_waiters();
        signal(
            reader_control.pid.load(Ordering::SeqCst),
            nix::sys::signal::Signal::SIGKILL,
        );
        if let Some(id) = reader_control.host.lock().await.take() {
            emit(&reader_output, json!({"event":"host_cancel","id":id})).await?;
            let _ = replies_tx
                .send(json!({"id":id,"data":{"status":"error","error":"Host connection closed"}}));
        }
        Ok::<_, anyhow::Error>(())
    });
    let result = async {
        while let Some(request) = requests.recv().await {
            let kind = request["type"].as_str().unwrap_or("");
            if kind == "shutdown" {
                if let Some(id) = request["id"].as_str() {
                    emit(&output, json!({"event":"done","id":id,"status":"ok"})).await?;
                }
                break;
            }
            let id = request["id"].as_str().context("Request needs string id")?;
            {
                let mut state = control.requests.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                state.pending.retain(|pending| pending != id);
                state.active = Some(id.to_owned());
                control.interrupted.store(state.closing || state.cancelled.remove(id), Ordering::SeqCst);
            }
            let outcome = if control.interrupted.load(Ordering::SeqCst) { Err(anyhow::anyhow!("Interrupted")) } else { match kind {
                "execute" => match request["code"].as_str() {
                    Some(code) => execute(&mut evaluator, code, &control, &output, &mut replies).await,
                    None => Err(anyhow::anyhow!("execute needs string code")),
                },
                "list_names" => Ok(json!({"names":evaluator.names})),
                "snapshot" | "restore" => evaluator.state(&request).await,
                _ => Err(anyhow::anyhow!("Unsupported request type {kind}")),
            }};
            let mut done = json!({"event":"done","id":id,"status":"ok"});
            match outcome {
                Ok(fields) if !control.interrupted.load(Ordering::SeqCst) => {
                    if let Some(text) = fields["output"].as_str().filter(|s| !s.is_empty()) {
                        emit(&output, json!({"event":"result","id":id,"text":text})).await?;
                    }
                    if let Some(fields) = fields.as_object() {
                        for (key, value) in fields { if key != "output" { done[key] = value.clone(); } }
                    }
                }
                outcome => {
                    let interrupted = control.interrupted.load(Ordering::SeqCst);
                    let message = outcome.err().map_or_else(|| "Interrupted".into(), |error| error.to_string());
                    done["status"] = json!("error");
                    done["reason"] = json!(message);
                    emit(&output, json!({"event":"error","id":id,"ename":if interrupted {"KeyboardInterrupt"} else {"NixError"},"evalue":message,"traceback":[]})).await?;
                }
            }
            control.requests.lock().unwrap_or_else(std::sync::PoisonError::into_inner).active = None;
            emit(&output, done).await?;
        }
        Ok::<_, anyhow::Error>(())
    }.await;
    reader.abort();
    evaluator.close().await?;
    result
}

pub(super) async fn execute(
    evaluator: &mut Evaluator,
    code: &str,
    control: &Control,
    output: &Output,
    replies: &mut mpsc::UnboundedReceiver<Value>,
) -> anyhow::Result<Value> {
    let code = code.trim();
    if code.is_empty() {
        return Ok(json!({}));
    }
    let (command, rest) = code.split_once(char::is_whitespace).unwrap_or((code, ""));
    if matches!(command, ":run" | ":edit" | ":host") {
        let rest = rest.trim();
        let legacy = command == ":host" && rest.starts_with('{');
        let (name, expression) = if legacy {
            ("lastHostReply", rest)
        } else {
            let (name, expression) = rest
                .split_once(char::is_whitespace)
                .context("Command requires NAME and a Nix expression")?;
            if !valid_name(name) {
                bail!("Invalid or reserved binding name: {name}");
            }
            (name, expression.trim())
        };
        let parsed = parse(evaluator, expression, control).await?;
        let encoded = evaluator
            .evaluate(&format!(":p (builtins.toJSON ({parsed}))"))
            .await?;
        let mut data: Value = serde_json::from_str(&encoded)?;
        if command == ":run" && data.is_string() {
            data = json!({"command":data});
        }
        if !data.is_object() {
            bail!("{command} requires an attribute set (or a string for :run)");
        }
        if command == ":edit" {
            let attrs = data.as_object_mut().context("edit attributes")?;
            if !attrs.contains_key("edits") {
                let old = attrs
                    .remove("old")
                    .or_else(|| attrs.remove("old_str"))
                    .context(":edit needs old or edits")?;
                let new = attrs
                    .remove("new")
                    .or_else(|| attrs.remove("new_str"))
                    .context(":edit needs new or edits")?;
                attrs.insert("edits".into(), json!([{"oldText":old,"newText":new}]));
            }
        }
        if command != ":host" {
            data["type"] = json!(if command == ":run" {
                "tool.run"
            } else {
                "tool.edit"
            });
        }
        let rid = uuid::Uuid::new_v4().to_string();
        {
            let mut host = control.host.lock().await;
            if control.interrupted.load(Ordering::SeqCst) {
                bail!("Interrupted before host dispatch");
            }
            *host = Some(rid.clone());
            emit(output, json!({"event":"host_request","id":rid,"data":data})).await?;
        }
        let reply = loop {
            let reply = replies.recv().await.context("Host connection closed")?;
            if reply["id"] == rid {
                break reply["data"].clone();
            }
        };
        *control.host.lock().await = None;
        if control.interrupted.load(Ordering::SeqCst) {
            bail!("Host request interrupted");
        }
        evaluator.bind("lastHostReply", &reply).await?;
        let value = if legacy { &reply } else { &reply["result"] };
        if legacy || reply["status"] == "ok" {
            evaluator.bind(name, value).await?;
        }
        if reply["status"] != "ok" {
            bail!("Host request failed: {reply}");
        }
        if command == ":run" {
            let stdout = value["stdout"].as_str().unwrap_or("");
            let stderr = value["stderr"].as_str().unwrap_or("");
            match value.get("exitCode") {
                Some(Value::Number(code)) if code.as_i64() == Some(0) => {}
                Some(Value::Number(code)) if code.as_i64().is_some() => {
                    bail!("{stdout}{stderr}\nCommand exited with {code}");
                }
                Some(Value::Null) => bail!("{stdout}{stderr}\nCommand terminated by signal"),
                _ => bail!("Invalid command exit status"),
            }
            return Ok(json!({"output":format!("{stdout}{stderr}")}));
        }
        Ok(
            json!({"output":if command == ":edit" { value.get("details").and_then(|details| details.get("diff")).or_else(|| value.get("output")).and_then(Value::as_str).unwrap_or("Edit completed").to_string() } else { serde_json::to_string(value)? }}),
        )
    } else if command.starts_with(':') {
        if ![":b", ":p", ":t", ":doc", ":?", ":help"].contains(&command) || code.contains('\n') {
            bail!("Unsupported REPL command: {command}");
        }
        let source = if rest.is_empty() {
            command.to_string()
        } else {
            format!("{command} {}", parse(evaluator, rest, control).await?)
        };
        Ok(json!({"output":evaluator.evaluate(&source).await?}))
    } else {
        let assignment = code
            .split_once('=')
            .filter(|(name, expr)| valid_name(name.trim()) && !expr.starts_with('='));
        let expression = assignment.map_or(code, |(_, expr)| expr.trim());
        let parsed = parse(evaluator, expression, control).await?;
        if control.interrupted.load(Ordering::SeqCst) {
            bail!("Interrupted before evaluation");
        }
        let source = assignment.map_or_else(
            || parsed.clone(),
            |(name, _)| format!("{} = {parsed}", name.trim()),
        );
        let result = evaluator.evaluate(&source).await?;
        if let Some((name, _)) = assignment {
            let name = name.trim().to_string();
            evaluator.names.insert(name.clone());
            evaluator.literals.remove(&name);
            if !expression.contains("${")
                && !["\\u", "\\b", "\\f", "\\/"]
                    .iter()
                    .any(|escape| expression.contains(escape))
            {
                if let Ok(value) = serde_json::from_str::<Value>(expression) {
                    if !value.is_array() && !value.is_object() {
                        evaluator.literals.insert(name, value);
                    }
                }
            }
        }
        Ok(json!({"output":result}))
    }
}

async fn parse(
    evaluator: &Evaluator,
    expression: &str,
    control: &Control,
) -> anyhow::Result<String> {
    let changed = control.changed.notified();
    tokio::pin!(changed);
    changed.as_mut().enable();
    if control.interrupted.load(Ordering::SeqCst) {
        bail!("Interrupted before parsing");
    }
    tokio::select! {
        result = evaluator.parse(expression) => result,
        () = &mut changed => Err(anyhow::anyhow!("Parsing interrupted")),
    }
}
