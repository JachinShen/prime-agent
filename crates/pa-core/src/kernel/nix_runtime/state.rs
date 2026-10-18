use super::evaluator::{valid_name, Evaluator};
use anyhow::{bail, Context};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

impl Evaluator {
    pub(super) async fn state(&mut self, request: &Value) -> anyhow::Result<Value> {
        let path = std::path::Path::new(request["path"].as_str().context("Snapshot needs path")?);
        let max_bytes = request["max_bytes"].as_u64().unwrap_or(256 * 1024 * 1024);
        let max_variable = request["max_variable_bytes"]
            .as_u64()
            .unwrap_or(16 * 1024 * 1024);
        if request["type"] == "restore" {
            let metadata = match tokio::fs::metadata(path).await {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(json!({"restored":[],"failed":[],"reason":"snapshot not found"}))
                }
                Err(error) => return Err(error.into()),
            };
            if metadata.len() > max_bytes {
                bail!("Snapshot exceeds max_bytes");
            }
            let payload: Value = serde_json::from_slice(&tokio::fs::read(path).await?)?;
            if payload["format"] != "prime-agent-nix-literals-v1" {
                bail!("Not a Nix literal snapshot");
            }
            let values = payload["values"]
                .as_object()
                .context("Invalid snapshot values")?;
            for (name, value) in values {
                if !valid_name(name)
                    || value.is_array()
                    || value.is_object()
                    || serde_json::to_vec(value)?.len() as u64 > max_variable
                {
                    bail!("Invalid or oversized snapshot literal {name}");
                }
            }
            let mut restored = Vec::new();
            for (name, value) in values {
                self.bind(name, value).await?;
                self.literals.insert(name.clone(), value.clone());
                restored.push(name);
            }
            return Ok(json!({"restored":restored,"failed":[]}));
        }
        let mut values = serde_json::Map::new();
        let mut skipped = Vec::new();
        for name in &self.names {
            if let Some(value) = self.literals.get(name) {
                if serde_json::to_vec(value)?.len() as u64 <= max_variable {
                    values.insert(name.clone(), value.clone());
                    continue;
                }
            }
            skipped.push(json!({"name":name,"reason":"Lazy, non-literal, or oversized value; not forced or replayed"}));
        }
        let payload =
            serde_json::to_vec(&json!({"format":"prime-agent-nix-literals-v1","values":values}))?;
        if payload.len() as u64 > max_bytes {
            bail!("Nix snapshot exceeds max_bytes");
        }
        let names: Vec<_> = values.keys().collect();
        let manifest = json!({"version":1,"savedNames":names,"skipped":skipped,"pruned":[],"bytes":payload.len(),"runtime":"nix"});
        let manifest_path = std::path::Path::new(
            request["manifest_path"]
                .as_str()
                .context("Snapshot needs manifest_path")?,
        );
        for (target, bytes) in [
            (path, payload.clone()),
            (manifest_path, serde_json::to_vec(&manifest)?),
        ] {
            let parent = target.parent().context("Snapshot needs parent directory")?;
            crate::platform::perms::create_dir_all_private(parent)?;
            let temporary = parent.join(format!(".nix-snapshot-{}", uuid::Uuid::new_v4()));
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            crate::platform::perms::set_private_mode(&mut options);
            let file = options.open(&temporary)?;
            let mut file = tokio::fs::File::from_std(file);
            if let Err(error) = file.write_all(&bytes).await {
                let _ = tokio::fs::remove_file(&temporary).await;
                return Err(error.into());
            }
            file.flush().await?;
            drop(file);
            if let Err(error) = tokio::fs::rename(&temporary, target).await {
                let _ = tokio::fs::remove_file(&temporary).await;
                return Err(error.into());
            }
        }
        Ok(json!({"saved":names,"skipped":skipped,"pruned":[],"bytes":payload.len()}))
    }
}
