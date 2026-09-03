//! Starting and stopping an instance's services.
//!
//! No daemon. Each service runs in its own process group with its output on disk, so
//! grove can exit immediately after `up` and still stop the whole tree later.

use anyhow::{Context, Result};
use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Handle {
    pub pid: u32,
    /// Unix seconds at spawn. Recorded rather than read back from `ps`, whose output
    /// format differs between macOS and Linux and would need parsing on both.
    #[serde(default)]
    pub started_at: u64,
}

/// Written to a service's log at each spawn, so `logs --since-restart` can find where
/// this run began amid the replayed build output.
pub const START_MARKER: &str = "=== grove: service started ===";

/// Start a service detached, with its output appended to `log`.
///
/// `process_group(0)` puts the child in a new group of its own — portable across macOS
/// and Linux, where `setsid` is not. It is what makes `stop` able to reach a dev server
/// the shell went on to spawn.
pub fn spawn(
    command: &str,
    cwd: &Path,
    env: &BTreeMap<String, String>,
    log: &Path,
) -> Result<Handle> {
    use std::os::unix::process::CommandExt;

    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("opening {}", log.display()))?;
    let err = out.try_clone().context("duplicating the log handle")?;
    {
        use std::io::Write;
        let mut marker = out.try_clone().context("duplicating the log handle")?;
        let _ = writeln!(marker, "{START_MARKER}");
    }

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .process_group(0)
        .spawn()
        .with_context(|| format!("starting `{command}` in {}", cwd.display()))?;

    let pid = child.id();

    // Reap the child when it exits. An unwaited child lingers as a zombie, and a zombie
    // still answers signal 0 — so without this, `stop` would report a service as running
    // for the whole of its grace period after killing it.
    std::thread::spawn(move || {
        let _ = child.wait();
    });

    let started_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    Ok(Handle { pid, started_at })
}

/// True while any process in the group survives. `process_group(0)` makes the group id
/// equal the leader's pid, so the handle is all we need.
pub fn is_alive(handle: &Handle) -> bool {
    match pid(handle) {
        Some(pid) => test_kill_process_group(pid).is_ok(),
        None => false,
    }
}

/// Stop the whole group: TERM, a grace period, then KILL for anything still standing.
/// Signalling the group rather than the leader is the point — a shell that started a dev
/// server would otherwise leave it holding the port.
pub fn stop(handle: &Handle) -> Result<()> {
    let Some(pid) = pid(handle) else {
        return Ok(());
    };

    if kill_process_group(pid, Signal::TERM).is_err() {
        return Ok(()); // already gone
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if test_kill_process_group(pid).is_err() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let _ = kill_process_group(pid, Signal::KILL);
    Ok(())
}

/// One attempt, no retry: is anything serving this URL right now?
///
/// Any HTTP response counts, including an error status — a health endpoint may report
/// degraded by design, and grove is checking that the port is served, not that the
/// application is happy. `wait_ready` polls on this, so that rule lives in one place.
pub fn probe(url: &str, timeout: Duration) -> bool {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .build()
        .new_agent();
    matches!(
        agent.get(url).call(),
        Ok(_) | Err(ureq::Error::StatusCode(_))
    )
}

/// Why a service did not become ready. Two variants because they call for different
/// reactions: a timeout means wait longer or read the log, an exit means the port was
/// never this service's to answer on.
#[derive(Debug)]
pub enum NotReady {
    Exited { url: String },
    TimedOut { url: String, after: Duration },
}

impl std::fmt::Display for NotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotReady::Exited { url } => write!(f, "the process exited before {url} answered"),
            NotReady::TimedOut { url, after } => {
                write!(f, "{url} never answered within {after:?}")
            }
        }
    }
}

impl std::error::Error for NotReady {}

/// Poll until the service answers, its process exits, or the timeout expires.
///
/// The probe cannot tell this service's answer from a neighbour's on the same port,
/// which is exactly what a second instance is when two `up`s race for a block. The
/// process can: once it has exited, an answer on the port belongs to someone else, and
/// reporting it as readiness would hand every later request to that someone.
pub fn wait_ready(handle: &Handle, url: &str, timeout: Duration) -> Result<(), NotReady> {
    let deadline = Instant::now() + timeout;
    loop {
        if probe(url, Duration::from_secs(2)) {
            return Ok(());
        }
        if !is_alive(handle) {
            return Err(NotReady::Exited {
                url: url.to_string(),
            });
        }
        if Instant::now() >= deadline {
            return Err(NotReady::TimedOut {
                url: url.to_string(),
                after: timeout,
            });
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn pid(handle: &Handle) -> Option<Pid> {
    Pid::from_raw(i32::try_from(handle.pid).ok()?)
}
