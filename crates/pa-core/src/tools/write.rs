//! Create new UTF-8 files for the native Nix runtime without overwriting existing paths.

use std::{io::Write, path::Path, sync::Arc};

use anyhow::Context;
use serde::Deserialize;
use serde_json::json;

use super::tool_definition::{AbortSignal, ToolContentBlock, ToolDefinition, ToolExecutionResult};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteInput {
    path: String,
    text: String,
}

#[tracing::instrument(level = "debug", skip_all)]
async fn execute_write(
    cwd: &str,
    input: serde_json::Value,
    signal: Option<AbortSignal>,
) -> anyhow::Result<ToolExecutionResult> {
    let input: WriteInput = serde_json::from_value(input).context("Invalid write arguments")?;
    anyhow::ensure!(!input.path.is_empty(), "Write path must not be empty");
    let path = Path::new(cwd).join(&input.path);
    tokio::task::spawn_blocking(move || {
        anyhow::ensure!(
            !signal.as_ref().is_some_and(AbortSignal::is_cancelled),
            "Operation aborted"
        );
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("Could not create {}", path.display()))?;
        file.write_all(input.text.as_bytes())
            .with_context(|| format!("Could not write {}", path.display()))?;
        let bytes = input.text.len();
        let path = path.to_string_lossy();
        Ok(ToolExecutionResult {
            content: vec![ToolContentBlock::text(format!(
                "Created {path} ({bytes} bytes)"
            ))],
            details: Some(json!({"path":path,"bytes":bytes})),
            is_error: false,
        })
    })
    .await?
}

pub(crate) fn create_write_tool_definition(cwd: &str) -> ToolDefinition {
    let cwd = cwd.to_owned();
    ToolDefinition {
        name: "write".into(),
        label: "write".into(),
        description: "Create a new UTF-8 file. Existing paths, including symlinks, are rejected; parent directories must already exist.".into(),
        prompt_snippet: "Create a new file without overwriting existing paths".into(),
        parameters: json!({
            "type":"object",
            "required":["path","text"],
            "properties":{
                "path":{"type":"string","minLength":1},
                "text":{"type":"string"}
            },
            "additionalProperties":false
        }),
        execution_mode: None,
        prepare_arguments: None,
        execute: Arc::new(move |_id, params, signal, _update| {
            let cwd = cwd.clone();
            Box::pin(async move { execute_write(&cwd, params, signal).await })
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn creates_utf8_and_refuses_overwrite_or_missing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let tool = create_write_tool_definition(cwd);
        let result = (tool.execute)(
            "test",
            json!({"path":"new.txt","text":"中文🌸\n"}),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            result.details,
            Some(json!({"path":dir.path().join("new.txt"),"bytes":11}))
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("new.txt")).unwrap(),
            "中文🌸\n"
        );
        for args in [
            json!({"path":"new.txt","text":"replacement"}),
            json!({"path":"missing/new.txt","text":"no parent"}),
            json!({"path":"other.txt","text":"no override","overwrite":true}),
        ] {
            assert!((tool.execute)("test", args, None, None).await.is_err());
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("new.txt")).unwrap(),
            "中文🌸\n"
        );
        assert!(!dir.path().join("missing").exists());
        assert!(!dir.path().join("other.txt").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_existing_and_dangling_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        std::fs::write(dir.path().join("target"), "preserved").unwrap();
        for (link, target) in [("live", "target"), ("dangling", "absent")] {
            std::os::unix::fs::symlink(target, dir.path().join(link)).unwrap();
            assert!(
                execute_write(cwd, json!({"path":link,"text":"replacement"}), None)
                    .await
                    .is_err()
            );
            assert_eq!(
                std::fs::read_link(dir.path().join(link)).unwrap(),
                Path::new(target)
            );
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("target")).unwrap(),
            "preserved"
        );
        assert!(!dir.path().join("absent").exists());
    }
}
