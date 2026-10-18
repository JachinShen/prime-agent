use anyhow::{bail, Context};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::OwnedFd;
use std::process::Stdio;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};

pub(super) const CAP: usize = 1_048_576;
pub(super) fn string(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t")
            .replace("${", "\\${")
    )
}
pub(super) fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || "_'-".contains(c))
        && ![
            "if", "then", "else", "assert", "with", "let", "in", "rec", "inherit", "or", "true",
            "false", "null", "builtins",
        ]
        .contains(&name)
}
pub(super) struct Evaluator {
    child: Child,
    input: ChildStdin,
    output: tokio::net::UnixStream,
    pub(super) names: BTreeSet<String>,
    pub(super) literals: BTreeMap<String, Value>,
}
impl Evaluator {
    pub(super) async fn start(pid: &Arc<AtomicU32>) -> anyhow::Result<Self> {
        let (reader, writer) = std::os::unix::net::UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        let stderr = writer.try_clone()?;
        let mut child = Command::new("nix")
            .args([
                "repl",
                "--extra-experimental-features",
                "nix-command flakes",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::from(OwnedFd::from(writer)))
            .stderr(Stdio::from(OwnedFd::from(stderr)))
            .process_group(0)
            .kill_on_drop(true)
            .spawn()?;
        pid.store(
            child.id().context("Nix process has no pid")?,
            Ordering::SeqCst,
        );
        let input = child.stdin.take().context("Nix stdin unavailable")?;
        let mut evaluator = Self {
            child,
            input,
            output: tokio::net::UnixStream::from_std(reader)?,
            names: BTreeSet::new(),
            literals: BTreeMap::new(),
        };
        evaluator.evaluate("null").await?;
        Ok(evaluator)
    }
    pub(super) async fn evaluate(&mut self, source: &str) -> anyhow::Result<String> {
        let marker = format!("prime_agent_fence_{}", uuid::Uuid::new_v4().simple());
        let fence = format!("\"{marker}\"\n");
        self.input
            .write_all(format!("{source}\n{}\n", string(&marker)).as_bytes())
            .await?;
        self.input.flush().await?;
        let mut pending = Vec::new();
        let mut captured = Vec::new();
        let mut chunk = [0; 8192];
        let mut ansi = 0;
        let mut error_tail = Vec::new();
        let mut failed = false;
        loop {
            let count = self.output.read(&mut chunk).await?;
            if count == 0 {
                bail!("Nix REPL exited unexpectedly");
            }
            for byte in &chunk[..count] {
                match (ansi, *byte) {
                    (0, 27) => ansi = 1,
                    (1, b'[') => ansi = 2,
                    (2, 0x40..=0x7e) => ansi = 0,
                    (1 | 2, _) => {}
                    _ => pending.push(*byte),
                }
            }
            let end = pending
                .windows(fence.len())
                .position(|w| w == fence.as_bytes());
            let take = end.unwrap_or_else(|| pending.len().saturating_sub(fence.len()));
            error_tail.extend_from_slice(&pending[..take]);
            failed |= error_tail.starts_with(b"error:")
                || error_tail.windows(7).any(|w| w == b"\nerror:");
            if error_tail.len() > 8 {
                error_tail.drain(..error_tail.len() - 8);
            }
            let available = CAP.saturating_sub(captured.len()).min(take);
            captured.extend_from_slice(&pending[..available]);
            pending.drain(..take);
            if end.is_some() {
                break;
            }
        }
        if captured.len() == CAP {
            bail!("Nix output reached the {CAP}-byte limit; output was discarded");
        }
        let text = String::from_utf8_lossy(&captured).trim().to_string();
        if failed {
            bail!("{text}");
        }
        Ok(text)
    }
    pub(super) async fn parse(&self, expression: &str) -> anyhow::Result<String> {
        let names = self.names.iter().cloned().collect::<Vec<_>>().join(", ");
        let source = format!("{{ {names} }}: (\n{expression}\n)");
        let output = Command::new("nix-instantiate")
            .args(["--parse", "--expr", &source])
            .kill_on_drop(true)
            .output()
            .await?;
        if !output.status.success() {
            bail!("{}", String::from_utf8_lossy(&output.stderr));
        }
        let inherits = self
            .names
            .iter()
            .map(|name| format!("inherit {name};"))
            .collect::<Vec<_>>()
            .join(" ");
        Ok(format!(
            "({}) {{ {inherits} }}",
            String::from_utf8_lossy(&output.stdout).trim()
        ))
    }
    pub(super) async fn bind(&mut self, name: &str, value: &Value) -> anyhow::Result<()> {
        self.evaluate(&format!(
            "{name} = builtins.fromJSON {}",
            string(&serde_json::to_string(value)?)
        ))
        .await?;
        self.names.insert(name.to_string());
        self.literals.remove(name);
        Ok(())
    }
    pub(super) async fn close(&mut self) -> anyhow::Result<()> {
        if let Some(pid) = self.child.id() {
            signal(pid, nix::sys::signal::Signal::SIGKILL);
        }
        self.child.wait().await?;
        Ok(())
    }
}
pub(super) fn signal(pid: u32, signal: nix::sys::signal::Signal) {
    if let Ok(pid) = i32::try_from(pid) {
        if pid <= 0 {
            return;
        }
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), signal);
    }
}

#[cfg(test)]
mod interrupt_tests {
    use super::*;
    use std::os::fd::AsFd;
    use std::time::Duration;

    #[tokio::test]
    async fn active_evaluation_interrupt_drains_fence_and_recovers_repeatedly() {
        let pid = Arc::new(AtomicU32::new(0));
        let mut evaluator = Evaluator::start(&pid).await.unwrap();
        for _ in 0..3 {
            // A duplicate socket observes readiness without stealing bytes from the
            // framing reader. Startup has drained the socket; the next bytes are
            // the trace emitted immediately before entering the expensive fold.
            let fd = evaluator.output.as_fd().try_clone_to_owned().unwrap();
            let socket = std::os::unix::net::UnixStream::from(fd);
            socket.set_nonblocking(true).unwrap();
            let observer = tokio::net::UnixStream::from_std(socket).unwrap();
            let interrupt = async {
                observer.readable().await.unwrap();
                signal(pid.load(Ordering::SeqCst), nix::sys::signal::Signal::SIGINT);
            };
            let evaluate = evaluator.evaluate("builtins.trace \"interrupt-ready\" (builtins.foldl' (a: b: a + b) 0 (builtins.genList (x: x) 10000000))");
            let ((), result) = tokio::time::timeout(Duration::from_secs(15), async {
                tokio::join!(biased; interrupt, evaluate)
            })
            .await
            .expect("SIGINT must settle the evaluator and its fence");
            assert!(
                result.is_err(),
                "interrupted evaluation unexpectedly succeeded"
            );
            assert_eq!(evaluator.evaluate("6 * 7").await.unwrap(), "42");
        }
        evaluator.close().await.unwrap();
    }
}
