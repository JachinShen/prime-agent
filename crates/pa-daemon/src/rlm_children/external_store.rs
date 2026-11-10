//! Persistent external-child state and per-turn answer-file reads.
//!
//! RFC-0001: 本轮答复文件是结果源；读写失败保留具体原因，不降级为终端文本。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use pa_types::daemon::external_subagent::ExternalSubagentInfo;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const STORE_VERSION: u32 = 1;
const MANIFEST_NAME: &str = "external.json";

/// Durable state of one logical external child and its current turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExternalRecord {
    pub(crate) child_id: String,
    pub(crate) name: String,
    pub(crate) dir: PathBuf,
    pub(crate) socket_path: String,
    pub(crate) info: ExternalSubagentInfo,
    pub(crate) expected_agent: Option<String>,
    pub(crate) status: String,
    pub(crate) settled: bool,
    pub(crate) text: String,
    pub(crate) error: Option<String>,
    pub(crate) baseline_state_seq: u64,
    pub(crate) baseline_completion_seq: Option<u64>,
    pub(crate) started_at_ms: u64,
    pub(crate) notice_sent: bool,
    pub(crate) closed: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    record: ExternalRecord,
}

impl ExternalRecord {
    /// Atomically replace this child's versioned manifest.
    pub(crate) async fn persist(&self) -> Result<()> {
        validate_record(self)?;
        let manifest_path = self.dir.join(MANIFEST_NAME);
        let bytes = serde_json::to_vec(&Manifest {
            version: STORE_VERSION,
            record: self.clone(),
        })
        .context("serialize external subagent manifest")?;
        tokio::fs::create_dir_all(&self.dir)
            .await
            .with_context(|| format!("create external child directory {}", self.dir.display()))?;
        let temporary = self
            .dir
            .join(format!(".external-{}.tmp", uuid::Uuid::new_v4()));
        let write = async {
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .await
                .context("create external manifest staging file")?;
            file.write_all(&bytes)
                .await
                .context("write external manifest")?;
            file.flush().await.context("flush external manifest")?;
            file.sync_all().await.context("sync external manifest")?;
            drop(file);
            tokio::fs::rename(&temporary, &manifest_path)
                .await
                .with_context(|| format!("publish external manifest {}", manifest_path.display()))
        }
        .await;
        if let Err(error) = write {
            return match tokio::fs::remove_file(&temporary).await {
                Ok(()) => Err(error),
                Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => Err(error),
                Err(cleanup) => Err(error.context(format!(
                    "also failed to remove staging file {}: {cleanup}",
                    temporary.display()
                ))),
            };
        }
        Ok(())
    }
}

/// Read one explicit manifest path; malformed state never becomes an empty registry.
pub(crate) async fn load(path: &Path) -> Result<ExternalRecord> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("read external manifest {}", path.display()))?;
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("decode external manifest {}", path.display()))?;
    if manifest.version != STORE_VERSION {
        bail!(
            "external manifest {} has unsupported version {}",
            path.display(),
            manifest.version
        );
    }
    validate_record(&manifest.record)?;
    if path.parent() != Some(manifest.record.dir.as_path()) {
        bail!(
            "external manifest {} does not belong to recorded directory {}",
            path.display(),
            manifest.record.dir.display()
        );
    }
    Ok(manifest.record)
}

fn validate_record(record: &ExternalRecord) -> Result<()> {
    let identities = [
        record.child_id.as_str(),
        record.name.as_str(),
        record.socket_path.as_str(),
        record.info.agent.as_str(),
        record.info.workspace_id.as_str(),
        record.info.pane_id.as_str(),
        record.info.terminal_id.as_str(),
        record.info.agent_name.as_str(),
        record.info.state.as_str(),
    ];
    if identities.iter().any(|identity| identity.trim().is_empty())
        || record
            .expected_agent
            .as_ref()
            .is_some_and(|agent| agent.trim().is_empty())
    {
        bail!("external manifest requires nonempty child, runtime, socket and state identities");
    }
    if !record.dir.is_absolute() {
        bail!("external manifest requires an absolute child directory");
    }
    if let Some(worktree) = &record.info.worktree {
        if !Path::new(&worktree.path).is_absolute()
            || !Path::new(&worktree.repo_root).is_absolute()
            || worktree.branch.trim().is_empty()
            || [&worktree.path, &worktree.branch, &worktree.repo_root]
                .iter()
                .any(|value| value.contains('\0'))
        {
            bail!("external manifest has invalid worktree metadata");
        }
    }
    if record.info.turn == 0 {
        bail!("external manifest turn must be positive");
    }
    let answer = record
        .dir
        .join("turns")
        .join(format!("{:03}", record.info.turn))
        .join("answer.md");
    if record.info.answer_path.as_deref().map(Path::new) != Some(answer.as_path()) {
        bail!(
            "external manifest answer path must match its current turn: {}",
            answer.display()
        );
    }
    if !matches!(
        record.status.as_str(),
        "running" | "completed" | "error" | "cancelled"
    ) {
        bail!("external manifest has invalid status {}", record.status);
    }
    if record.settled && record.status == "running" {
        bail!("external manifest cannot settle a running turn");
    }
    Ok(())
}

/// Read a regular UTF-8 answer, bounding the returned prefix by Unicode characters.
///
/// The entire file is validated without retaining its full contents in memory.
/// Missing, whitespace-only, invalid UTF-8 and non-regular files are errors.
pub(crate) async fn read_answer(path: &Path, max_chars: usize) -> Result<(String, bool)> {
    if max_chars == 0 {
        bail!("answer character limit must be positive");
    }
    let before = tokio::fs::symlink_metadata(path)
        .await
        .with_context(|| format!("inspect answer file {}", path.display()))?;
    if !before.is_file() {
        bail!("answer path {} is not a regular file", path.display());
    }
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("open answer file {}", path.display()))?;
    let opened = file
        .metadata()
        .await
        .with_context(|| format!("inspect opened answer file {}", path.display()))?;
    if !opened.is_file() {
        bail!(
            "opened answer path {} is not a regular file",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if (before.dev(), before.ino()) != (opened.dev(), opened.ino()) {
            bail!(
                "answer file {} changed identity while opening",
                path.display()
            );
        }
    }
    let mut prefix = String::new();
    let mut count = 0_usize;
    let mut nonblank = false;
    let mut pending = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let length = file
            .read(&mut chunk)
            .await
            .with_context(|| format!("read answer file {}", path.display()))?;
        if length == 0 {
            if !pending.is_empty() {
                bail!("answer file {} contains incomplete UTF-8", path.display());
            }
            break;
        }
        pending.extend_from_slice(&chunk[..length]);
        let valid_length = match std::str::from_utf8(&pending) {
            Ok(text) => text.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("answer file {} is not UTF-8", path.display()));
            }
        };
        let text = std::str::from_utf8(&pending[..valid_length])
            .context("decode validated answer-file segment")?;
        for character in text.chars() {
            nonblank |= !character.is_whitespace();
            if count < max_chars {
                prefix.push(character);
            }
            count = count.saturating_add(1);
        }
        pending.drain(..valid_length);
    }
    if !nonblank {
        bail!("answer file {} is empty or whitespace-only", path.display());
    }
    Ok((prefix, count > max_chars))
}

#[cfg(test)]
mod tests;
