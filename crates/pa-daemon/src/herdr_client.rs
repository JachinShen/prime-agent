//! Request and subscription transport for external subagents.
//!
//! RFC-0001: Herdr owns terminal execution; this private client validates the
//! existing JSON-lines API without invoking a shell or inheriting daemon context.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use pa_types::platform::transport::{connect_transport, AsyncReadHalf, AsyncWriteHalf};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Limit one response before allocating an unbounded frame.
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// An API refusal, retained as a typed cause even when callers add context.
#[derive(Debug, thiserror::Error, Deserialize, PartialEq, Eq)]
#[error("Herdr {code}: {message}")]
pub(crate) struct HerdrApiError {
    pub(crate) code: String,
    pub(crate) message: String,
}

/// One explicit session's Herdr endpoint; never resolves process environment.
#[derive(Clone, Debug)]
pub(crate) struct HerdrClient {
    socket_path: String,
    pane_id: Option<String>,
    workspace_id: Option<String>,
}

impl HerdrClient {
    /// Construct a client for an explicit endpoint, without a caller pane.
    pub(crate) fn new(socket_path: String) -> Self {
        Self {
            socket_path,
            pane_id: None,
            workspace_id: None,
        }
    }

    /// Resolve the owning client's context, failing on incomplete bindings.
    pub(crate) fn from_env(env: &BTreeMap<String, String>) -> Result<Self> {
        if env.get("HERDR_ENV").map(String::as_str) != Some("1") {
            bail!("external subagents require a session inside Herdr (HERDR_ENV=1)");
        }
        let required = |key: &str| {
            env.get(key)
                .filter(|value| !value.trim().is_empty())
                .cloned()
                .with_context(|| format!("Herdr session context is missing {key}"))
        };
        let mut client = Self::new(required("HERDR_SOCKET_PATH")?);
        client.pane_id = Some(required("HERDR_PANE_ID")?);
        client.workspace_id = Some(required("HERDR_WORKSPACE_ID")?);
        Ok(client)
    }

    /// The pane supplied by this session's client, when available.
    pub(crate) fn pane_id(&self) -> Option<&str> {
        self.pane_id.as_deref()
    }

    /// The workspace supplied by this session's client, when available.
    pub(crate) fn workspace_id(&self) -> Option<&str> {
        self.workspace_id.as_deref()
    }

    /// Make one request; the budget covers connect, write and the full response.
    pub(crate) async fn request(
        &self,
        method: &str,
        params: Value,
        budget: Duration,
    ) -> Result<Value> {
        tokio::time::timeout(budget, async {
            let (mut connection, id) = self.open_request(method, params).await?;
            let response = connection.read().await?;
            response_result(&response, &id)
        })
        .await
        .with_context(|| format!("Herdr {method} timed out after {budget:?}"))?
        .with_context(|| format!("Herdr request {method}"))
    }

    /// Subscribe to existing API events; retain the acknowledged connection.
    pub(crate) async fn subscribe(
        &self,
        subscriptions: Vec<Value>,
        budget: Duration,
    ) -> Result<HerdrSubscription> {
        tokio::time::timeout(budget, async {
            let (mut connection, id) = self
                .open_request(
                    "events.subscribe",
                    json!({ "subscriptions": subscriptions }),
                )
                .await?;
            let response = connection.read().await?;
            let result = response_result(&response, &id)?;
            if result.get("type").and_then(Value::as_str) != Some("subscription_started") {
                bail!("Herdr events.subscribe returned no subscription acknowledgement");
            }
            Ok(HerdrSubscription { connection })
        })
        .await
        .with_context(|| format!("Herdr subscription timed out after {budget:?}"))?
    }

    async fn open_request(&self, method: &str, params: Value) -> Result<(Connection, String)> {
        if method.trim().is_empty() || !params.is_object() {
            bail!("Herdr request requires a method and object params");
        }
        if self.socket_path.trim().is_empty() {
            bail!("Herdr socket path is empty");
        }
        let target = super::herdr::herdr_socket_target(&self.socket_path, cfg!(windows));
        let transport = connect_transport(Path::new(&target))
            .await
            .context("connect to the session's Herdr socket")?;
        let (reader, mut writer) = transport.split();
        let id = format!("prime-subagent:{}", uuid::Uuid::new_v4());
        let mut frame = serde_json::to_vec(&json!({
            "id": id,
            "method": method,
            "params": params,
        }))?;
        if frame.len() >= MAX_FRAME_BYTES {
            bail!("Herdr request exceeds the {MAX_FRAME_BYTES}-byte frame limit");
        }
        frame.push(b'\n');
        writer
            .write_all(&frame)
            .await
            .context("write Herdr request")?;
        writer.flush().await.context("flush Herdr request")?;
        Ok((
            Connection {
                reader: BufReader::new(reader),
                _writer: writer,
                pending_frame: Vec::new(),
            },
            id,
        ))
    }
}

/// An acknowledged event connection. A timed-out read preserves partial data.
pub(crate) struct HerdrSubscription {
    connection: Connection,
}

impl HerdrSubscription {
    /// Read a validated event envelope, preserving its backend-owned data.
    pub(crate) async fn next(&mut self, budget: Duration) -> Result<Value> {
        let event = tokio::time::timeout(budget, self.connection.read())
            .await
            .with_context(|| format!("Herdr event read timed out after {budget:?}"))??;
        if event
            .get("event")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
            || !event.get("data").is_some_and(Value::is_object)
        {
            bail!("Herdr subscription returned an invalid event envelope");
        }
        Ok(event)
    }
}

struct Connection {
    reader: BufReader<Box<dyn AsyncReadHalf>>,
    // Keep the write half alive for subscriptions; dropping it shuts down writes.
    _writer: Box<dyn AsyncWriteHalf>,
    pending_frame: Vec<u8>,
}

impl Connection {
    async fn read(&mut self) -> Result<Value> {
        loop {
            let available = self
                .reader
                .fill_buf()
                .await
                .context("read Herdr response")?;
            if available.is_empty() {
                bail!("Herdr socket closed before a complete JSON-lines frame");
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let count = newline.map_or(available.len(), |index| index + 1);
            if self.pending_frame.len() + count > MAX_FRAME_BYTES {
                bail!("Herdr response exceeds the {MAX_FRAME_BYTES}-byte frame limit");
            }
            self.pending_frame.extend_from_slice(&available[..count]);
            self.reader.consume(count);
            if newline.is_some() {
                let frame = std::mem::take(&mut self.pending_frame);
                let value: Value =
                    serde_json::from_slice(&frame).context("Herdr returned invalid JSON")?;
                if !value.is_object() {
                    bail!("Herdr returned a non-object JSON frame");
                }
                return Ok(value);
            }
        }
    }
}

fn response_result(response: &Value, expected_id: &str) -> Result<Value> {
    if response.get("id").and_then(Value::as_str) != Some(expected_id) {
        bail!("Herdr response id does not match request {expected_id}");
    }
    match (response.get("result"), response.get("error")) {
        (Some(result), None) if result.is_object() => Ok(result.clone()),
        (None, Some(error)) => {
            let error: HerdrApiError =
                serde_json::from_value(error.clone()).context("invalid Herdr API error")?;
            Err(error.into())
        }
        (Some(_) | None, None) | (Some(_), Some(_)) => {
            bail!("Herdr response must contain exactly one object result or error");
        }
    }
}

#[cfg(all(test, unix))]
mod tests;
