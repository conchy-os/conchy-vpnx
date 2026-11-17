//! Running mihomo directly as a supervised child process.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, watch};

use crate::api::Api;

/// Restarts allowed inside [`CRASH_WINDOW`] before giving up.
const MAX_CRASHES: usize = 5;
const CRASH_WINDOW: Duration = Duration::from_secs(60);
const READY_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcState {
    Stopped,
    Running,
    /// Crashed and is being restarted.
    Restarting,
    /// Crashed too often; needs a manual start.
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct Launch {
    pub bin: PathBuf,
    pub home: PathBuf,
    pub config: PathBuf,
    pub socket: PathBuf,
}

/// Check a config with `mihomo -t`. mihomo prints errors to stdout.
pub async fn validate(bin: &Path, home: &Path, config: &Path) -> Result<()> {
    let out = Command::new(bin)
        .arg("-t")
        .arg("-d")
        .arg(home)
        .arg("-f")
        .arg(config)
        .output()
        .await
        .with_context(|| format!("running {}", bin.display()))?;
    if out.status.success() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&out.stdout) + String::from_utf8_lossy(&out.stderr);
    let reason = text
        .lines()
        .rev()
        .find(|l| l.contains("level=error") || l.contains("error") || l.contains("fatal"))
        .unwrap_or(text.trim())
        .to_owned();
    bail!("mihomo rejected the config: {reason}")
}

pub struct Supervisor {
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Supervisor {
    /// Start mihomo, wait until its API answers, then keep it alive.
    pub async fn start(launch: Launch, state: Arc<watch::Sender<ProcState>>) -> Result<Self> {
        let mut child = spawn(&launch).await?;
        if let Err(e) = wait_ready(&launch.socket, &mut child).await {
            terminate(&mut child).await;
            return Err(e);
        }
        state.send_replace(ProcState::Running);

        let (stop_tx, stop_rx) = oneshot::channel();
        let task = tokio::spawn(supervise(launch, child, stop_rx, state));
        Ok(Self {
            stop: Some(stop_tx),
            task: Some(task),
        })
    }

    pub async fn stop(mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// `sun_path` holds 108 bytes including the trailing NUL.
const MAX_SOCKET_PATH: usize = 107;

/// mihomo only logs a too-long controller path and keeps running without
/// its API, so catch it here instead of timing out.
pub fn check_socket_path(socket: &Path) -> Result<()> {
    let len = socket.as_os_str().len();
    if len > MAX_SOCKET_PATH {
        bail!(
            "controller socket path is {len} bytes, the limit is {MAX_SOCKET_PATH}: {}; \
             use a shorter XDG_RUNTIME_DIR",
            socket.display()
        );
    }
    Ok(())
}

async fn spawn(launch: &Launch) -> Result<Child> {
    check_socket_path(&launch.socket)?;
    if let Some(dir) = launch.socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(&launch.socket);
    let mut child = Command::new(&launch.bin)
        .arg("-d")
        .arg(&launch.home)
        .arg("-f")
        .arg(&launch.config)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        // Own group: mihomo's helpers (resolvectl, ...) inherit the TUN fd
        // and would keep the interface alive if they outlived it.
        .process_group(0)
        .spawn()
        .with_context(|| format!("starting {}", launch.bin.display()))?;

    // Forward mihomo's log into ours.
    for stream in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
        child.stderr.take().map(|s| Box::new(s) as _),
    ]
    .into_iter()
    .flatten()
    {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "mihomo", "{line}");
            }
        });
    }
    Ok(child)
}

/// SIGTERM first: in TUN mode mihomo removes its routes and `ip rule`s on
/// exit, which SIGKILL would leave behind. Then clear out the rest of its
/// process group.
async fn terminate(child: &mut Child) {
    let Some(pid) = child.id() else { return };
    signal_group(pid, libc::SIGTERM);
    if tokio::time::timeout(STOP_TIMEOUT, child.wait())
        .await
        .is_err()
    {
        tracing::warn!("mihomo ignored SIGTERM, killing it");
        let _ = child.kill().await;
    }
    signal_group(pid, libc::SIGKILL);
}

fn signal_group(pgid: u32, sig: libc::c_int) {
    // SAFETY: kill(2) on the process group we created in `spawn`.
    unsafe { libc::kill(-(pgid as libc::pid_t), sig) };
}

async fn wait_ready(socket: &Path, child: &mut Child) -> Result<()> {
    let api = Api::new(socket);
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            bail!("mihomo exited during startup ({status}); see the daemon log");
        }
        if api.version().await.is_ok() {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("mihomo did not open its controller socket in time");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn supervise(
    launch: Launch,
    mut child: Child,
    mut stop: oneshot::Receiver<()>,
    state: Arc<watch::Sender<ProcState>>,
) {
    let mut crashes: Vec<Instant> = Vec::new();
    loop {
        let pid = child.id();
        tokio::select! {
            _ = &mut stop => {
                terminate(&mut child).await;
                state.send_replace(ProcState::Stopped);
                return;
            }
            status = child.wait() => {
                tracing::warn!("mihomo exited unexpectedly: {status:?}");
                if let Some(pid) = pid {
                    signal_group(pid, libc::SIGKILL);
                }
                let now = Instant::now();
                crashes.retain(|t| now.duration_since(*t) < CRASH_WINDOW);
                crashes.push(now);
                if crashes.len() > MAX_CRASHES {
                    state.send_replace(ProcState::Failed("mihomo keeps crashing".into()));
                    return;
                }
                state.send_replace(ProcState::Restarting);
                tokio::select! {
                    _ = &mut stop => {
                        state.send_replace(ProcState::Stopped);
                        return;
                    }
                    _ = tokio::time::sleep(Duration::from_secs(crashes.len() as u64)) => {}
                }
                match spawn(&launch).await {
                    Ok(mut c) => match wait_ready(&launch.socket, &mut c).await {
                        Ok(()) => {
                            state.send_replace(ProcState::Running);
                            child = c;
                        }
                        Err(e) => {
                            terminate(&mut c).await;
                            state.send_replace(ProcState::Failed(format!("{e:#}")));
                            return;
                        }
                    },
                    Err(e) => {
                        state.send_replace(ProcState::Failed(format!("{e:#}")));
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_limit() {
        assert!(check_socket_path(Path::new("/run/user/1000/conchy-vpnx/mihomo.sock")).is_ok());
        let long = format!("/tmp/{}/mihomo.sock", "x".repeat(100));
        assert!(check_socket_path(Path::new(&long)).is_err());
    }
}
