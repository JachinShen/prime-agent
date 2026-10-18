//! Local shell execution backend for the bash tool: spawns the shell in its own process group,
//! streams stdout+stderr through one ordered channel, kills the whole group on timeout or abort,
//! and resolves with the exit code (`None` when killed by a signal).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use std::sync::atomic::Ordering;

use crate::tools::bash::{BashOperations, ExecFuture, ExecOptions};
use crate::tools::shell_utils::{get_shell_config, get_shell_env, kill_process_tree};

#[derive(Default)]
pub struct LocalBashOperations {
    pub shell_path: Option<String>,
}

pub(crate) struct CapturingBashOperations {
    pub shell_path: Option<String>,
    pub capture: Arc<std::sync::Mutex<RunCapture>>,
    pub argv: Option<Vec<String>>,
    pub stdin: Option<String>,
}

#[derive(Default)]
pub(crate) struct RunCapture {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: Option<Option<i32>>,
    pub truncated: bool,
}

struct ProcessGroupGuard(Arc<std::sync::Mutex<Option<i32>>>);
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0.lock().unwrap().take() {
            kill_process_tree(pid);
        }
    }
}

impl BashOperations for LocalBashOperations {
    fn exec<'a>(
        &'a self,
        command: &'a str,
        cwd: &'a str,
        options: ExecOptions<'a>,
    ) -> ExecFuture<'a> {
        exec_local(
            self.shell_path.clone(),
            None,
            None,
            None,
            command,
            cwd,
            options,
        )
    }
}

impl BashOperations for CapturingBashOperations {
    fn exec<'a>(
        &'a self,
        command: &'a str,
        cwd: &'a str,
        options: ExecOptions<'a>,
    ) -> ExecFuture<'a> {
        exec_local(
            self.shell_path.clone(),
            Some(self.capture.clone()),
            self.argv.as_deref(),
            self.stdin.as_deref(),
            command,
            cwd,
            options,
        )
    }
}

#[tracing::instrument(level = "trace", name = "bash_local_exec", skip_all)]
fn exec_local<'a>(
    shell_path: Option<String>,
    capture: Option<Arc<std::sync::Mutex<RunCapture>>>,
    argv: Option<&'a [String]>,
    stdin: Option<&'a str>,
    command: &'a str,
    cwd: &'a str,
    options: ExecOptions<'a>,
) -> ExecFuture<'a> {
    // Destructure up front: #[instrument] keeps a guard on the argument
    // binding, so partial moves of its fields would borrow-check-fail.
    let ExecOptions {
        on_data,
        signal,
        timeout,
        env,
    } = options;
    Box::pin(async move {
        if let Some(capture) = &capture {
            *capture.lock().unwrap() = RunCapture::default();
        }

        if !std::path::Path::new(cwd).exists() {
            anyhow::bail!("Working directory does not exist: {cwd}\nCannot execute bash commands.");
        }

        let mut process = if let Some(argv) = argv {
            let mut process = std::process::Command::new(&argv[0]);
            process.args(&argv[1..]);
            process
        } else {
            let shell = get_shell_config(shell_path.as_deref())?;
            let mut process = std::process::Command::new(&shell.shell);
            process.args(&shell.args).arg(command);
            process
        };
        process
            .current_dir(cwd)
            .stdin(if stdin.is_some() {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for (key, value) in env.unwrap_or_else(get_shell_env) {
            process.env(key, value);
        }
        // Detached process group on POSIX only, so kill_process_tree can reach descendants
        // (TS: `detached: process.platform !== "win32"`); on Windows tree kills go through
        // `taskkill /T`. Hidden window everywhere.
        #[cfg(unix)]
        crate::platform::process::set_new_process_group(&mut process);
        crate::platform::process::set_no_window(&mut process);

        if signal
            .as_ref()
            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
        {
            anyhow::bail!("aborted");
        }
        let mut child = process.spawn()?;

        let pid = child.id() as i32;
        let group = Arc::new(std::sync::Mutex::new(Some(pid)));
        let _process_guard = ProcessGroupGuard(group.clone());
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (stdin_tx, mut stdin_rx) = tokio::sync::oneshot::channel();
        if let Some(input) = stdin {
            let input = input.to_owned();
            let mut pipe = child.stdin.take().expect("piped stdin");
            std::thread::spawn(move || {
                use std::io::Write;
                let result = pipe.write_all(input.as_bytes()).or_else(|error| {
                    if error.kind() == std::io::ErrorKind::BrokenPipe {
                        Ok(())
                    } else {
                        Err(error)
                    }
                });
                drop(pipe);
                let _ = stdin_tx.send(result);
            });
        } else {
            let _ = stdin_tx.send(Ok(()));
        }
        let mut stdin_complete = false;

        // Chunks from both pipes flow through one channel preserving arrival
        // order, like node's stream events.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(bool, Vec<u8>)>(16);
        {
            let tx = tx.clone();
            std::thread::spawn(move || {
                use std::io::Read;
                let mut reader = std::io::BufReader::new(stdout);
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if tx.blocking_send((false, buf[..n].to_vec())).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
        std::thread::spawn(move || {
            use std::io::Read;
            let mut reader = std::io::BufReader::new(stderr);
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.blocking_send((true, buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let receive = |(is_stderr, chunk): (bool, Vec<u8>)| {
            if let Some(capture) = &capture {
                let mut capture = capture.lock().unwrap();
                // Bound each stream before it enters the JSON/Nix value.
                let bytes = if is_stderr {
                    &mut capture.stderr
                } else {
                    &mut capture.stdout
                };
                let retained = chunk
                    .len()
                    .min((1024 * 1024usize).saturating_sub(bytes.len()));
                bytes.extend_from_slice(&chunk[..retained]);
                capture.truncated |= retained < chunk.len();
            }
            (on_data)(&chunk);
        };
        let waiter_group = group.clone();
        let mut waiter = tokio::task::spawn_blocking(move || {
            // Keep the leader unreaped until its descendants are killed. This
            // prevents a recycled PID from being signalled during cleanup.
            #[cfg(unix)]
            {
                use nix::sys::wait::{waitid, Id, WaitPidFlag};
                loop {
                    match waitid(
                        Id::Pid(nix::unistd::Pid::from_raw(pid)),
                        WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT,
                    ) {
                        Err(nix::errno::Errno::EINTR) => continue,
                        result => {
                            result.map_err(std::io::Error::from)?;
                            break;
                        }
                    }
                }
            }
            #[cfg(unix)]
            if let Some(pid) = waiter_group.lock().unwrap().take() {
                kill_process_tree(pid);
            }
            let status = child.wait();
            #[cfg(not(unix))]
            {
                waiter_group.lock().unwrap().take();
            }
            status
        });
        let timeout_deadline = timeout
            .filter(|t| *t > 0.0)
            .map(|t| tokio::time::Instant::now() + Duration::from_secs_f64(t));
        let mut abort_watcher = signal
            .as_ref()
            .map(|signal| Box::pin(signal.clone().cancelled_owned()));

        // Abort before spawn kills immediately (node checks signal.aborted
        // synchronously and attaches the abort listener otherwise).
        if let Some(signal) = &signal {
            if signal.is_cancelled() {
                if let Some(pid) = group.lock().unwrap().take() {
                    kill_process_tree(pid);
                }
            }
        }

        let timed_out = Arc::new(AtomicBool::new(false));
        let exit_status: std::io::Result<std::process::ExitStatus> = 'wait: {
            loop {
                tokio::select! {
                    result = &mut stdin_rx, if !stdin_complete => {
                        result??;
                        stdin_complete = true;
                    }
                    status = &mut waiter => {
                        break 'wait status?;
                    }
                    () = async {
                        match timeout_deadline {
                            Some(deadline) => tokio::time::sleep_until(deadline).await,
                            None => std::future::pending::<()>().await,
                        }
                    }, if !timed_out.load(Ordering::SeqCst) && timeout_deadline.is_some() => {
                        timed_out.store(true, Ordering::SeqCst);
                        if let Some(pid) = group.lock().unwrap().take() { kill_process_tree(pid); }
                    }
                    () = async {
                        match abort_watcher.as_mut() {
                            Some(cancelled) => cancelled.as_mut().await,
                            None => std::future::pending::<()>().await,
                        }
                    }, if abort_watcher.is_some() => {
                        if let Some(pid) = group.lock().unwrap().take() { kill_process_tree(pid); }
                        abort_watcher = None;
                    }
                    Some(chunk) = rx.recv() => {
                        receive(chunk);
                    }
                }
            }
        };
        loop {
            tokio::select! {
                chunk = rx.recv() => match chunk {
                    Some(chunk) => receive(chunk),
                    None => break,
                },
                () = async {
                    match &signal {
                        Some(signal) => signal.cancelled().await,
                        None => std::future::pending().await,
                    }
                } => anyhow::bail!("aborted"),
                () = async {
                    match timeout_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => anyhow::bail!("timeout:{}", timeout.unwrap_or_default()),
            }
        }

        if !stdin_complete {
            tokio::select! {
                result = &mut stdin_rx => { result??; }
                () = async {
                    match &signal {
                        Some(signal) => signal.cancelled().await,
                        None => std::future::pending().await,
                    }
                } => anyhow::bail!("aborted"),
                () = async {
                    match timeout_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => anyhow::bail!("timeout:{}", timeout.unwrap_or_default()),
            }
        }
        let status = exit_status.map_err(|err| anyhow::anyhow!("{err}"))?;

        if let Some(signal) = &signal {
            if signal.is_cancelled() {
                anyhow::bail!("aborted");
            }
        }
        if timed_out.load(Ordering::SeqCst) {
            anyhow::bail!("timeout:{}", timeout.unwrap_or_default());
        }
        if let Some(capture) = &capture {
            capture.lock().unwrap().exit_code = Some(status.code());
        }
        Ok(status.code())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_with_blocked_stdin_finishes() {
        let signal = tokio_util::sync::CancellationToken::new();
        let ops = CapturingBashOperations {
            shell_path: None,
            capture: Arc::new(std::sync::Mutex::new(RunCapture::default())),
            argv: Some(vec![
                "sh".to_string(),
                "-c".to_string(),
                "printf ready; exec sleep 300".to_string(),
            ]),
            stdin: Some("x".repeat(2 * 1024 * 1024)),
        };
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            ops.exec(
                "unused",
                "/tmp",
                ExecOptions {
                    on_data: &|_| signal.cancel(),
                    signal: Some(signal.clone()),
                    timeout: None,
                    env: None,
                },
            ),
        )
        .await
        .expect("cancellation must release blocked stdin");
        assert_eq!(result.unwrap_err().to_string(), "aborted");
    }

    #[tokio::test]
    async fn cancelled_before_spawn_has_no_side_effect() {
        let dir = tempfile::tempdir().unwrap();
        let signal = tokio_util::sync::CancellationToken::new();
        signal.cancel();
        let result = LocalBashOperations::default()
            .exec(
                "touch should-not-exist",
                &dir.path().to_string_lossy(),
                ExecOptions {
                    on_data: &|_| {},
                    signal: Some(signal),
                    timeout: None,
                    env: None,
                },
            )
            .await;
        assert!(result.unwrap_err().to_string().contains("aborted"));
        assert!(!dir.path().join("should-not-exist").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_running_call_kills_the_process_group() {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let ready = std::sync::Mutex::new(Some(ready_tx));
        let task = tokio::spawn(async move {
            LocalBashOperations::default()
                .exec(
                    "sleep 300 & printf '%s %s\n' $$ $!; wait",
                    "/tmp",
                    ExecOptions {
                        on_data: &|chunk| {
                            if let Some(tx) = ready.lock().unwrap().take() {
                                let _ = tx.send(
                                    String::from_utf8_lossy(chunk)
                                        .split_whitespace()
                                        .map(|pid| pid.parse::<i32>().unwrap())
                                        .collect::<Vec<_>>(),
                                );
                            }
                        },
                        signal: None,
                        timeout: None,
                        env: None,
                    },
                )
                .await
        });
        let pids = tokio::time::timeout(Duration::from_secs(5), ready_rx)
            .await
            .unwrap()
            .unwrap();
        task.abort();
        let _ = task.await;
        tokio::time::timeout(Duration::from_secs(5), async {
            for pid in pids {
                while nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok() {
                    // Orphan zombies cannot execute or retain output pipes.
                    #[cfg(target_os = "linux")]
                    if std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                        stat.rsplit_once(") ")
                            .is_some_and(|(_, tail)| tail.starts_with('Z'))
                    }) {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            }
        })
        .await
        .expect("shell must be killed and reaped");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn foreground_exit_closes_descendant_pipes() {
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            LocalBashOperations::default()
                .exec(
                    "sleep 300 & printf done",
                    "/tmp",
                    ExecOptions {
                        on_data: &|_| {},
                        signal: None,
                        timeout: None,
                        env: None,
                    },
                )
                .await
        })
        .await
        .expect("orphaned pipe writer must not keep foreground command alive")
        .unwrap();
        assert_eq!(result, Some(0));
    }
}
