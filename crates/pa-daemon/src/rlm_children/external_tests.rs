//! Native external-child lifecycle against fake Herdr and supervisor sockets.

use super::*;
use crate::protocol::{response_failure, response_success};
use pa_types::platform::transport::bind_transport;
use std::collections::BTreeMap;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{broadcast, mpsc, Notify};
use tokio::task::{JoinHandle, JoinSet};

const TEST_BUDGET: Duration = Duration::from_secs(10);

#[derive(Default)]
struct Faults {
    tab_error: bool,
    malformed_tab: bool,
    worktree_error: bool,
    malformed_worktree: bool,
    start_error: Option<&'static str>,
    start_error_code: Option<&'static str>,
    busy_starts_remaining: usize,
    start_disconnect: bool,
    pending_start: bool,
    startup_get_gate: Option<Arc<Notify>>,
    foreground_processes: Option<Vec<Value>>,
    notice_error: Option<&'static str>,
    notice_gate: Option<Arc<Notify>>,
}

struct Fixture {
    _directory: tempfile::TempDir,
    sessions: SupervisorChildSessions,
    agent: Arc<Mutex<Value>>,
    history: Arc<std::sync::Mutex<Vec<Value>>>,
    requests: mpsc::UnboundedReceiver<Value>,
    updates: mpsc::UnboundedReceiver<Value>,
    notices: mpsc::UnboundedReceiver<Value>,
    events: broadcast::Sender<Value>,
    faults: Arc<std::sync::Mutex<Faults>>,
    servers: Vec<JoinHandle<()>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for server in &self.servers {
            server.abort();
        }
    }
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("pa-external-")
            .tempdir_in("/tmp")
            .unwrap();
        let herdr_path = directory.path().join("h.sock");
        let supervisor_path = directory.path().join("s.sock");
        let herdr_listener = UnixListener::bind(&herdr_path).unwrap();
        let supervisor_listener = bind_transport(&supervisor_path).await.unwrap();
        let agent = Arc::new(Mutex::new(json!({
            "name":"not-started", "agent":"codex", "workspace_id":"w1",
            "pane_id":"w1:p2", "terminal_id":"terminal-child", "tab_id":"w1:t2",
            "focused":false, "revision":3,
            "agent_status":"idle", "state_change_seq":10, "completion_seq":10,
            "interactive_ready":true,
        })));
        let history = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (request_tx, requests) = mpsc::unbounded_channel();
        let (update_tx, updates) = mpsc::unbounded_channel();
        let (notice_tx, notices) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel::<Value>(32);
        let faults = Arc::new(std::sync::Mutex::new(Faults::default()));
        let server_faults = Arc::clone(&faults);
        let server_agent = Arc::clone(&agent);
        let server_history = Arc::clone(&history);
        let server_events = events.clone();
        let herdr_server = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = herdr_listener.accept() => {
                        let (stream, _) = accepted.unwrap();
                        let agent = Arc::clone(&server_agent);
                        let history = Arc::clone(&server_history);
                        let requests = request_tx.clone();
                        let events = server_events.clone();
                        let faults = Arc::clone(&server_faults);
                        connections.spawn(async move {
                            let (reader, mut writer) = stream.into_split();
                            let mut reader = BufReader::new(reader);
                            let mut line = String::new();
                            reader.read_line(&mut line).await.unwrap();
                            let request: Value = serde_json::from_str(&line).unwrap();
                            history.lock().unwrap().push(request.clone());
                            requests.send(request.clone()).unwrap();
                            let method = request["method"].as_str().unwrap();
                            if method == "agent.start" {
                                let (error, disconnect) = {
                                    let mut faults = faults.lock().unwrap();
                                    let error = if faults.busy_starts_remaining > 0 {
                                        faults.busy_starts_remaining -= 1;
                                        Some(("agent_pane_busy", "shell integration has not become ready"))
                                    } else {
                                        faults.start_error.map(|message| (faults.start_error_code.unwrap_or("agent_start_failed"),message))
                                    };
                                    (error, faults.start_disconnect)
                                };
                                if disconnect { return; }
                                if let Some((code,message)) = error {
                                    if code == "agent_not_ready" {
                                        agent.lock().await["name"] = request["params"]["name"].clone();
                                    }
                                    let response = json!({"id":request["id"],"error":{"code":code,"message":message}});
                                    writer.write_all(format!("{response}\n").as_bytes()).await.unwrap();
                                    return;
                                }
                            }
                            if method == "tab.create" && faults.lock().unwrap().tab_error {
                                let response=json!({"id":request["id"],"error":{"code":"tab_create_failed","message":"tab allocation refused"}});
                                writer.write_all(format!("{response}\n").as_bytes()).await.unwrap();
                                return;
                            }
                            if method == "worktree.create" && faults.lock().unwrap().worktree_error {
                                let response=json!({"id":request["id"],"error":{"code":"worktree_create_failed","message":"branch already exists"}});
                                writer.write_all(format!("{response}\n").as_bytes()).await.unwrap();
                                return;
                            }
                            if method == "events.subscribe" {
                                let subscription=&request["params"]["subscriptions"][0];
                                assert_eq!(subscription["type"], "pane.agent_status_changed");
                                // Subscription acknowledgement must not take the agent-response
                                // lock: the stale-observer test deliberately holds that response.
                                assert!(matches!(subscription["pane_id"].as_str(),Some("w1:p2"|"w2:p1")));
                                let mut events = events.subscribe();
                                let response = json!({"id":request["id"],"result":{"type":"subscription_started"}});
                                writer.write_all(format!("{response}\n").as_bytes()).await.unwrap();
                                while let Ok(event) = events.recv().await {
                                    if writer.write_all(format!("{event}\n").as_bytes()).await.is_err() {
                                        return;
                                    }
                                }
                                return;
                            }
                            if method == "agent.get" {
                                let gate = faults.lock().unwrap().startup_get_gate.clone();
                                if let Some(gate) = gate { gate.notified().await; }
                            }
                            let result = match method {
                                "pane.get" => json!({"pane":{"pane_id":"w1:p1","workspace_id":"w1","terminal_id":"terminal-parent"}}),
                                "pane.process_info" => {
                                    let foreground = faults.lock().unwrap().foreground_processes.clone()
                                        .unwrap_or_else(|| vec![json!({"pid":42})]);
                                    json!({"process_info":{"shell_pid":42,"foreground_processes":foreground}})
                                }
                                "tab.create" => {
                                    if faults.lock().unwrap().malformed_tab {
                                        json!({"tab":{"tab_id":"w1:t2","workspace_id":"w1"}})
                                    } else {
                                        json!({"tab":{"tab_id":"w1:t2","workspace_id":"w1"},
                                            "root_pane":{"pane_id":"w1:p2","tab_id":"w1:t2","workspace_id":"w1","terminal_id":"terminal-child"}})
                                    }
                                }
                                "worktree.create" => {
                                    assert_eq!(request["params"]["focus"],false);
                                    let checkout=Path::new(request["params"]["cwd"].as_str().unwrap()).join("isolated-checkout");
                                    tokio::fs::create_dir_all(&checkout).await.unwrap();
                                    tokio::fs::write(checkout.join("keep.txt"),"uncommitted child changes").await.unwrap();
                                    let mut agent=agent.lock().await;
                                    agent["workspace_id"]=json!("w2");
                                    agent["pane_id"]=json!("w2:p1");
                                    agent["tab_id"]=json!("w2:t1");
                                    let path=checkout.display().to_string();
                                    let membership=json!({"repo_root":request["params"]["cwd"],"checkout_path":path,"is_linked_worktree":true});
                                    json!({"workspace":{"workspace_id":"w2","worktree":membership},
                                        "root_pane":{"pane_id":"w2:p1","workspace_id":"w2","terminal_id":"terminal-child"},
                                        "worktree":{"path":path,"branch":request["params"]["branch"],
                                            "is_linked_worktree":!faults.lock().unwrap().malformed_worktree}})
                                }
                                "agent.start" => {
                                    let mut agent = agent.lock().await;
                                    agent["name"] = request["params"]["name"].clone();
                                    if faults.lock().unwrap().pending_start {
                                        // Captured raw socket shape: optional None/false fields are omitted.
                                        agent.as_object_mut().unwrap().retain(|key,_| !matches!(key.as_str(),"agent"|"interactive_ready"|"completion_seq"));
                                        agent["launch_pending"] = json!(true);
                                        agent["agent_status"] = json!("unknown");
                                        agent["state_change_seq"] = json!(0);
                                    }
                                    json!({"agent":agent.clone()})
                                }
                                "agent.get" | "agent.prompt" => json!({"agent":agent.lock().await.clone()}),
                                "agent.read" => {
                                    let source = if matches!(agent.lock().await["agent_status"].as_str(),Some("idle"|"done")) {"recent_unwrapped"} else {"visible"};
                                    assert_eq!(request["params"]["source"], source);
                                    assert_eq!(request["params"]["strip_ansi"], true);
                                    assert_eq!(request["params"]["format"], "text");
                                    json!({"read":{"text":"terminal evidence","source":source,"revision":7,"truncated":false}})
                                }
                                "agent.send_keys" | "pane.focus" | "pane.close" => json!({"ok":true}),
                                other => panic!("unexpected Herdr method: {other}"),
                            };
                            let response = json!({"id":request["id"],"result":result});
                            writer.write_all(format!("{response}\n").as_bytes()).await.unwrap();
                        });
                    }
                    Some(result) = connections.join_next(), if !connections.is_empty() => {
                        result.unwrap();
                    }
                }
            }
        });
        let server_faults = Arc::clone(&faults);
        let supervisor_server = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = supervisor_listener.accept() => {
                        let stream = accepted.unwrap();
                        let notices = notice_tx.clone();
                        let faults = Arc::clone(&server_faults);
                        connections.spawn(async move {
                            let (reader, mut writer) = stream.split();
                            let mut reader = BufReader::new(reader);
                            writer.write_all(b"{\"type\":\"daemon_hello\",\"protocol\":{\"name\":\"prime-agent.daemon\",\"version\":7}}\n").await.unwrap();
                            loop {
                                let mut line = String::new();
                                if reader.read_line(&mut line).await.unwrap() == 0 { return; }
                                let request: Value = serde_json::from_str(&line).unwrap();
                                let id = request["id"].as_str().unwrap();
                                let command = &request["command"];
                                let kind = command["type"].as_str().unwrap();
                                let response = match kind {
                                    "follow_up" => {
                                        notices.send(command.clone()).unwrap();
                                        let (gate, error) = {
                                            let faults = faults.lock().unwrap();
                                            (faults.notice_gate.clone(),faults.notice_error)
                                        };
                                        if let Some(gate) = gate { gate.notified().await; }
                                        match error {
                                            Some(error) => response_failure(Some(id),kind,error,None),
                                            None => response_success(Some(id),kind,Some(json!({"queued":true}))),
                                        }
                                    }
                                    "get_state" => response_success(Some(id),kind,Some(json!({"isStreaming":false,"sessionActions":{"queuedCount":0}}))),
                                    "kill" => response_success(Some(id),kind,None),
                                    other => response_failure(Some(id),other,"unexpected supervisor command",None),
                                };
                                let response = serde_json::to_string(&response).unwrap();
                                writer.write_all(format!("{response}\n").as_bytes()).await.unwrap();
                            }
                        });
                    }
                    Some(result) = connections.join_next(), if !connections.is_empty() => {
                        result.unwrap();
                    }
                }
            }
        });
        let sessions = SupervisorChildSessions::new(
            Arc::new(SupervisorLink::new(supervisor_path)),
            directory.path().to_path_buf(),
            "parent-live".into(),
            Arc::new(crate::model_allowlist::ModelRefusalTelemetry::new(
                directory.path().to_path_buf(),
                /*telemetry_disabled*/ true,
            )),
            String::new(),
        );
        sessions.set_identity(ParentIdentity {
            cwd: Some(directory.path().display().to_string()),
            session_id: Some("parent-file".into()),
            ..ParentIdentity::with_default_depth()
        });
        sessions.set_herdr_env(BTreeMap::from([
            ("HERDR_ENV".into(), "1".into()),
            ("HERDR_SOCKET_PATH".into(), herdr_path.display().to_string()),
            ("HERDR_PANE_ID".into(), "w1:p1".into()),
            ("HERDR_WORKSPACE_ID".into(), "w1".into()),
        ]));
        sessions.set_child_update_sink(Arc::new(move |update| {
            update_tx.send(update).unwrap();
        }));
        Self {
            _directory: directory,
            sessions,
            agent,
            history,
            requests,
            updates,
            notices,
            events,
            faults,
            servers: vec![herdr_server, supervisor_server],
        }
    }

    async fn request(&mut self, method: &str) -> Value {
        tokio::time::timeout(TEST_BUDGET, async {
            loop {
                let request = self.requests.recv().await.expect("Herdr server stopped");
                if request["method"] == method {
                    return request;
                }
            }
        })
        .await
        .expect("expected Herdr request")
    }

    async fn update(&mut self, turn: u64, state: &str) -> Value {
        self.update_matching(|update| {
            update["external"]["turn"] == turn && update["external"]["state"] == state
        })
        .await
    }

    async fn update_matching(&mut self, matches: impl Fn(&Value) -> bool) -> Value {
        tokio::time::timeout(TEST_BUDGET, async {
            loop {
                let update = self
                    .updates
                    .recv()
                    .await
                    .expect("child update stream stopped");
                if matches(&update) {
                    return update;
                }
            }
        })
        .await
        .expect("expected child state update")
    }

    async fn notice(&mut self, event: &str) -> Value {
        tokio::time::timeout(TEST_BUDGET, async {
            loop {
                let notice = self.notices.recv().await.expect("supervisor stopped");
                if notice["message"].as_str().unwrap().contains(event) {
                    return notice;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("expected parent follow-up containing {event}"))
    }

    async fn change_status(&self, status: &str, state_seq: u64, completion_seq: u64) {
        {
            let mut agent = self.agent.lock().await;
            agent["agent_status"] = json!(status);
            agent["state_change_seq"] = json!(state_seq);
            agent["completion_seq"] = json!(completion_seq);
        }
        let agent = self.agent.lock().await.clone();
        self.events
            .send(json!({"event":"pane.agent_status_changed","data":{
                "pane_id":agent["pane_id"],"workspace_id":agent["workspace_id"],"agent_status":status
            }}))
            .expect("watcher must subscribe before admitting the task");
    }

    fn prompt_count(&self) -> usize {
        self.history
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["method"] == "agent.prompt")
            .count()
    }
}

fn request() -> RlmSpawnRequest {
    RlmSpawnRequest {
        prompt: "Review the API error handling".into(),
        name: Some("review-api".into()),
        model: None,
        thinking: None,
        agent: Some("codex".into()),
        agent_args: vec!["--model".into(), "a model; $(literal argument)".into()],
        worktree: None,
        spawned_by_request_id: None,
    }
}

async fn snapshot(sessions: &SupervisorChildSessions, child_id: &str) -> Value {
    let result = sessions.collect(vec![child_id.into()], 0).await.unwrap();
    assert_eq!(result.len(), 1);
    serde_json::to_value(&result[0]).unwrap()
}

async fn settled(sessions: &SupervisorChildSessions, child_id: &str) -> Value {
    let result = sessions
        .collect(vec![child_id.into()], TEST_BUDGET.as_millis() as u64)
        .await
        .unwrap();
    assert_eq!(result.len(), 1);
    assert!(
        result[0].settled,
        "the child should settle after its terminal notice"
    );
    serde_json::to_value(&result[0]).unwrap()
}

#[tokio::test]
async fn external_worktree_admission_preserves_branch_results_and_checkout_after_close() {
    use pa_types::daemon::external_subagent::ExternalSubagentWorktreeRequest;
    for branch in [None, Some("task/review".to_string())] {
        let mut fixture = Fixture::new().await;
        fixture.sessions.notify_turn_started();
        let mut request = request();
        request.worktree = Some(ExternalSubagentWorktreeRequest {
            branch: branch.clone(),
            base: Some("HEAD~1".into()),
        });
        let handle = fixture.sessions.spawn(request).await.unwrap();
        let create = fixture.request("worktree.create").await;
        let expected_branch = branch.unwrap_or_else(|| format!("task/{}", handle.rlm_child_id));
        assert_eq!(create["params"]["branch"], expected_branch);
        assert_eq!(create["params"]["base"], "HEAD~1");
        let info = handle.external.unwrap();
        assert_eq!(info.workspace_id, "w2");
        assert_eq!(
            fixture.request("agent.start").await["params"]["pane_id"],
            "w2:p1"
        );
        assert!(!fixture
            .history
            .lock()
            .unwrap()
            .iter()
            .any(|r| r["method"] == "tab.create"));
        let worktree = info.worktree.unwrap();
        assert_eq!(worktree.branch, expected_branch);
        let rows = fixture.sessions.external_roster_summaries();
        assert_eq!(rows[0]["cwd"], worktree.path);
        assert_eq!(
            external_store::load(&Path::new(&handle.session_dir).join("external.json"))
                .await
                .unwrap()
                .info
                .worktree,
            Some(worktree.clone())
        );
        fixture.sessions.notify_turn_done();
        fixture.request("agent.prompt").await;
        tokio::fs::write(info.answer_path.unwrap(), "isolated answer")
            .await
            .unwrap();
        fixture.change_status("done", 12, 11).await;
        fixture.notice(": completed").await;
        let collected = settled(&fixture.sessions, &handle.rlm_child_id).await;
        assert_eq!(
            collected["external"]["worktree"],
            serde_json::to_value(&worktree).unwrap()
        );
        fixture
            .sessions
            .control_external(&handle.rlm_child_id, "close", Vec::new(), 80)
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(Path::new(&worktree.path).join("keep.txt"))
                .await
                .unwrap(),
            "uncommitted child changes"
        );
        assert!(!fixture
            .history
            .lock()
            .unwrap()
            .iter()
            .any(|r| r["method"] == "worktree.remove"));
    }
}

#[tokio::test]
async fn external_worktree_failure_never_falls_back_to_parent_checkout() {
    use pa_types::daemon::external_subagent::ExternalSubagentWorktreeRequest;
    for malformed in [false, true] {
        let mut fixture = Fixture::new().await;
        fixture.faults.lock().unwrap().worktree_error = !malformed;
        fixture.faults.lock().unwrap().malformed_worktree = malformed;
        let mut request = request();
        request.worktree = Some(ExternalSubagentWorktreeRequest {
            branch: None,
            base: None,
        });
        let error = fixture.sessions.spawn(request).await.unwrap_err();
        assert!(format!("{error:#}").contains(if malformed {
            "resources retained"
        } else {
            "branch already exists"
        }));
        fixture.request("worktree.create").await;
        let history = fixture.history.lock().unwrap();
        assert_eq!(
            history
                .iter()
                .filter(|r| r["method"] == "worktree.create")
                .count(),
            1
        );
        assert!(!history.iter().any(|r| matches!(
            r["method"].as_str(),
            Some("tab.create" | "agent.start" | "agent.prompt" | "worktree.remove")
        )));
    }
}

#[tokio::test]
async fn external_spawn_creates_a_named_background_tab_in_the_parent_workspace() {
    let fixture = Fixture::new().await;
    let handle = fixture.sessions.spawn(request()).await.unwrap();
    let created: Vec<Value> = fixture
        .history
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request["method"] == "tab.create")
        .map(|request| request["params"].clone())
        .collect();
    assert_eq!(
        created,
        vec![json!({"workspace_id":"w1","cwd":fixture._directory.path(),
            "label":"review-api","focus":false})]
    );
    let info = handle.external.unwrap();
    assert_eq!(
        json!({"workspace":info.workspace_id,"pane":info.pane_id,"terminal":info.terminal_id}),
        json!({"workspace":"w1","pane":"w1:p2","terminal":"terminal-child"})
    );
}

#[tokio::test]
async fn external_tab_allocation_errors_are_explicit_and_never_retried() {
    for malformed in [false, true] {
        let fixture = Fixture::new().await;
        fixture.faults.lock().unwrap().tab_error = !malformed;
        fixture.faults.lock().unwrap().malformed_tab = malformed;
        let error = fixture.sessions.spawn(request()).await.unwrap_err();
        assert!(format!("{error:#}").contains(if malformed {
            "Herdr tab.create returned no root pane"
        } else {
            "tab allocation refused"
        }));
        let methods: Vec<String> = fixture
            .history
            .lock()
            .unwrap()
            .iter()
            .map(|request| request["method"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(methods, vec!["pane.get", "tab.create"]);
    }
}

#[tokio::test]
async fn ready_spawn_keeps_idle_and_blocked_turns_running_then_wakes_parent_for_each_answer() {
    let mut fixture = Fixture::new().await;
    fixture.sessions.notify_turn_started();
    let handle = tokio::time::timeout(TEST_BUDGET, fixture.sessions.spawn(request()))
        .await
        .unwrap()
        .unwrap();
    let first_answer = handle
        .external
        .as_ref()
        .unwrap()
        .answer_path
        .clone()
        .unwrap();
    assert_eq!(handle.name, "review-api");
    assert_eq!(handle.model, None);
    let started = fixture.request("agent.start").await;
    assert_eq!(
        started["params"]["args"],
        json!(["--model", "a model; $(literal argument)"])
    );
    assert_eq!(
        fixture.prompt_count(),
        0,
        "spawn returns after readiness, before parent turn ends"
    );
    fixture.sessions.notify_turn_done();
    let prompt = fixture.request("agent.prompt").await;
    assert!(prompt["params"]["text"]
        .as_str()
        .unwrap()
        .contains(&first_answer));
    fixture.update(1, "idle").await;
    let initial = snapshot(&fixture.sessions, &handle.rlm_child_id).await;
    assert_eq!(
        json!({"status":initial["status"],"settled":initial["settled"]}),
        json!({"status":"running","settled":false})
    );

    fixture.change_status("blocked", 11, 10).await;
    fixture.update(1, "blocked").await;
    fixture.notice("interactive input required").await;
    assert_eq!(
        fixture.request("agent.read").await["params"]["source"],
        "visible"
    );
    let blocked = snapshot(&fixture.sessions, &handle.rlm_child_id).await;
    assert_eq!(
        json!({"status":blocked["status"],"settled":blocked["settled"]}),
        json!({"status":"running","settled":false})
    );
    assert_eq!(blocked["external"]["output"]["source"], "visible");

    tokio::fs::write(&first_answer, "# Review\nThe error handling is correct.\n")
        .await
        .unwrap();
    fixture.change_status("done", 12, 11).await;
    fixture.notice(": completed").await;
    let completed = settled(&fixture.sessions, &handle.rlm_child_id).await;
    assert_eq!(
        json!({"status":completed["status"],"settled":completed["settled"],"text":completed["answer_text"]}),
        json!({"status":"done","settled":true,"text":"# Review\nThe error handling is correct.\n"})
    );
    let rows = fixture.sessions.list_subagents().await.unwrap();
    assert_eq!(rows[0].status, "completed");

    fixture
        .sessions
        .send_external(&handle.rlm_child_id, "Review the retry path too")
        .await
        .unwrap()
        .unwrap();
    let second_prompt = fixture.request("agent.prompt").await;
    fixture.update(2, "done").await;
    let second = snapshot(&fixture.sessions, &handle.rlm_child_id).await;
    let second_answer = second["external"]["answer_path"].as_str().unwrap();
    assert_ne!(first_answer, second_answer);
    assert!(second_prompt["params"]["text"]
        .as_str()
        .unwrap()
        .contains(second_answer));
    assert_eq!(
        second["settled"], false,
        "the previous turn's completion cannot finish a fresh task"
    );
    fixture.change_status("working", 13, 11).await;
    fixture.update(2, "working").await;
    assert_eq!(
        fixture.prompt_count(),
        2,
        "status observations never resubmit prompts"
    );
    tokio::fs::write(second_answer, "The retry path is correct too.")
        .await
        .unwrap();
    fixture.change_status("done", 14, 12).await;
    fixture.notice(": completed").await;
    let completed = settled(&fixture.sessions, &handle.rlm_child_id).await;
    assert_eq!(completed["answer_text"], "The retry path is correct too.");
    assert_eq!(completed["external"]["turn"], 2);
    assert_eq!(
        tokio::fs::read_to_string(first_answer).await.unwrap(),
        "# Review\nThe error handling is correct.\n"
    );
}

#[tokio::test]
async fn completion_without_the_current_answer_file_is_an_explicit_error_and_wakes_parent() {
    let mut fixture = Fixture::new().await;
    let handle = fixture.sessions.spawn(request()).await.unwrap();
    fixture.request("agent.prompt").await;
    fixture.update(1, "idle").await;
    fixture.change_status("done", 11, 11).await;
    fixture.notice(": error").await;
    let failed = settled(&fixture.sessions, &handle.rlm_child_id).await;
    assert_eq!(
        json!({"status":failed["status"],"settled":failed["settled"],"answer_text":failed["answer_text"]}),
        json!({"status":"error","settled":true,"answer_text":null})
    );
    assert!(failed["error"].as_str().unwrap().contains("answer.md"));
    assert_eq!(
        fixture.sessions.list_subagents().await.unwrap()[0].status,
        "error"
    );
    assert_eq!(fixture.prompt_count(), 1);
}

#[tokio::test]
async fn controls_refuse_a_replacement_terminal_before_sending_terminal_input() {
    let fixture = Fixture::new().await;
    fixture.sessions.notify_turn_started();
    let handle = fixture.sessions.spawn(request()).await.unwrap();
    fixture.agent.lock().await["terminal_id"] = json!("replacement-terminal");
    for action in ["read", "keys", "focus", "stop", "close"] {
        let error = fixture
            .sessions
            .control_external(&handle.rlm_child_id, action, vec!["enter".into()], 80)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("identity changed"), "{error:#}");
    }
    let methods: Vec<String> = fixture
        .history
        .lock()
        .unwrap()
        .iter()
        .filter_map(|request| {
            let method = request["method"].as_str().unwrap();
            (method.starts_with("agent.") && method != "agent.start").then(|| method.to_string())
        })
        .collect();
    assert_eq!(methods, vec!["agent.get"; 5]);
}

#[tokio::test]
async fn explicit_reads_of_a_working_agent_capture_visible_output_without_settling_the_turn() {
    let mut fixture = Fixture::new().await;
    fixture.sessions.notify_turn_started();
    let handle = fixture.sessions.spawn(request()).await.unwrap();
    fixture.agent.lock().await["agent_status"] = json!("working");
    let output = fixture
        .sessions
        .control_external(&handle.rlm_child_id, "read", Vec::new(), 80)
        .await
        .unwrap();
    assert_eq!(
        output,
        json!({"output":{"text":"terminal evidence","source":"visible","revision":7,"truncated":false}})
    );
    let read = fixture.request("agent.read").await;
    assert_eq!(read["params"]["source"], "visible");
    let current = snapshot(&fixture.sessions, &handle.rlm_child_id).await;
    assert_eq!(
        json!({"status":current["status"],"settled":current["settled"]}),
        json!({"status":"running","settled":false})
    );
    assert_eq!(current["external"]["output"]["source"], "visible");
}

#[tokio::test]
async fn owned_controls_preserve_terminal_evidence_and_cancel_only_after_confirmed_pane_close() {
    let mut fixture = Fixture::new().await;
    fixture.sessions.notify_turn_started();
    let handle = fixture.sessions.spawn(request()).await.unwrap();
    let output = fixture
        .sessions
        .control_external(&handle.rlm_child_id, "read", Vec::new(), 80)
        .await
        .unwrap();
    assert_eq!(
        output,
        json!({"output":{"text":"terminal evidence","source":"recent_unwrapped","revision":7,"truncated":false}})
    );
    let evidence = external_store::load(&Path::new(&handle.session_dir).join("external.json"))
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(evidence.info.output.unwrap()).unwrap(),
        output["output"]
    );
    fixture
        .sessions
        .control_external(
            &handle.rlm_child_id,
            "keys",
            vec!["y".into(), "enter".into()],
            80,
        )
        .await
        .unwrap();
    let keys = fixture.request("agent.send_keys").await;
    assert_eq!(keys["params"]["keys"], json!(["y", "enter"]));
    fixture
        .sessions
        .control_external(&handle.rlm_child_id, "focus", Vec::new(), 80)
        .await
        .unwrap();
    let focus = fixture.request("pane.focus").await;
    assert_eq!(focus["params"], json!({"pane_id":"w1:p2"}));
    fixture
        .sessions
        .control_external(&handle.rlm_child_id, "stop", Vec::new(), 80)
        .await
        .unwrap();
    let close = fixture.request("pane.close").await;
    assert_eq!(close["params"], json!({"pane_id":"w1:p2"}));
    let cancelled = snapshot(&fixture.sessions, &handle.rlm_child_id).await;
    assert_eq!(
        json!({"status":cancelled["status"],"settled":cancelled["settled"]}),
        json!({"status":"cancelled","settled":true})
    );
    assert_eq!(fixture.prompt_count(), 0);
}

#[tokio::test]
async fn restoring_a_delivered_turn_observes_completion_without_resending_the_prompt() {
    let mut fixture = Fixture::new().await;
    fixture.agent.lock().await["name"] = json!("pa-restored");
    let identity = fixture.sessions.inner.identity.lock().unwrap().clone();
    let directory = fixture
        .sessions
        .inner
        .child_session_dir("sub-restored", &identity)
        .unwrap();
    let answer_path = directory.join("turns/001/answer.md");
    let record = external_store::ExternalRecord {
        child_id: "sub-restored".into(),
        name: "restored-review".into(),
        dir: directory,
        socket_path: fixture.sessions.inner.external_env.lock().unwrap()["HERDR_SOCKET_PATH"]
            .clone(),
        info: pa_types::daemon::external_subagent::ExternalSubagentInfo {
            agent: "codex".into(),
            workspace_id: "w1".into(),
            pane_id: "w1:p2".into(),
            terminal_id: "terminal-child".into(),
            agent_name: "pa-restored".into(),
            turn: 1,
            state: "delivery-uncertain".into(),
            answer_path: Some(answer_path.display().to_string()),
            answer_truncated: false,
            output: None,
            notification_error: None,
            worktree: None,
        },
        expected_agent: Some("codex".into()),
        status: "running".into(),
        settled: false,
        text: String::new(),
        error: None,
        baseline_state_seq: 10,
        baseline_completion_seq: Some(10),
        started_at_ms: now_ms(),
        notice_sent: false,
        closed: false,
    };
    record.persist().await.unwrap();
    fixture.sessions.inner.restore_external().await.unwrap();
    fixture.update(1, "idle").await;
    assert_eq!(fixture.prompt_count(), 0);
    assert_eq!(
        snapshot(&fixture.sessions, "sub-restored").await["settled"],
        false
    );
    tokio::fs::create_dir_all(answer_path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&answer_path, "The restored task finished.")
        .await
        .unwrap();
    fixture.change_status("done", 11, 11).await;
    fixture.notice(": completed").await;
    let completed = settled(&fixture.sessions, "sub-restored").await;
    assert_eq!(completed["status"], "done");
    assert_eq!(completed["answer_text"], "The restored task finished.");
    assert_eq!(fixture.prompt_count(), 0);
    assert!(fixture
        .history
        .lock()
        .unwrap()
        .iter()
        .all(|request| request["method"] != "agent.start"));
}

#[tokio::test]
async fn a_completed_answer_is_settled_and_durable_before_the_parent_notice_is_acknowledged() {
    let mut fixture = Fixture::new().await;
    let gate = Arc::new(Notify::new());
    fixture.faults.lock().unwrap().notice_gate = Some(Arc::clone(&gate));
    let handle = fixture.sessions.spawn(request()).await.unwrap();
    fixture.request("agent.prompt").await;
    fixture.update(1, "idle").await;
    let answer = handle.external.unwrap().answer_path.unwrap();
    tokio::fs::write(answer, "A durable result.").await.unwrap();
    fixture.change_status("done", 11, 11).await;
    fixture.notice(": completed").await;
    let collected = snapshot(&fixture.sessions, &handle.rlm_child_id).await;
    assert_eq!(
        json!({"status":collected["status"],"settled":collected["settled"],"text":collected["answer_text"]}),
        json!({"status":"done","settled":true,"text":"A durable result."})
    );
    let path = Path::new(&handle.session_dir).join("external.json");
    let durable = external_store::load(&path).await.unwrap();
    assert_eq!(
        (
            durable.status.as_str(),
            durable.settled,
            durable.text.as_str(),
            durable.notice_sent
        ),
        ("completed", true, "A durable result.", false)
    );
    gate.notify_one();
    fixture
        .update_matching(|update| update["status"] == "completed")
        .await;
    assert!(external_store::load(&path).await.unwrap().notice_sent);
}

#[tokio::test]
async fn a_parent_notification_refusal_is_visible_and_persisted_beside_the_answer() {
    let mut fixture = Fixture::new().await;
    fixture.faults.lock().unwrap().notice_error = Some("parent refused the follow-up");
    let handle = fixture.sessions.spawn(request()).await.unwrap();
    fixture.request("agent.prompt").await;
    fixture.update(1, "idle").await;
    tokio::fs::write(
        handle.external.unwrap().answer_path.unwrap(),
        "The task completed.",
    )
    .await
    .unwrap();
    fixture.change_status("done", 11, 11).await;
    fixture.notice(": completed").await;
    fixture
        .update_matching(|update| update["external"]["notification_error"].is_string())
        .await;
    let result = snapshot(&fixture.sessions, &handle.rlm_child_id).await;
    assert_eq!(
        json!({"status":result["status"],"settled":result["settled"],"text":result["answer_text"]}),
        json!({"status":"done","settled":true,"text":"The task completed."})
    );
    assert!(result["external"]["notification_error"]
        .as_str()
        .unwrap()
        .contains("parent refused the follow-up"));
    assert!(result["error"]
        .as_str()
        .unwrap()
        .contains("Parent notification failed"));
    let persisted = external_store::load(&Path::new(&handle.session_dir).join("external.json"))
        .await
        .unwrap();
    assert!(!persisted.notice_sent);
    assert_eq!(
        serde_json::to_value(persisted.info.notification_error).unwrap(),
        result["external"]["notification_error"]
    );
    assert_eq!(fixture.prompt_count(), 1);
}

#[tokio::test]
async fn startup_failure_retains_a_durable_error_record_and_the_inspectable_pane() {
    let mut fixture = Fixture::new().await;
    fixture.faults.lock().unwrap().start_error = Some("codex is not installed");
    let error = fixture.sessions.spawn(request()).await.unwrap_err();
    assert!(error.to_string().contains("retained external child"));
    fixture.notice(": error").await;
    let rows = fixture.sessions.list_subagents().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "error");
    let durable = external_store::load(&Path::new(&rows[0].session_dir).join("external.json"))
        .await
        .unwrap();
    assert_eq!(
        json!({"status":durable.status,"settled":durable.settled,"closed":durable.closed}),
        json!({"status":"error","settled":true,"closed":false})
    );
    assert!(durable.error.unwrap().contains("codex is not installed"));
    assert!(fixture
        .history
        .lock()
        .unwrap()
        .iter()
        .all(|request| request["method"] != "pane.close"));
    assert_eq!(fixture.prompt_count(), 0);
    assert_eq!(
        fixture
            .history
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["method"] == "agent.start")
            .count(),
        1
    );
}

#[tokio::test]
async fn startup_waits_for_shell_foreground_then_retries_only_explicit_busy_refusals() {
    let mut fixture = Fixture::new().await;
    fixture.sessions.notify_turn_started();
    {
        let mut faults = fixture.faults.lock().unwrap();
        faults.foreground_processes = Some(Vec::new());
        faults.busy_starts_remaining = 1;
    }
    let spawning = tokio::spawn(fixture.sessions.spawn(request()));
    fixture.request("pane.process_info").await;
    assert!(fixture
        .history
        .lock()
        .unwrap()
        .iter()
        .all(|request| request["method"] != "agent.start"));
    fixture.faults.lock().unwrap().foreground_processes = None;
    let first = fixture.request("agent.start").await;
    let second = fixture.request("agent.start").await;
    let handle = tokio::time::timeout(TEST_BUDGET, spawning)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        first["params"], second["params"],
        "busy rejection cannot change the owned agent name or argv"
    );
    assert_eq!(handle.name, "review-api");
    assert_eq!(fixture.prompt_count(), 0);
    assert_eq!(
        fixture
            .history
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["method"] == "agent.start")
            .count(),
        2
    );
}

#[tokio::test]
async fn startup_observes_pending_launch_and_persists_recognition_before_waiting_for_interactive_idle(
) {
    let mut fixture = Fixture::new().await;
    fixture.sessions.notify_turn_started();
    let gate = Arc::new(Notify::new());
    {
        let mut faults = fixture.faults.lock().unwrap();
        faults.pending_start = true;
        faults.startup_get_gate = Some(Arc::clone(&gate));
    }
    let spawning = tokio::spawn(fixture.sessions.spawn(request()));
    fixture.request("agent.start").await;
    fixture.request("agent.get").await;
    assert!(
        !spawning.is_finished(),
        "an omitted pending agent label must not admit the task"
    );
    {
        let mut agent = fixture.agent.lock().await;
        agent["agent"] = json!("codex");
        agent["agent_status"] = json!("blocked");
    }
    fixture.faults.lock().unwrap().startup_get_gate = None;
    gate.notify_one();
    fixture.request("agent.get").await;
    let rows = fixture.sessions.list_subagents().await.unwrap();
    assert_eq!(rows.len(), 1);
    let manifest = Path::new(&rows[0].session_dir).join("external.json");
    assert_eq!(
        external_store::load(&manifest)
            .await
            .unwrap()
            .expected_agent
            .as_deref(),
        Some("codex")
    );
    assert!(
        !spawning.is_finished(),
        "recognition alone is not interactive readiness"
    );
    {
        let mut agent = fixture.agent.lock().await;
        agent["interactive_ready"] = json!(true);
        agent
            .as_object_mut()
            .unwrap()
            .retain(|key, _| key != "launch_pending");
        agent["agent_status"] = json!("working");
    }
    fixture.request("agent.get").await;
    assert!(
        !spawning.is_finished(),
        "interactive_ready during work cannot admit another task"
    );
    fixture.agent.lock().await["agent_status"] = json!("idle");
    let handle = tokio::time::timeout(TEST_BUDGET, spawning)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(handle.name, "review-api");
    assert_eq!(fixture.prompt_count(), 0);
    assert_eq!(
        fixture
            .history
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["method"] == "agent.start")
            .count(),
        1
    );
}

#[tokio::test]
async fn startup_transport_disconnect_is_not_retried_when_admission_is_uncertain() {
    let fixture = Fixture::new().await;
    fixture.faults.lock().unwrap().start_disconnect = true;
    let error = fixture.sessions.spawn(request()).await.unwrap_err();
    assert!(
        error.to_string().contains("complete JSON-lines frame"),
        "{error:#}"
    );
    assert_eq!(
        fixture
            .history
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["method"] == "agent.start")
            .count(),
        1
    );
    let rows = fixture.sessions.list_subagents().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "error");
    assert_eq!(fixture.prompt_count(), 0);
}

#[tokio::test]
async fn explicit_not_ready_startup_error_keeps_verified_pane_inspection_available() {
    let mut fixture = Fixture::new().await;
    {
        let mut faults = fixture.faults.lock().unwrap();
        faults.start_error_code = Some("agent_not_ready");
        faults.start_error = Some("agent prompt was not detected before its readiness deadline");
    }
    let error = fixture.sessions.spawn(request()).await.unwrap_err();
    assert!(error.to_string().contains("agent_not_ready"));
    let rows = fixture.sessions.list_subagents().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "error");
    let persisted = external_store::load(&Path::new(&rows[0].session_dir).join("external.json"))
        .await
        .unwrap();
    assert_eq!(persisted.expected_agent.as_deref(), Some("codex"));
    assert!(persisted.error.unwrap().contains("agent_not_ready"));
    let output = fixture
        .sessions
        .control_external(&rows[0].rlm_child_id, "read", Vec::new(), 80)
        .await
        .unwrap();
    assert_eq!(output["output"]["text"], "terminal evidence");
    fixture
        .sessions
        .control_external(&rows[0].rlm_child_id, "focus", Vec::new(), 80)
        .await
        .unwrap();
    assert_eq!(
        fixture.request("pane.focus").await["params"],
        json!({"pane_id":"w1:p2"})
    );
    fixture
        .sessions
        .control_external(&rows[0].rlm_child_id, "keys", vec!["enter".into()], 80)
        .await
        .unwrap();
    assert_eq!(
        fixture.request("agent.send_keys").await["params"]["keys"],
        json!(["enter"])
    );
    assert_eq!(
        fixture
            .history
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["method"] == "agent.start")
            .count(),
        1
    );
    assert_eq!(fixture.prompt_count(), 0);
}

#[tokio::test]
async fn corrupt_external_manifests_fail_roster_and_collect_instead_of_disappearing() {
    let fixture = Fixture::new().await;
    let identity = fixture.sessions.inner.identity.lock().unwrap().clone();
    let directory = fixture
        .sessions
        .inner
        .child_session_dir("sub-corrupt", &identity)
        .unwrap();
    tokio::fs::create_dir_all(&directory).await.unwrap();
    tokio::fs::write(directory.join("external.json"), b"{invalid manifest")
        .await
        .unwrap();
    fixture.sessions.reseed_from_ledger().await;
    for error in [
        fixture.sessions.list_subagents().await.unwrap_err(),
        fixture.sessions.collect(Vec::new(), 0).await.unwrap_err(),
    ] {
        assert!(error
            .to_string()
            .contains("Cannot restore external subagents"));
        assert!(error.to_string().contains("external.json"));
    }
    assert!(fixture.history.lock().unwrap().is_empty());
}

#[tokio::test]
async fn mixed_collect_and_the_running_feed_include_external_children_until_parent_close() {
    let fixture = Fixture::new().await;
    let mut running = fixture.sessions.subscribe_running();
    fixture.sessions.notify_turn_started();
    let handle = fixture.sessions.spawn(request()).await.unwrap();
    tokio::time::timeout(TEST_BUDGET, running.wait_for(|running| *running))
        .await
        .unwrap()
        .unwrap();
    fixture
        .sessions
        .push_test_child(RlmChildIdentity {
            rlm_child_id: "sub-native".into(),
            active_session_id: "native-live".into(),
            session_id: Some("native-file".into()),
            session_name: "native-review".into(),
        })
        .await;
    {
        let native = fixture.sessions.inner.children.lock().await[0].clone();
        let mut native = native.lock().await;
        native.settled_status = Some("done");
        native.settled = true;
        native.prompt_admitted = false;
        native.answer_text = Some("The native answer.".into());
        native.answer_preview = Some("The native answer.".into());
        native.answer_captured = true;
    }
    let all = fixture.sessions.collect(Vec::new(), 0).await.unwrap();
    let statuses: BTreeMap<_, _> = all
        .iter()
        .map(|result| (result.rlm_child_id.as_str(), result.status))
        .collect();
    assert_eq!(
        statuses,
        BTreeMap::from([
            ("sub-native", "done"),
            (handle.rlm_child_id.as_str(), "running")
        ])
    );
    let selected = fixture
        .sessions
        .collect(vec![handle.rlm_child_id.clone(), "sub-native".into()], 0)
        .await
        .unwrap();
    assert_eq!(selected.len(), 2);
    assert!(fixture.sessions.has_running_children());
    fixture
        .sessions
        .close_children(ChildCloseReason::Shutdown)
        .await
        .unwrap();
    tokio::time::timeout(TEST_BUDGET, running.wait_for(|running| !*running))
        .await
        .unwrap()
        .unwrap();
    assert!(!fixture.sessions.has_running_children());
    let closed = snapshot(&fixture.sessions, &handle.rlm_child_id).await;
    assert_eq!(
        json!({"status":closed["status"],"settled":closed["settled"]}),
        json!({"status":"cancelled","settled":true})
    );
    let durable = external_store::load(&Path::new(&handle.session_dir).join("external.json"))
        .await
        .unwrap();
    assert!(durable.closed);
}

#[tokio::test]
async fn a_closed_pane_is_published_when_tombstone_persistence_fails_and_delete_retries_it() {
    let fixture = Fixture::new().await;
    fixture.sessions.notify_turn_started();
    let handle = fixture.sessions.spawn(request()).await.unwrap();
    let manifest = Path::new(&handle.session_dir).join("external.json");
    tokio::fs::remove_file(&manifest).await.unwrap();
    tokio::fs::create_dir(&manifest).await.unwrap();
    let error = fixture
        .sessions
        .control_external(&handle.rlm_child_id, "close", Vec::new(), 80)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("tombstone persistence failed"),
        "{error:#}"
    );
    assert!(fixture.sessions.external_roster_summaries().is_empty());
    assert!(fixture.sessions.child_snapshots().await.is_empty());
    assert!(!fixture.sessions.has_running_children());
    let result = snapshot(&fixture.sessions, &handle.rlm_child_id).await;
    assert_eq!(
        json!({"status":result["status"],"settled":result["settled"],"state":result["external"]["state"]}),
        json!({"status":"cancelled","settled":true,"state":"closed"}),
    );
    // Delete must keep reporting the failed tombstone until persistence succeeds.
    let error = fixture
        .sessions
        .delete_subagent(handle.rlm_child_id.clone())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("tombstone persistence failed"),
        "{error:#}"
    );
    tokio::fs::remove_dir(&manifest).await.unwrap();
    let deleted = fixture
        .sessions
        .delete_subagent(handle.rlm_child_id.clone())
        .await
        .unwrap();
    assert_eq!(deleted.outcome, Some("deleted"));
    assert!(external_store::load(&manifest).await.unwrap().closed);
    fixture
        .sessions
        .control_external(&handle.rlm_child_id, "stop", Vec::new(), 80)
        .await
        .unwrap();
    let close_count = fixture
        .history
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request["method"] == "pane.close")
        .count();
    assert_eq!(
        close_count, 1,
        "tombstone retries must not close another pane"
    );
}

#[tokio::test]
async fn a_stale_observer_failure_preserves_the_newer_settled_turn() {
    let mut fixture = Fixture::new().await;
    fixture.sessions.notify_turn_started();
    let handle = fixture.sessions.spawn(request()).await.unwrap();
    // Hold the authoritative response while the old turn's observer is in flight.
    let agent_slot = Arc::clone(&fixture.agent);
    let mut agent = agent_slot.lock().await;
    fixture.sessions.notify_turn_done();
    fixture.request("agent.get").await;
    let record = fixture
        .sessions
        .inner
        .find_external(&handle.rlm_child_id)
        .await
        .unwrap()
        .unwrap();
    {
        let mut state = record.lock().await;
        // Model a newer follow-up already settled while an old observer unwinds.
        state.info.turn = 2;
        state.info.answer_path = Some(state.dir.join("turns/002/answer.md").display().to_string());
        state.info.state = "done".into();
        state.status = "completed".into();
        state.settled = true;
        state.notice_sent = true;
        state.text = "The second turn's answer.".into();
        state.persist().await.unwrap();
    }
    agent["terminal_id"] = json!("replacement-terminal");
    drop(agent);
    // The registry and this test retain one Arc each; the old observer releases
    // its third owner when it finishes unwinding. No timing delay is involved.
    tokio::time::timeout(TEST_BUDGET, async {
        while Arc::strong_count(&record) > 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("old observer did not finish");
    let result = snapshot(&fixture.sessions, &handle.rlm_child_id).await;
    assert_eq!(
        json!({"turn":result["external"]["turn"],"status":result["status"],"text":result["answer_text"],"error":result["error"]}),
        json!({"turn":2,"status":"done","text":"The second turn's answer.","error":null}),
    );
}
