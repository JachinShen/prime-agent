//! Idempotent admission of authenticated Herdr child notices (RFC-0001).
//!
//! The queue journal closes the sender's acknowledgement gap. Transcript evidence
//! retires stale queue snapshots, while the memory set covers the in-flight handoff.

use std::collections::{HashSet, VecDeque};
use std::io::{BufRead, BufReader};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use super::{
    enqueue_priority, json, parse_prompt_images, queue_lanes, response_success, Lane,
    QueuePriority, QueuedItem, SessionFile, TurnPolicy, Worker,
};
use crate::protocol::DaemonResponse;

/// Called only after a reserved custom row's one-shot capability was consumed.
pub(super) fn external_notice_id(row: &Value) -> Result<Option<&str>> {
    if !crate::child_status_notices::is_reserved_child_status_custom_type(row)
        || row.pointer("/details/backend").and_then(Value::as_str) != Some("herdr")
    {
        return Ok(None);
    }
    let id = row
        .pointer("/details/externalNoticeId")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty() && id.len() <= 512 && !id.contains('\0'))
        .context(
            "external child notice requires a non-empty externalNoticeId of at most 512 bytes",
        )?;
    Ok(Some(id))
}

fn delivered_notice_ids(store: &SessionFile) -> Result<HashSet<String>> {
    if store.path.as_os_str().is_empty() {
        bail!("external child notices require a durable session transcript");
    }
    // A windowed store may omit old notices. Read the authoritative file rather
    // than interpreting a cache miss as proof that a notice never arrived.
    let file = std::fs::File::open(&store.path).with_context(|| {
        format!(
            "read external notice evidence from {}",
            store.path.display()
        )
    })?;
    let mut ids = HashSet::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line =
            line.with_context(|| format!("read external notice evidence line {}", index + 1))?;
        let entry: Value = serde_json::from_str(&line).with_context(|| {
            format!(
                "invalid external notice transcript evidence at line {}",
                index + 1
            )
        })?;
        let row = match entry.get("type").and_then(Value::as_str) {
            Some("custom_message") => Some(&entry),
            Some("message") => entry.get("message"),
            _ => None,
        };
        if let Some(row) = row {
            if let Some(id) = external_notice_id(row)? {
                ids.insert(id.to_string());
            }
        }
    }
    Ok(ids)
}

fn matches_notice_id(row: &Value, id: &str) -> bool {
    crate::child_status_notices::is_reserved_child_status_custom_type(row)
        && row.pointer("/details/backend").and_then(Value::as_str) == Some("herdr")
        && row
            .pointer("/details/externalNoticeId")
            .and_then(Value::as_str)
            == Some(id)
}

impl Worker {
    pub(super) fn admit_external_notice(
        &self,
        payload: &Value,
        lane: Lane,
        row: Value,
        notice_id: &str,
    ) -> Result<DaemonResponse> {
        // Same lock order as recovery checkpoints. The core remains locked until
        // the durable append finishes, so the runner cannot pick up a failed admission.
        let mut seen = self.external_notices.lock().unwrap();
        let mut recovery = self.recovery.lock().unwrap();
        let journal = recovery
            .as_mut()
            .context("external child notices require a recovery journal")?;
        let mut core = self.core.lock().unwrap();
        let store = core
            .store
            .as_ref()
            .context("external child notices require a session transcript")?;
        let key = (store.path.clone(), notice_id.to_string());
        let mut queued = false;
        for item in core.steering.iter().chain(&core.follow_up) {
            if let Some(row) = &item.custom_message {
                queued |= external_notice_id(row)? == Some(notice_id);
            }
        }
        let duplicate =
            seen.contains(&key) || queued || delivered_notice_ids(store)?.contains(notice_id);
        if duplicate {
            seen.insert(key);
            return Ok(response_success(
                None,
                lane.as_str(),
                Some(json!({ "queued": true, "duplicate": true })),
            ));
        }
        let was_suspended = core.queued_input_suspended;
        core.queued_input_suspended = false;
        let item = QueuedItem {
            priority: QueuePriority::Background,
            preview: None,
            message: payload
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            custom_message: Some(row),
            agent_message: None,
            queue_key: None,
            admission_id: None,
            images: parse_prompt_images(payload),
            done: None,
            queue_visible: true,
            policy: TurnPolicy::Queued,
            forced_batch: false,
        };
        match lane {
            Lane::Steering => enqueue_priority(&mut core.steering, item),
            Lane::FollowUp => enqueue_priority(&mut core.follow_up, item),
        }
        let lanes = queue_lanes(&core);
        let store = core.store.as_ref().expect("session checked before enqueue");
        let checkpoint = journal.record_queue_checkpoint(
            &core.active_session_id,
            store.session_id(),
            Some(store.path.to_string_lossy().as_ref()),
            true,
            "external_notice_queued",
            &lanes.steering,
            &lanes.follow_up,
        );
        if let Err(error) = checkpoint {
            let keep = |item: &QueuedItem| {
                !item
                    .custom_message
                    .as_ref()
                    .is_some_and(|row| matches_notice_id(row, notice_id))
            };
            core.steering.retain(keep);
            core.follow_up.retain(keep);
            core.queued_input_suspended = was_suspended;
            return Err(error.context("persist external child notice admission"));
        }
        seen.insert(key);
        let snapshot = Self::snapshot_locked(&core);
        drop(core);
        drop(recovery);
        drop(seen);
        let _ = self.emit_action_update(&snapshot);
        self.resume_queued_input();
        Ok(response_success(
            None,
            lane.as_str(),
            Some(json!({ "queued": true })),
        ))
    }

    pub(super) fn restore_external_notice_dedup(
        &self,
        store: &SessionFile,
        steering: &mut VecDeque<QueuedItem>,
        follow_up: &mut VecDeque<QueuedItem>,
    ) -> Result<()> {
        let mut pending = HashSet::new();
        for item in steering.iter().chain(follow_up.iter()) {
            if let Some(row) = &item.custom_message {
                if let Some(id) = external_notice_id(row)? {
                    pending.insert(id.to_string());
                }
            }
        }
        if pending.is_empty() {
            return Ok(());
        }
        let delivered = delivered_notice_ids(store)?;
        let mut retained = HashSet::new();
        let keep = |item: &QueuedItem, retained: &mut HashSet<String>| {
            let id = item.custom_message.as_ref().and_then(|row| {
                // Every queued external row was validated in the loop above.
                external_notice_id(row)
                    .expect("external notice validated before recovery filtering")
            });
            match id {
                Some(id) if pending.contains(id) => {
                    !delivered.contains(id) && retained.insert(id.to_string())
                }
                Some(_) | None => true,
            }
        };
        steering.retain(|item| keep(item, &mut retained));
        follow_up.retain(|item| keep(item, &mut retained));
        let mut seen = self.external_notices.lock().unwrap();
        for id in retained.into_iter().chain(delivered) {
            seen.insert((store.path.clone(), id));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
