//! Bridge pa-core tool definitions into the pa-agent loop's `AgentTool`
//! (TS `createAgentSession` passes `ToolDefinition`s to the loop
//! unchanged; this adapter is the Rust equivalent).

use std::sync::Arc;

use pa_agent::abort::AbortSignal;
use pa_agent::types::{
    AgentTool, AgentToolResult, AgentToolUpdateCallback,
    ToolExecutionMode as LoopToolExecutionMode, ToolResultContent,
};

use crate::tools::tool_definition::{ExecutionMode, ToolDefinition, ToolExecutionResult};

/// A loop-visible tool backed by a pa-core definition.
pub struct ToolDefinitionBridge {
    definition: ToolDefinition,
}

impl ToolDefinitionBridge {
    #[must_use]
    pub fn new(definition: ToolDefinition) -> Self {
        Self { definition }
    }
}

fn convert_content(result: ToolExecutionResult) -> Vec<ToolResultContent> {
    convert_content_blocks(result.content)
}

fn convert_content_blocks(
    blocks: Vec<crate::tools::tool_definition::ToolContentBlock>,
) -> Vec<ToolResultContent> {
    blocks
        .into_iter()
        .map(|block| match block {
            crate::tools::tool_definition::ToolContentBlock::Text { text } => {
                ToolResultContent::text(text)
            }
            crate::tools::tool_definition::ToolContentBlock::Image { data, mime_type } => {
                ToolResultContent::Image(pa_agent::types::ImageContent { data, mime_type })
            }
        })
        .collect()
}

impl AgentTool for ToolDefinitionBridge {
    fn name(&self) -> &str {
        &self.definition.name
    }

    fn description(&self) -> &str {
        &self.definition.description
    }

    fn parameters(&self) -> &serde_json::Value {
        &self.definition.parameters
    }

    fn label(&self) -> &str {
        &self.definition.label
    }

    fn prepare_arguments(&self, args: &serde_json::Value) -> Option<serde_json::Value> {
        self.definition
            .prepare_arguments
            .as_ref()
            .map(|prepare| prepare(args.clone()))
    }

    fn accepts_alias(&self, _name: &str) -> bool {
        false
    }

    fn execute(
        self: Arc<Self>,
        tool_call_id: String,
        params: serde_json::Value,
        signal: AbortSignal,
        on_update: AgentToolUpdateCallback,
    ) -> pa_agent::BoxFut<'static, anyhow::Result<AgentToolResult>> {
        let definition = self.definition.clone();
        Box::pin(async move {
            let abort = crate::tools::tool_definition::AbortSignal::new();
            {
                // Wire the loop's signal into the tool's cancellation token.
                let abort = abort.clone();
                let signal = signal.clone();
                tokio::spawn(async move {
                    // Park on the signal's watch channel: an abort fires the token
                    // immediately, and an uninterrupted call leaves no wake-up work behind —
                    // a completed tool call must not keep a 10 ms polling task alive.
                    signal.aborted().await;
                    abort.cancel();
                });
            }
            // Streamed tool updates become `tool_execution_update` events — the same
            // in-flight previews TS forwards; the loop's callback owns the gating.
            let tool_on_update: Option<crate::tools::tool_definition::OnUpdate> =
                Some(Arc::new(move |update| {
                    on_update(AgentToolResult {
                        content: convert_content_blocks(update.content),
                        details: update.details.unwrap_or(serde_json::Value::Null),
                        terminate: None,
                    });
                }));
            let f = definition.execute.clone();
            let result: ToolExecutionResult =
                f(&tool_call_id, params, Some(abort), tool_on_update).await?;
            let details = result.details.clone().unwrap_or(serde_json::Value::Null);
            let is_error = result.is_error;
            let _ = is_error;
            Ok(AgentToolResult {
                content: convert_content(result),
                details,
                terminate: None,
            })
        })
    }

    fn execution_mode(&self) -> Option<LoopToolExecutionMode> {
        // The pa-core contract has no parallel variant; sequential tools
        // carry the only override.
        (self.definition.execution_mode == Some(ExecutionMode::Sequential))
            .then_some(LoopToolExecutionMode::Sequential)
    }
}

/// Register a native tool behind the Nix host bridge, without exposing its schema.
pub(crate) fn register_host_tool(
    handlers: &mut crate::kernel::shared::HostRequestHandlers,
    definition: ToolDefinition,
) {
    let request_type = format!("tool.{}", definition.name);
    handlers.register(
        request_type,
        crate::kernel::shared::host_handler(move |payload| {
            let definition = definition.clone();
            async move {
                let mut params = payload.data.get("kwargs").cloned().unwrap_or(payload.data);
                if let Some(object) = params.as_object_mut() {
                    object.remove("type");
                }
                if let Some(prepare) = definition.prepare_arguments {
                    params = prepare(params);
                }
                let result = (definition.execute)(&definition.name, params, None, None).await?;
                if definition.name == "run" {
                    return Ok(result.details.unwrap_or_default());
                }
                Ok(serde_json::json!({
                    "content": result.content,
                    "details": result.details,
                    "isError": result.is_error,
                }))
            }
        }),
    );
}

/// Wrap a definition in the loop's tool trait.
#[must_use]
pub fn bridge_tool(definition: ToolDefinition) -> Arc<dyn AgentTool> {
    Arc::new(ToolDefinitionBridge::new(definition))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn host_tools_use_native_bash_and_edit_in_the_session_directory() {
        use crate::kernel::shared::{HostRequestHandlers, HostRequestPayload};

        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy();
        let mut handlers = HostRequestHandlers::new();
        register_host_tool(
            &mut handlers,
            crate::create_bash_tool_definition_with_options(
                &cwd,
                &crate::BashToolOptions {
                    command_prefix: Some("export BRIDGE_VALUE=before".to_string()),
                    ..Default::default()
                },
            ),
        );
        register_host_tool(&mut handlers, crate::create_edit_tool_definition(&cwd));
        let bash = handlers.get("tool.bash").unwrap();
        let output = bash(HostRequestPayload {
            data: serde_json::json!({
                "type": "tool.bash",
                "command": "printf '%s' \"$BRIDGE_VALUE\" > example.txt; cat example.txt",
            }),
        })
        .await
        .unwrap();
        assert_eq!(
            output,
            serde_json::json!({
                "content": [{"type": "text", "text": "before"}],
                "details": null,
                "isError": false,
            })
        );
        let edit = handlers.get("tool.edit").unwrap();
        let output = edit(HostRequestPayload {
            data: serde_json::json!({
                "type": "tool.edit",
                "path": "example.txt",
                "edits": [{"oldText": "before", "newText": "after"}],
            }),
        })
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("example.txt")).unwrap(),
            "after"
        );
        assert_eq!(output["isError"], false);
        assert!(!output["details"]["diff"].as_str().unwrap().is_empty());
        let error = bash(HostRequestPayload {
            data: serde_json::json!({"type": "tool.bash", "command": "exit 7"}),
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("7"));
        let error = edit(HostRequestPayload {
            data: serde_json::json!({
                "type": "tool.edit",
                "path": "example.txt",
                "edits": [{"oldText": "missing", "newText": "wrong"}],
            }),
        })
        .await
        .unwrap_err();
        assert!(!error.to_string().is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("example.txt")).unwrap(),
            "after"
        );
    }

    #[tokio::test]
    async fn nix_run_captures_streams_and_nonzero_status() {
        use crate::kernel::shared::{HostRequestHandlers, HostRequestPayload};
        let dir = tempfile::tempdir().unwrap();
        let mut handlers = HostRequestHandlers::new();
        register_host_tool(
            &mut handlers,
            crate::tools::bash::create_run_tool_definition(
                &dir.path().to_string_lossy(),
                &crate::BashToolOptions::default(),
            ),
        );
        let output = handlers.get("tool.run").unwrap()(HostRequestPayload {
            data: serde_json::json!({"command":"printf hello; printf problem >&2; exit 7"}),
        })
        .await
        .unwrap();
        assert_eq!(
            output,
            serde_json::json!({"stdout":"hello","stderr":"problem","exitCode":7,"truncated":false})
        );
    }

    #[tokio::test]
    #[ignore = "requires built native CLI in PRIME_AGENT_NIX_RUNTIME and nix on PATH"]
    async fn native_nix_run_and_edit_round_trip() {
        use crate::kernel::manager::KernelStartOptions;
        use crate::kernel::shared::{
            ExecuteOptions, ExecuteStatus, HostRequestHandlers, KernelManagerOptions,
            KernelShutdownOptions,
        };
        std::env::var_os("PRIME_AGENT_NIX_RUNTIME").expect("set native CLI executable");
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy();
        let mut handlers = HostRequestHandlers::new();
        register_host_tool(
            &mut handlers,
            crate::tools::bash::create_run_tool_definition(
                &cwd,
                &crate::BashToolOptions::default(),
            ),
        );
        register_host_tool(&mut handlers, crate::create_edit_tool_definition(&cwd));
        let mut manager = crate::kernel::ReplKernelManager::new(KernelManagerOptions {
            cwd: Some(dir.path().to_path_buf()),
            host_handlers: handlers.clone(),
            ..Default::default()
        });
        manager.start(KernelStartOptions::default()).await.unwrap();
        let run = manager
            .execute(
                r#":run result "printf before > sample; printf out; printf err >&2; exit 7""#,
                ExecuteOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(run.status, ExecuteStatus::Error, "{run:?}");
        assert!(
            run.error
                .as_ref()
                .is_some_and(|error| error.evalue.contains('7')),
            "{run:?}"
        );
        let check = manager
            .execute(":p (builtins.toJSON result)", ExecuteOptions::default())
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(check.result.as_deref().unwrap()).unwrap(),
            serde_json::json!({"stdout":"out", "stderr":"err", "exitCode":7, "truncated":false})
        );
        let edit = manager
            .execute(
                r#":edit edited { path = "sample"; old = "before"; new = "after"; }"#,
                ExecuteOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(edit.status, ExecuteStatus::Ok, "{edit:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("sample")).unwrap(),
            "after"
        );
        let signalled = manager
            .execute(
                r#":run signalled "kill -TERM $$""#,
                ExecuteOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(signalled.status, ExecuteStatus::Error, "{signalled:?}");
        assert!(
            signalled
                .error
                .as_ref()
                .is_some_and(|error| error.evalue.contains("signal")),
            "{signalled:?}"
        );
        let check = manager
            .execute("signalled.exitCode", ExecuteOptions::default())
            .await
            .unwrap();
        assert_eq!(check.result.as_deref(), Some("null"));
        for shutdown in [false, true] {
            let signal = crate::kernel::AbortSignal::new();
            let running = manager.clone();
            let cell_signal = signal.clone();
            let task = tokio::spawn(async move {
                running
                    .execute(
                        r#":run pending "printf '%s' $$ > pid; exec sleep 300""#,
                        ExecuteOptions {
                            signal: Some(cell_signal),
                            ..Default::default()
                        },
                    )
                    .await
            });
            let pid = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if let Ok(text) = std::fs::read_to_string(dir.path().join("pid")) {
                        if let Ok(pid) = text.parse::<i32>() {
                            break pid;
                        }
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("host command ready");
            if shutdown {
                manager
                    .shutdown(KernelShutdownOptions::default())
                    .await
                    .unwrap();
            } else {
                signal.abort();
            }
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap();
            #[cfg(unix)]
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("host child killed and reaped");
            std::fs::remove_file(dir.path().join("pid")).unwrap();
            if shutdown {
                manager = crate::kernel::ReplKernelManager::new(KernelManagerOptions {
                    cwd: Some(dir.path().to_path_buf()),
                    host_handlers: handlers.clone(),
                    ..Default::default()
                });
                manager.start(KernelStartOptions::default()).await.unwrap();
            }
            let healthy = manager
                .execute("6 * 7", ExecuteOptions::default())
                .await
                .unwrap();
            assert_eq!(healthy.result.as_deref(), Some("42"), "{healthy:?}");
        }
        manager
            .shutdown(KernelShutdownOptions::default())
            .await
            .unwrap();
    }

    fn shout_args(mut args: serde_json::Value) -> serde_json::Value {
        if let Some(text) = args.get("text").and_then(|t| t.as_str()) {
            args["text"] = serde_json::json!(text.to_uppercase());
        }
        args
    }

    fn echo_definition(is_error: bool) -> ToolDefinition {
        let is_error = std::sync::Arc::new(is_error);
        ToolDefinition {
            name: "echo".to_string(),
            label: "Echo".to_string(),
            description: "Echoes its input".to_string(),
            prompt_snippet: String::new(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }),
            execution_mode: Some(ExecutionMode::Sequential),
            prepare_arguments: Some(shout_args),
            execute: Arc::new(move |_id, params, _signal, _on_update| {
                let text = params
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_string();
                let is_error = *is_error;
                Box::pin(async move {
                    if is_error {
                        anyhow::bail!("echo failed");
                    }
                    Ok(ToolExecutionResult::text(format!("echo: {text}")))
                })
            }),
        }
    }

    #[tokio::test]
    async fn bridges_definition_into_the_loop_contract() {
        let tool = bridge_tool(echo_definition(false));
        assert_eq!(tool.name(), "echo");
        assert_eq!(tool.description(), "Echoes its input");
        assert_eq!(tool.label(), "Echo");
        assert!(tool.parameters().get("properties").is_some());
        assert_eq!(
            tool.execution_mode(),
            Some(LoopToolExecutionMode::Sequential)
        );
        let prepared = tool.prepare_arguments(&serde_json::json!({ "text": "hi" }));
        assert_eq!(prepared.unwrap()["text"], "HI");
        let result = tool
            .clone()
            .execute(
                "call-1".to_string(),
                serde_json::json!({ "text": "hello" }),
                AbortSignal::default(),
                Arc::new(|_result| {}),
            )
            .await
            .unwrap();
        assert_eq!(result.content.len(), 1);
    }

    /// The interactive loader's kernel-boot note and live cell output
    /// ride this channel.
    #[tokio::test]
    async fn streamed_updates_reach_the_loop_callback() {
        use std::sync::Mutex;
        let definition = ToolDefinition {
            name: "progress".to_string(),
            label: "Progress".to_string(),
            description: "Reports progress".to_string(),
            prompt_snippet: String::new(),
            parameters: serde_json::json!({ "type": "object" }),
            execution_mode: None,
            prepare_arguments: None,
            execute: Arc::new(|_id, _params, _signal, on_update| {
                Box::pin(async move {
                    if let Some(on_update) = on_update {
                        on_update(crate::tools::tool_definition::ToolUpdate {
                            content: vec![crate::tools::tool_definition::ToolContentBlock::text(
                                "starting",
                            )],
                            details: Some(serde_json::json!({ "status": "starting" })),
                        });
                    }
                    Ok(ToolExecutionResult::text("done"))
                })
            }),
        };
        let tool = bridge_tool(definition);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        tool.execute(
            "call-3".to_string(),
            serde_json::json!({}),
            AbortSignal::default(),
            Arc::new(move |result| sink.lock().unwrap().push(result)),
        )
        .await
        .unwrap();
        let updates = seen.lock().unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(
            updates[0].details,
            serde_json::json!({ "status": "starting" })
        );
        assert_eq!(
            updates[0].content,
            vec![ToolResultContent::text("starting")]
        );
    }

    #[test]
    fn nix_tool_does_not_execute_legacy_python_calls() {
        let mut definition = echo_definition(false);
        definition.name = pa_types::ai::PYTHON_TOOL_NAME.to_string();
        let tool = bridge_tool(definition);
        for name in ["python_repl", "python", "ipython"] {
            assert!(!tool.accepts_alias(name));
        }
        assert_eq!(tool.name(), "nix_repl");
    }

    #[tokio::test]
    async fn errors_surface_as_err_like_a_throw() {
        let tool = bridge_tool(echo_definition(true));
        let error = tool
            .clone()
            .execute(
                "call-2".to_string(),
                serde_json::json!({ "text": "x" }),
                AbortSignal::default(),
                Arc::new(|_result| {}),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("echo failed"));
    }
}
