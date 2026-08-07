//! Starting and stopping an instance's services.
//!
//! No daemon. Each service runs in its own process group with its output on disk, so
//! treeish can exit immediately after `up` and still stop the whole tree later.

use anyhow::{Context, Result, bail};
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
}

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

    Ok(Handle { pid })
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

/// Poll until the service answers. Any HTTP response counts, including an error status:
/// a health endpoint may report degraded by design, and treeish is checking that the
/// process is up, not that it is happy.
pub fn wait_ready(url: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(2)))
        .build()
        .new_agent();

    loop {
        match agent.get(url).call() {
            Ok(_) => return Ok(()),
            Err(ureq::Error::StatusCode(_)) => return Ok(()),
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => bail!("{url} never answered within {timeout:?} ({e})"),
        }
    }
}

fn pid(handle: &Handle) -> Option<Pid> {
    Pid::from_raw(i32::try_from(handle.pid).ok()?)
}
