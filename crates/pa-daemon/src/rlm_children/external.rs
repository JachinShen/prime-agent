//! Herdr-backed logical children. Herdr owns agent detection and terminal I/O;
//! Prime Agent owns turn admission, durable answers, and parent wakeups.
use super::external_store::{read_answer, ExternalRecord};
use super::{
    anyhow, bail, cap_text, create_rlm_child_terminal_notice, json, now_ms, Arc, Context,
    DaemonCommand, Duration, Instant, Map, Mutex, ParentIdentity, Path, PromptInput, Result,
    RlmChildResult, RlmChildTerminalNotice, RlmDeleteSubagentResult, RlmSpawnHandle,
    RlmSpawnRequest, RlmSubagentActivity, RlmSubagentEntry, SupervisorChildSessions,
    SupervisorChildSessionsInner, Value, ANSWER_TEXT_MAX_CHARS, NOTICE_DELIVERY_TIMEOUT_MS,
};
use crate::herdr_client::{HerdrApiError, HerdrClient, HerdrSubscription};
use pa_core::session_engine::agent_messaging::{AgentMessageDeliveryStatus, AgentMessageReceipt};
use pa_types::daemon::external_subagent::{ExternalSubagentInfo, ExternalSubagentOutput};
use std::collections::BTreeMap;

mod workspace;

const API_BUDGET: Duration = Duration::from_secs(15);

impl ExternalRecord {
    pub(super) fn entry(&self) -> RlmSubagentEntry {
        RlmSubagentEntry {
            rlm_child_id: self.child_id.clone(),
            active_session_id: None,
            session_id: None,
            session_name: self.name.clone(),
            session_dir: self.dir.display().to_string(),
            status: match self.status.as_str() {
                "completed" => "completed",
                "error" => "error",
                "cancelled" => "cancelled",
                _ => "running",
            },
            activity: (!self.settled).then_some(RlmSubagentActivity {
                kind: "executing",
                tool_name: None,
            }),
            tool_use_count: None,
            duration_ms: Some(now_ms().saturating_sub(self.started_at_ms)),
            answer_preview: (!self.text.is_empty()).then(|| cap_text(&self.text, 500)),
            replied_since_task: None,
            progress_note: self.error.clone(),
            label: Some(self.info.agent.clone()),
            last_activity_at: Some(self.started_at_ms),
            activity_stale_ms: None,
            external: Some(self.info.clone()),
        }
    }
    pub(super) fn result(&self) -> RlmChildResult {
        RlmChildResult {
            rlm_child_id: self.child_id.clone(),
            session_name: Some(self.name.clone()),
            session_dir: Some(self.dir.display().to_string()),
            status: match self.status.as_str() {
                "completed" => "done",
                "error" => "error",
                "cancelled" => "cancelled",
                _ => "running",
            },
            settled: self.settled,
            answer_preview: (!self.text.is_empty()).then(|| cap_text(&self.text, 500)),
            answer_text: (!self.text.is_empty()).then(|| self.text.clone()),
            error: self.error.clone(),
            duration_ms: Some(now_ms().saturating_sub(self.started_at_ms)),
            tool_use_count: None,
            replied_since_task: None,
            external: Some(self.info.clone()),
        }
    }
    pub(super) fn snapshot(&self) -> Value {
        let mut snapshot = json!({"id":self.child_id,"rlmChildId":self.child_id,"sessionName":self.name,
            "sessionDir":self.dir,"label":self.info.agent,"status":self.status,
            "durationMs":now_ms().saturating_sub(self.started_at_ms),"answerPreview":cap_text(&self.text,500),
            "backend":"herdr","external":self.info,"error":self.error});
        if let Some(worktree) = &self.info.worktree {
            snapshot["cwd"] = json!(worktree.path);
        }
        snapshot
    }
}

impl SupervisorChildSessions {
    pub(crate) fn check_external_restore(&self) -> Result<()> {
        self.inner.check_external_restore()
    }
    pub(crate) fn set_herdr_env(&self, env: BTreeMap<String, String>) {
        *self
            .inner
            .external_env
            .lock()
            .expect("external environment lock") = env;
    }
    pub(crate) fn external_roster_summaries(&self) -> Vec<Value> {
        self.inner
            .external_roster
            .lock()
            .expect("external roster lock")
            .clone()
    }
    pub(crate) async fn control_external(
        &self,
        target: &str,
        action: &str,
        keys: Vec<String>,
        lines: u32,
    ) -> Result<Value> {
        let record = self
            .inner
            .find_external(target)
            .await?
            .ok_or_else(|| anyhow!("Unknown external subagent: {target}"))?;
        self.inner
            .external_control(&record, action, keys, lines)
            .await
    }
    pub(crate) async fn send_external(
        &self,
        target: &str,
        message: &str,
    ) -> Result<Option<AgentMessageReceipt>> {
        let Some(record) = self.inner.find_external(target).await? else {
            return Ok(None);
        };
        {
            let mut current = record.lock().await;
            let mut state = current.clone();
            if state.closed {
                bail!("External subagent is closed: {target}");
            }
            if !state.settled {
                bail!(
                    "External subagent already has an active turn; use keys for interactive input"
                );
            }
            let client = HerdrClient::new(state.socket_path.clone());
            let agent = owned_agent(&client, &state).await?;
            require_ready(&agent)?;
            state.info.turn += 1;
            state.info.state = "pending".into();
            state.info.notification_error = None;
            state.info.answer_truncated = false;
            state.info.output = None;
            state.info.answer_path = Some(
                state
                    .dir
                    .join("turns")
                    .join(format!("{:03}", state.info.turn))
                    .join("answer.md")
                    .display()
                    .to_string(),
            );
            state.text.clear();
            state.error = None;
            state.status = "running".into();
            state.settled = false;
            state.notice_sent = false;
            state.started_at_ms = now_ms();
            write_prompt(&state, message).await?;
            state.persist().await?;
            *current = state;
        }
        self.inner.external_changed().await;
        let info = record.lock().await.info.clone();
        let cwd = self
            .inner
            .identity
            .lock()
            .expect("identity lock")
            .cwd
            .clone()
            .unwrap_or_else(|| "/".into());
        self.inner.model_refusal_telemetry.track_external(
            pa_telemetry::ExternalSubagentEvent::Started {
                agent_kind: pa_telemetry::ExternalAgentKind::from_agent(&info.agent),
                worktree: info.worktree.is_some(),
            },
            Path::new(&cwd),
        );
        self.inner.launch_external(Arc::clone(&record), true);
        let state = record.lock().await;
        Ok(Some(AgentMessageReceipt {
            id: format!("{}-turn-{}", state.child_id, state.info.turn),
            target: state.child_id.clone(),
            target_session_id: None,
            target_session_name: Some(state.name.clone()),
            target_runtime_kind: Some("external".into()),
            message: message.into(),
            delivery_status: AgentMessageDeliveryStatus::Queued,
            delivery_mode: Some("followUp"),
            receiver_role: Some(
                pa_core::session_engine::agent_messaging::AgentFamilyRelationship::Child,
            ),
            delivered_at: None,
            queued_at: Some(now_ms().to_string()),
        }))
    }
}

impl SupervisorChildSessionsInner {
    pub(super) fn check_external_restore(&self) -> Result<()> {
        if let Some(error) = self
            .external_restore_error
            .lock()
            .expect("external restore error lock")
            .as_ref()
        {
            bail!("Cannot restore external subagents: {error}");
        }
        Ok(())
    }
    pub(super) async fn find_external(
        &self,
        target: &str,
    ) -> Result<Option<Arc<Mutex<ExternalRecord>>>> {
        self.check_external_restore()?;
        let mut found = None;
        for record in self.external_children.lock().await.iter() {
            let state = record.lock().await;
            if state.child_id == target || (!state.closed && state.name == target) {
                if found.is_some() {
                    bail!("External subagent selector is ambiguous: {target}");
                }
                found = Some(Arc::clone(record));
            }
        }
        Ok(found)
    }
    pub(super) async fn external_running(&self) -> bool {
        for record in self.external_children.lock().await.iter() {
            if !record.lock().await.settled {
                return true;
            }
        }
        false
    }
    pub(super) async fn external_changed(&self) {
        let identity = self.identity.lock().expect("identity lock").clone();
        let mut rows = Vec::new();
        let mut snapshots = Vec::new();
        for record in self.external_children.lock().await.iter() {
            let state = record.lock().await;
            if state.closed {
                let mut snapshot = state.snapshot();
                snapshot["deleted"] = json!(true);
                snapshots.push(snapshot);
                continue;
            }
            let mut row = state.snapshot();
            let object = row.as_object_mut().expect("external snapshot object");
            object.extend(json!({"runtimeKind":"subagent","parentSessionId":identity.session_id,
                "parentSessionPath":identity.session_file,"parentActiveSessionId":self.parent_active_session_id,
                "activeSessionId":null,"sessionId":null,"activity":if state.settled {"idle"}else{"working"},
                "taskState":state.status,"runtimeResident":!state.closed}).as_object().expect("roster object").clone());
            rows.push(row);
            snapshots.push(state.snapshot());
        }
        *self.external_roster.lock().expect("external roster lock") = rows;
        self.refresh_running().await;
        self.settle_notify.notify_waiters();
        let sink = self
            .child_update_sink
            .lock()
            .expect("child update sink lock")
            .clone();
        if let Some(sink) = sink {
            for row in snapshots {
                sink(row);
            }
        }
    }
    pub(super) async fn spawn_external(
        self: &Arc<Self>,
        request: RlmSpawnRequest,
        child_id: String,
        name: String,
        identity: ParentIdentity,
    ) -> Result<RlmSpawnHandle> {
        self.check_external_restore()?;
        if request.model.is_some() || request.thinking.is_some() {
            bail!("External subagents use agent_args for model and reasoning options");
        }
        let env = self
            .external_env
            .lock()
            .expect("external environment lock")
            .clone();
        let client = HerdrClient::from_env(&env)?;
        let caller = client.pane_id().context("Herdr caller pane missing")?;
        client.workspace_id().context("Herdr workspace missing")?;
        let pane = client
            .request("pane.get", json!({"pane_id":caller}), API_BUDGET)
            .await?;
        let workspace = pane
            .pointer("/pane/workspace_id")
            .and_then(Value::as_str)
            .context("Herdr caller workspace missing")?;
        if identity.session_id.is_none() {
            bail!("External subagents require a persisted parent session identity");
        }
        let (pane, workspace, worktree) = if let Some(worktree) = &request.worktree {
            let created = workspace::create_worktree(
                &client,
                worktree,
                identity
                    .cwd
                    .as_deref()
                    .context("Worktree subagents require a parent cwd")?,
                &child_id,
                &name,
            )
            .await?;
            (created.pane, created.workspace_id, Some(created.worktree))
        } else {
            let created = client
                .request(
                    "tab.create",
                    json!({"workspace_id":workspace,"cwd":identity.cwd,
                        "label":name,"focus":false}),
                    API_BUDGET,
                )
                .await?;
            let pane = created
                .get("root_pane")
                .context("Herdr tab.create returned no root pane")?;
            (pane.clone(), workspace.to_string(), None)
        };
        let pane_id = required_str(&pane, "pane_id")?.to_string();
        let terminal_id = required_str(&pane, "terminal_id")?.to_string();
        if required_str(&pane, "workspace_id")? != workspace {
            bail!("Herdr created pane in a different workspace: {pane_id}");
        }
        let agent = request.agent.context("external agent kind missing")?;
        let agent_name = format!("pa-{}", uuid::Uuid::new_v4().simple());
        let agent_name = agent_name[..31].to_string();
        let dir = self.child_session_dir(&child_id, &identity)?;
        let answer = dir.join("turns/001/answer.md");
        let state = ExternalRecord {
            child_id: child_id.clone(),
            name: name.clone(),
            dir: dir.clone(),
            socket_path: env
                .get("HERDR_SOCKET_PATH")
                .context("Herdr socket missing")?
                .clone(),
            info: ExternalSubagentInfo {
                agent,
                workspace_id: workspace,
                pane_id,
                terminal_id,
                agent_name,
                turn: 1,
                state: "starting".into(),
                worktree,
                answer_path: Some(answer.display().to_string()),
                answer_truncated: false,
                output: None,
                notification_error: None,
            },
            expected_agent: None,
            status: "running".into(),
            settled: false,
            text: String::new(),
            error: None,
            baseline_state_seq: 0,
            baseline_completion_seq: None,
            started_at_ms: now_ms(),
            notice_sent: false,
            closed: false,
        };
        let prepared = async {
            write_prompt(&state, &request.prompt).await?;
            state.persist().await
        }
        .await;
        prepared.with_context(|| format!("Prepare external child {child_id}; pane {} and checkout {:?} retained, artifacts {}",
            state.info.pane_id, state.info.worktree.as_ref().map(|worktree| &worktree.path), dir.display()))?;
        let record = Arc::new(Mutex::new(state));
        self.external_children
            .lock()
            .await
            .push(Arc::clone(&record));
        self.external_changed().await;
        let start_info = record.lock().await.info.clone();
        let started = async {
            // Creating a tab allocates its terminal before shell startup finishes.
            // Foreground ownership precedes Herdr's shell-integration readiness.
            tokio::time::timeout(Duration::from_secs(30), async {
                let mut poll = tokio::time::interval(Duration::from_millis(100));
                loop {
                    poll.tick().await;
                    let process = client
                        .request(
                            "pane.process_info",
                            json!({"pane_id":start_info.pane_id}),
                            API_BUDGET,
                        )
                        .await?;
                    let process = process
                        .get("process_info")
                        .context("Herdr shell readiness response missing process_info")?;
                    let shell = process.get("shell_pid").and_then(Value::as_u64);
                    let foreground = process
                        .get("foreground_processes")
                        .and_then(Value::as_array)
                        .context("Herdr shell readiness response missing foreground_processes")?;
                    if let Some(shell) = shell {
                        if foreground.len() == 1
                            && foreground[0].get("pid").and_then(Value::as_u64) == Some(shell)
                        {
                            break;
                        }
                    }
                }
                anyhow::Ok(())
            })
            .await
            .context("Herdr pane shell did not become available within 30 seconds")??;
            let ready_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            let params = json!({"pane_id":start_info.pane_id,"name":start_info.agent_name,
                "kind":start_info.agent,"args":request.agent_args,"timeout_ms":120_000});
            loop {
                match client.request("agent.start", params.clone(), Duration::from_secs(125)).await {
                    Ok(started) => break Ok(started),
                    Err(error) => {
                        // This typed refusal guarantees no agent was started. An uncertain
                        // transport result or any other API error must never resend argv.
                        if error.downcast_ref::<HerdrApiError>().is_none_or(|error| error.code != "agent_pane_busy") {
                            break Err(error);
                        }
                        let next = tokio::time::Instant::now() + Duration::from_millis(500);
                        if next >= ready_deadline {
                            break Err(error.context("Herdr pane remained busy during the 30-second startup readiness budget"));
                        }
                        tokio::time::sleep_until(next).await;
                    }
                }
            }
        }
        .await;
        let admission = async {
            let started = started.with_context(|| {
                format!("Start external agent {child_id}; pane retained for inspection")
            })?;
            let mut agent = started
                .get("agent")
                .context("Herdr agent.start returned no agent")?.clone();
            let readiness = tokio::time::timeout(Duration::from_secs(120), async {
                loop {
                    let recognized = match agent.get("agent") {
                        None | Some(Value::Null) => None,
                        Some(Value::String(kind)) if !kind.trim().is_empty() => Some(kind.clone()),
                        _ => bail!("Herdr startup response has an invalid agent identity"),
                    };
                    // AgentInfo omits absent labels and false readiness flags on the
                    // socket wire; present values still require their schema type.
                    let ready = agent.get("interactive_ready")
                        .map(|value| value.as_bool().context("Herdr interactive_ready must be boolean"))
                        .transpose()?.unwrap_or(false);
                    let pending = agent.get("launch_pending")
                        .map(|value| value.as_bool().context("Herdr launch_pending must be boolean"))
                        .transpose()?.unwrap_or(false);
                    let status = required_str(&agent,"agent_status")?;
                    {
                        let mut current = record.lock().await;
                        let mut candidate = current.clone();
                        if candidate.expected_agent.is_none() { candidate.expected_agent = recognized; }
                        verify_agent(&candidate,&agent)?;
                        if ready && !pending && matches!(status,"idle"|"done") {
                            if candidate.expected_agent.is_none() { bail!("Herdr reported interactive readiness without recognizing the agent"); }
                            candidate.baseline_state_seq = state_sequence(&agent)?;
                            candidate.baseline_completion_seq = agent.get("completion_seq").and_then(Value::as_u64);
                            candidate.info.state = "pending".into();
                            candidate.persist().await?;
                            let info = candidate.info.clone();
                            *current = candidate;
                            return Ok(info);
                        }
                        if candidate.expected_agent != current.expected_agent {
                            candidate.persist().await?;
                            *current = candidate;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let result = client.request("agent.get",json!({"target":start_info.agent_name}),API_BUDGET).await?;
                    agent = result.get("agent").context("Herdr agent.get returned no startup agent")?.clone();
                }
            }).await;
            match readiness {
                Ok(result) => result,
                Err(_) => Err(HerdrApiError {
                    code:"agent_not_ready".into(),
                    message:"External agent did not become interactive and idle within 120 seconds; pane retained for inspection".into(),
                }.into()),
            }
        }
        .await;
        let info = match admission {
            Ok(info) => info,
            Err(error) => {
                let error = if error
                    .downcast_ref::<HerdrApiError>()
                    .is_some_and(|error| error.code == "agent_not_ready")
                {
                    // Herdr recognized this agent but did not observe a ready prompt.
                    // Preserve that startup error while retaining verified inspection access.
                    let inspection = async {
                        let result = client
                            .request(
                                "agent.get",
                                json!({"target":start_info.agent_name}),
                                API_BUDGET,
                            )
                            .await?;
                        let agent = result
                            .get("agent")
                            .context("Herdr inspection response missing agent")?;
                        let mut current = record.lock().await;
                        let mut candidate = current.clone();
                        candidate.expected_agent = Some(required_str(agent, "agent")?.to_string());
                        verify_agent(&candidate, agent)?;
                        candidate.persist().await?;
                        *current = candidate;
                        anyhow::Ok(())
                    }
                    .await;
                    match inspection {
                        Ok(()) => error,
                        Err(inspection) => error.context(format!(
                            "Startup inspection identity unavailable: {inspection:#}"
                        )),
                    }
                } else {
                    error
                };
                let detail = format!("{error:#}");
                self.finish_external(&record, 1, Err(error)).await?;
                bail!(
                    "{detail}; retained external child {child_id} in {}",
                    dir.display()
                );
            }
        };
        self.external_changed().await;
        self.model_refusal_telemetry.track_external(
            pa_telemetry::ExternalSubagentEvent::Started {
                agent_kind: pa_telemetry::ExternalAgentKind::from_agent(&info.agent),
                worktree: info.worktree.is_some(),
            },
            Path::new(identity.cwd.as_deref().unwrap_or("/")),
        );
        self.launch_external(record, true);
        Ok(RlmSpawnHandle {
            rlm_child_id: child_id,
            name,
            session_dir: dir.display().to_string(),
            model: None,
            external: Some(info),
        })
    }
    fn launch_external(self: &Arc<Self>, record: Arc<Mutex<ExternalRecord>>, admit: bool) {
        let this = Arc::clone(self);
        let parent_turn = *self.parent_turn.borrow();
        tokio::spawn(async move {
            if parent_turn.running {
                this.wait_turn_done(parent_turn.completed).await;
            }
            let turn = record.lock().await.info.turn;
            if let Err(error) = this.run_external(&record, turn, admit).await {
                let settlement = if record.lock().await.settled {
                    Err(error)
                } else {
                    this.finish_external(&record, turn, Err(error)).await
                };
                if let Err(persist_error) = settlement {
                    eprintln!("pa-daemon: external subagent settlement failed: {persist_error:#}");
                    let mut state = record.lock().await;
                    if state.info.turn != turn || state.closed {
                        return;
                    }
                    let detail = format!("Settlement failed: {persist_error:#}");
                    state.error = Some(detail.clone());
                    state.info.notification_error = Some(detail);
                    state.status = "error".into();
                    state.settled = true;
                    drop(state);
                    this.external_changed().await;
                    if let Err(error) = this.notify_external(&record, turn, "error").await {
                        eprintln!(
                            "pa-daemon: could not report external persistence failure: {error:#}"
                        );
                    }
                }
            }
        });
    }
    async fn run_external(
        self: &Arc<Self>,
        record: &Arc<Mutex<ExternalRecord>>,
        turn: u64,
        admit: bool,
    ) -> Result<()> {
        let snapshot = record.lock().await.clone();
        if snapshot.closed || snapshot.settled {
            return Ok(());
        }
        if snapshot.status != "running" {
            let result = if let Some(error) = snapshot.error {
                Err(anyhow!(error))
            } else {
                Ok((snapshot.text, snapshot.info.answer_truncated))
            };
            return self.finish_external(record, turn, result).await;
        }
        let client = HerdrClient::new(snapshot.socket_path.clone());
        // Subscribe before prompt admission. Events are hints; authoritative status is re-read.
        let mut subscription = external_subscription(&client, &snapshot.info.pane_id).await?;
        let before = owned_agent(&client, &snapshot).await?;
        let mut agent = before.clone();
        if admit {
            require_ready(&before)?;
            let prompt_path = snapshot
                .dir
                .join("turns")
                .join(format!("{turn:03}"))
                .join("prompt.md");
            let prompt = tokio::fs::read_to_string(&prompt_path)
                .await
                .with_context(|| format!("Read {}", prompt_path.display()))?;
            {
                let mut state = record.lock().await;
                if state.info.turn != turn || state.closed {
                    return Ok(());
                }
                state.baseline_state_seq = state_sequence(&before)?;
                state.baseline_completion_seq =
                    before.get("completion_seq").and_then(Value::as_u64);
                // Persist before transmitting; on reconnect this turn is observed, never resubmitted.
                state.info.state = "delivery-uncertain".into();
                state.persist().await?;
            }
            let prompted = client.request("agent.prompt",json!({"target":snapshot.info.agent_name,"text":prompt}),API_BUDGET).await
            .context("External prompt admission failed; delivery may be uncertain, prompt will not be resent")?;
            agent = prompted
                .get("agent")
                .context("Herdr agent.prompt returned no agent")?
                .clone();
        }
        let mut blocked_revision = None;
        let mut unknown_since = None;
        loop {
            {
                let mut state = record.lock().await;
                if state.info.turn != turn || state.closed || state.settled {
                    return Ok(());
                }
                verify_agent(&state, &agent)?;
                state.info.pane_id = required_str(&agent, "pane_id")?.into();
                let status = required_str(&agent, "agent_status")?;
                state.info.state = status.into();
                let newer_completion = agent
                    .get("completion_seq")
                    .and_then(Value::as_u64)
                    .is_some_and(|seq| seq > state.baseline_completion_seq.unwrap_or(0));
                let newer_state = state_sequence(&agent)? > state.baseline_state_seq;
                let complete = match status {
                    "idle" | "done" => newer_completion || (status == "done" && newer_state),
                    "working" | "blocked" | "unknown" => false,
                    _ => bail!("Unrecognized Herdr agent status: {status}"),
                };
                state.persist().await?;
                if complete {
                    drop(state);
                    self.external_control(record, "read", Vec::new(), 80)
                        .await?;
                    let answer = snapshot
                        .info
                        .answer_path
                        .as_deref()
                        .context("External turn has no answer path")?;
                    let answer = read_answer(Path::new(answer), ANSWER_TEXT_MAX_CHARS).await;
                    return self.finish_external(record, turn, answer).await;
                }
                if status == "unknown" {
                    let since = unknown_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= Duration::from_secs(60) {
                        bail!("Herdr could not determine the external agent state for 60 seconds");
                    }
                } else {
                    unknown_since = None;
                }
                if status == "blocked" && blocked_revision != Some(state_sequence(&agent)?) {
                    blocked_revision = Some(state_sequence(&agent)?);
                    drop(state);
                    self.external_control(record, "read", Vec::new(), 80)
                        .await?;
                    self.external_changed().await;
                    self.notify_external(
                        record,
                        turn,
                        &format!(
                            "blocked at state {}; interactive input required",
                            blocked_revision.unwrap_or(0)
                        ),
                    )
                    .await?;
                }
            }
            self.external_changed().await;
            // Timeouts re-check state without resending the task. A broken event connection
            // must reconnect successfully before observation can continue.
            match tokio::time::timeout(
                Duration::from_secs(30),
                subscription.next(Duration::from_secs(35)),
            )
            .await
            {
                Ok(Ok(_)) | Err(_) => {}
                Ok(Err(error)) => {
                    {
                        let mut state = record.lock().await;
                        state.info.state = "recovering".into();
                        state.persist().await?;
                    }
                    self.external_changed().await;
                    let current = record.lock().await.clone();
                    subscription = external_subscription(&client, &current.info.pane_id)
                        .await
                        .with_context(|| {
                            format!("Herdr event connection lost ({error:#}); reconnect failed")
                        })?;
                }
            }
            let snapshot = record.lock().await.clone();
            agent = owned_agent(&client, &snapshot).await?;
        }
    }
    async fn finish_external(
        &self,
        record: &Arc<Mutex<ExternalRecord>>,
        turn: u64,
        result: Result<(String, bool)>,
    ) -> Result<()> {
        let state = {
            let mut current = record.lock().await;
            let mut state = current.clone();
            if state.info.turn != turn || state.settled || state.closed {
                return Ok(());
            }
            match result {
                Ok((text, truncated)) => {
                    state.text = text;
                    state.info.answer_truncated = truncated;
                    state.status = "completed".into();
                }
                Err(error) => {
                    state.error = Some(format!("{error:#}"));
                    state.status = "error".into();
                }
            }
            state.settled = true;
            state.persist().await?;
            *current = state.clone();
            state
        };
        let cwd = self
            .identity
            .lock()
            .expect("identity lock")
            .cwd
            .clone()
            .unwrap_or_else(|| "/".into());
        self.model_refusal_telemetry.track_external(
            pa_telemetry::ExternalSubagentEvent::Completed {
                agent_kind: pa_telemetry::ExternalAgentKind::from_agent(&state.info.agent),
                outcome: match state.status.as_str() {
                    "completed" => pa_telemetry::ExternalSubagentOutcome::Completed,
                    "cancelled" => pa_telemetry::ExternalSubagentOutcome::Cancelled,
                    _ => pa_telemetry::ExternalSubagentOutcome::Error,
                },
                duration_ms: now_ms().saturating_sub(state.started_at_ms),
                answer_truncated: state.info.answer_truncated,
                output_truncated: state
                    .info
                    .output
                    .as_ref()
                    .is_some_and(|output| output.truncated),
            },
            Path::new(&cwd),
        );
        self.deliver_external_completion(record, turn).await?;
        self.external_changed().await;
        let hook = self.settle_hook.lock().expect("settle hook lock").clone();
        if let Some(hook) = hook {
            std::thread::spawn(move || hook());
        }
        Ok(())
    }
    async fn deliver_external_completion(
        &self,
        record: &Arc<Mutex<ExternalRecord>>,
        turn: u64,
    ) -> Result<()> {
        let snapshot = record.lock().await.clone();
        if snapshot.info.turn != turn || snapshot.notice_sent || snapshot.closed {
            return Ok(());
        }
        if let Err(error) = self.notify_external(record, turn, &snapshot.status).await {
            let mut state = record.lock().await;
            if state.info.turn != turn {
                return Ok(());
            }
            state.info.notification_error = Some(format!("{error:#}"));
            state.error = Some(format!("Parent notification failed: {error:#}"));
            state.persist().await?;
        }
        self.external_changed().await;
        Ok(())
    }
    async fn notify_external(
        &self,
        record: &Arc<Mutex<ExternalRecord>>,
        turn: u64,
        event: &str,
    ) -> Result<()> {
        let state = record.lock().await.clone();
        if state.info.turn != turn || state.closed {
            return Ok(());
        }
        let content = format!(
            "[herdr-subagent] {}: {event} (turn {})\n{}\n{}",
            state.name,
            state.info.turn,
            state.info.answer_path.as_deref().unwrap_or(""),
            cap_text(state.error.as_deref().unwrap_or(&state.text), 2000)
        );
        let mut message = create_rlm_child_terminal_notice(
            &RlmChildTerminalNotice::CompletedWithoutReply {
                child_id: state.child_id.clone(),
                session_name: state.name.clone(),
                last_assistant_text_preview: None,
            },
            now_ms(),
        );
        message.content = pa_types::ai::UserContent::Text(content.clone());
        message.details = Some(
            json!({"childId":state.child_id,"backend":"herdr","turn":state.info.turn,"event":event,
            "externalNoticeId":format!("{}-{}-{event}",state.child_id,state.info.turn)}),
        );
        let wire = serde_json::to_value(pa_types::session::AgentMessage::Custom(message))?;
        self.command(
            &DaemonCommand::FollowUp {
                id: None,
                active_session_id: self.parent_active_session_id.clone(),
                message: content,
                input: PromptInput {
                    content: None,
                    images: None,
                    streaming_behavior: None,
                    queue_if_busy: None,
                    expand_prompt_templates: None,
                    source: None,
                    agent_message_id: Some(format!(
                        "{}-{}-{event}",
                        state.child_id, state.info.turn
                    )),
                    custom_message: Some(wire),
                    queue_key: None,
                    prefix_messages: None,
                    admission_id: None,
                    rlm_notice_nonce: Some(crate::child_status_notices::mint()),
                },
                rest: Map::default(),
            },
            NOTICE_DELIVERY_TIMEOUT_MS,
        )
        .await?;
        let mut current = record.lock().await;
        if current.info.turn == state.info.turn && state.status != "running" {
            let mut accepted = current.clone();
            accepted.notice_sent = true;
            accepted.info.notification_error = None;
            accepted.persist().await?;
            *current = accepted;
        }
        Ok(())
    }
    pub(super) async fn external_control(
        &self,
        record: &Arc<Mutex<ExternalRecord>>,
        action: &str,
        keys: Vec<String>,
        lines: u32,
    ) -> Result<Value> {
        let snapshot = record.lock().await.clone();
        if snapshot.closed {
            if matches!(action, "close" | "stop") {
                // The pane already closed; retry only its tombstone, never control
                // a replacement agent that may now occupy the old pane id.
                record.lock().await.persist().await.context(
                    "External pane is closed but tombstone persistence failed; retry close",
                )?;
                return Ok(json!({"closed":true}));
            }
            bail!("External subagent pane is closed");
        }
        let client = HerdrClient::new(snapshot.socket_path.clone());
        let agent = owned_agent(&client, &snapshot).await?;
        let pane_id = required_str(&agent, "pane_id")?;
        match action {
            "read" => {
                if !(1..=2000).contains(&lines) {
                    bail!("lines must be between 1 and 2000");
                }
                let source = match required_str(&agent, "agent_status")? {
                    "idle" | "done" => "recent_unwrapped",
                    "working" | "blocked" | "unknown" => "visible",
                    status => bail!("Unrecognized Herdr agent status for terminal read: {status}"),
                };
                let result=client.request("agent.read",json!({"target":snapshot.info.agent_name,"source":source,"lines":lines,"strip_ansi":true,"format":"text"}),API_BUDGET).await?;
                let read = result
                    .get("read")
                    .context("Herdr agent.read returned no terminal evidence")?;
                if read
                    .get("pane_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id != pane_id)
                {
                    bail!("Herdr output belongs to a different pane");
                }
                let text = required_str(read, "text")?;
                let output = ExternalSubagentOutput {
                    text: cap_text(text, ANSWER_TEXT_MAX_CHARS),
                    source: required_str(read, "source")?.into(),
                    revision: read
                        .get("revision")
                        .and_then(Value::as_u64)
                        .context("Herdr output revision missing")?,
                    truncated: read
                        .get("truncated")
                        .and_then(Value::as_bool)
                        .context("Herdr output truncation missing")?
                        || text.chars().count() > ANSWER_TEXT_MAX_CHARS,
                };
                let mut state = record.lock().await;
                state.info.output = Some(output.clone());
                state.info.pane_id = pane_id.into();
                state.persist().await?;
                drop(state);
                self.external_changed().await;
                Ok(json!({"output":output}))
            }
            "keys" => {
                client
                    .request(
                        "agent.send_keys",
                        json!({"target":snapshot.info.agent_name,"keys":keys}),
                        API_BUDGET,
                    )
                    .await
            }
            "focus" => {
                client
                    .request("pane.focus", json!({"pane_id":pane_id}), API_BUDGET)
                    .await
            }
            "close" | "stop" => {
                let mut state = record.lock().await;
                if state.closed {
                    state.persist().await.context(
                        "External pane is closed but tombstone persistence failed; retry close",
                    )?;
                    return Ok(json!({"closed":true}));
                }
                client
                    .request("pane.close", json!({"pane_id":pane_id}), API_BUDGET)
                    .await?;
                let cancelled = !state.settled;
                state.closed = true;
                state.settled = true;
                if state.status == "running" {
                    state.status = "cancelled".into();
                }
                state.info.state = "closed".into();
                let persisted = state.persist().await;
                let completed = state.clone();
                drop(state);
                if cancelled {
                    let cwd = self
                        .identity
                        .lock()
                        .expect("identity lock")
                        .cwd
                        .clone()
                        .unwrap_or_else(|| "/".into());
                    self.model_refusal_telemetry.track_external(
                        pa_telemetry::ExternalSubagentEvent::Completed {
                            agent_kind: pa_telemetry::ExternalAgentKind::from_agent(
                                &completed.info.agent,
                            ),
                            outcome: pa_telemetry::ExternalSubagentOutcome::Cancelled,
                            duration_ms: now_ms().saturating_sub(completed.started_at_ms),
                            answer_truncated: completed.info.answer_truncated,
                            output_truncated: completed
                                .info
                                .output
                                .as_ref()
                                .is_some_and(|output| output.truncated),
                        },
                        Path::new(&cwd),
                    );
                }
                // Herdr confirmed closure, so publish it even if the durable
                // tombstone failed. The failed RPC remains retryable above.
                self.external_changed().await;
                persisted.context(
                    "External pane is closed but tombstone persistence failed; retry close",
                )?;
                Ok(json!({"closed":true}))
            }
            _ => bail!("Unknown external subagent action: {action}"),
        }
    }
    pub(super) async fn delete_external(
        &self,
        record: &Arc<Mutex<ExternalRecord>>,
    ) -> Result<RlmDeleteSubagentResult> {
        self.external_control(record, "close", Vec::new(), 80)
            .await?;
        let state = record.lock().await;
        Ok(RlmDeleteSubagentResult {
            subagent: state.entry(),
            outcome: Some("deleted"),
        })
    }
    pub(super) async fn close_external_children(&self) -> Result<()> {
        let records = self.external_children.lock().await.clone();
        let mut failure = None;
        for record in records {
            if let Err(error) = self
                .external_control(&record, "close", Vec::new(), 80)
                .await
            {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }
    pub(super) async fn restore_external(self: &Arc<Self>) -> Result<()> {
        let identity = self.identity.lock().expect("identity lock").clone();
        let Some(parent_id) = identity.session_id else {
            return Ok(());
        };
        let root = self.agent_dir.join("session-artifacts").join(parent_id);
        let mut dirs = match tokio::fs::read_dir(&root).await {
            Ok(dirs) => dirs,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        while let Some(entry) = dirs.next_entry().await? {
            if !entry.file_type().await?.is_dir() {
                continue;
            }
            let path = entry.path().join("external.json");
            if !tokio::fs::try_exists(&path).await? {
                continue;
            }
            let state = super::external_store::load(&path).await?;
            if self.find_external(&state.child_id).await?.is_some() {
                continue;
            }
            let resume = !state.settled;
            let pending_notice = !state.notice_sent && !state.closed;
            let turn = state.info.turn;
            let admit = state.info.state == "pending";
            let record = Arc::new(Mutex::new(state));
            self.external_children
                .lock()
                .await
                .push(Arc::clone(&record));
            if resume {
                self.launch_external(record, admit);
            } else if pending_notice {
                let this = Arc::clone(self);
                tokio::spawn(async move {
                    if let Err(error) = this.deliver_external_completion(&record, turn).await {
                        eprintln!("pa-daemon: restore external notice failed: {error:#}");
                    }
                });
            }
        }
        self.external_changed().await;
        Ok(())
    }
}

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("Herdr response missing string {key}"))
}
fn require_ready(agent: &Value) -> Result<()> {
    match required_str(agent, "agent_status")? {
        "idle" | "done" => Ok(()),
        status => bail!(
            "External agent is {status}; wait for it to become idle before sending a new task"
        ),
    }
}
fn state_sequence(agent: &Value) -> Result<u64> {
    agent
        .get("state_change_seq")
        .and_then(Value::as_u64)
        .context(
            "Herdr agent response missing state_change_seq; lifecycle observation is unsupported",
        )
}
fn verify_agent(state: &ExternalRecord, agent: &Value) -> Result<()> {
    state_sequence(agent)?;
    if required_str(agent, "terminal_id")? != state.info.terminal_id
        || required_str(agent, "workspace_id")? != state.info.workspace_id
        || agent.get("name").and_then(Value::as_str) != Some(state.info.agent_name.as_str())
        || agent.get("agent").and_then(Value::as_str) != state.expected_agent.as_deref()
    {
        bail!("External subagent identity changed; refusing to control a replacement agent");
    }
    Ok(())
}
async fn owned_agent(client: &HerdrClient, state: &ExternalRecord) -> Result<Value> {
    let result = client
        .request(
            "agent.get",
            json!({"target":state.info.agent_name}),
            API_BUDGET,
        )
        .await?;
    let agent = result
        .get("agent")
        .context("Herdr agent.get returned no agent")?
        .clone();
    verify_agent(state, &agent)?;
    Ok(agent)
}
async fn external_subscription(client: &HerdrClient, pane_id: &str) -> Result<HerdrSubscription> {
    client
        .subscribe(
            vec![
                json!({"type":"pane.agent_status_changed","pane_id":pane_id}),
                json!({"type":"pane.closed"}),
                json!({"type":"pane.exited"}),
            ],
            API_BUDGET,
        )
        .await
}
async fn write_prompt(state: &ExternalRecord, prompt: &str) -> Result<()> {
    let dir = state
        .dir
        .join("turns")
        .join(format!("{:03}", state.info.turn));
    tokio::fs::create_dir_all(&dir).await?;
    let answer = state
        .info
        .answer_path
        .as_deref()
        .context("External answer path missing")?;
    tokio::fs::write(dir.join("prompt.md"),format!("Write your final answer as Markdown to this absolute path before finishing: {answer}\n\n{prompt}")).await?;
    Ok(())
}
