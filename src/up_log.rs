//! Preserve one `up` invocation independently of pipes attached to the terminal.

use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static CANCEL_SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn remember_signal(signal: libc::c_int) {
    CANCEL_SIGNAL.store(signal, Ordering::Relaxed);
}

fn install_signal_handlers() -> Result<()> {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: a zeroed sigaction is initialized below; the handler only stores an
        // atomic integer. No allocation, locks or I/O run from the signal handler.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = remember_signal as *const () as libc::sighandler_t;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("installing up cancellation handler");
            }
        }
    }
    Ok(())
}

pub fn run(cwd: &Path) -> Result<i32> {
    let resolved = grove::resolve::resolve(cwd)?;
    let directory = grove::instance::state_dir()?
        .join(resolved.state_key())
        .join(&resolved.slug)
        .join("up-logs");
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let path = directory.join(format!("{timestamp}-{}.log", std::process::id()));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.lock().context("locking the active up log")?;
    let log = Arc::new(Mutex::new(file));
    if let Err(error) = prune_logs(&directory) {
        let warning = format!(
            "warning: could not rotate up logs in {}: {error:#}",
            directory.display()
        );
        let _ = writeln!(std::io::stderr(), "{warning}");
        writeln!(log.lock().unwrap(), "{warning}")?;
    }

    // A separate process keeps every existing print and top-level error in the log.
    // The private argument is not inherited by setup commands or nested Grove runs.
    install_signal_handlers()?;
    let mut args: Vec<_> = std::env::args_os().skip(1).collect();
    let up = args
        .iter()
        .position(|arg| arg == "up")
        .context("finding the up command")?;
    args.insert(up + 1, "--up-log-child".into());
    let mut child = Command::new(std::env::current_exe()?)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting the logged grove up")?;
    let stdout = child.stdout.take().context("capturing stdout")?;
    let stderr = child.stderr.take().context("capturing stderr")?;
    let out_log = Arc::clone(&log);
    let err_log = Arc::clone(&log);
    let out = std::thread::spawn(move || tee(stdout, std::io::stdout(), out_log));
    let err = std::thread::spawn(move || tee(stderr, std::io::stderr(), err_log));
    let status = loop {
        let signal = CANCEL_SIGNAL.swap(0, Ordering::Relaxed);
        if signal != 0 {
            // SAFETY: the unreaped worker is ours. Keep the terminal's process group
            // intact so setup can read stdin. PID-directed cancellation has the same
            // descendant semantics as a signal to Grove before the logging wrapper.
            unsafe {
                libc::kill(child.id() as i32, signal);
            }
        }
        if let Some(status) = child.try_wait().context("waiting for grove up")? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let out_result = out
        .join()
        .map_err(|_| anyhow::anyhow!("stdout log thread panicked"))?;
    let err_result = err
        .join()
        .map_err(|_| anyhow::anyhow!("stderr log thread panicked"))?;
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1));
    {
        let mut file = log.lock().unwrap();
        writeln!(file, "\ngrove up exit code: {code}")?;
        writeln!(file, "up log: {}", path.display())?;
        file.flush()?;
    }
    // Downstream grep/head may already have closed the pipe. The saved log is complete.
    let _ = writeln!(std::io::stderr(), "up log: {}", path.display());
    out_result.with_context(|| format!("saving stdout to {}", path.display()))?;
    err_result.with_context(|| format!("saving stderr to {}", path.display()))?;
    Ok(code)
}

fn tee(
    mut source: impl Read,
    mut terminal: impl Write,
    log: Arc<Mutex<File>>,
) -> std::io::Result<()> {
    let mut buffer = [0; 8192];
    let mut log_error = None;
    let mut terminal_open = true;
    loop {
        let count = match source.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if log_error.is_none()
            && let Err(error) = log.lock().unwrap().write_all(&buffer[..count])
        {
            log_error = Some(error);
        }
        if terminal_open {
            terminal_open = terminal
                .write_all(&buffer[..count])
                .and_then(|_| terminal.flush())
                .is_ok();
        }
    }
    log_error.map_or(Ok(()), Err)
}

/// Retain the newest 50 owned run files. Active older logs survive until a later up.
fn prune_logs(directory: &Path) -> Result<()> {
    let mut logs = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str().and_then(|name| name.strip_suffix(".log")) else {
            continue;
        };
        let Some((timestamp, pid)) = name.split_once('-') else {
            continue;
        };
        if let (Ok(timestamp), Ok(pid)) = (timestamp.parse::<u128>(), pid.parse::<u32>()) {
            logs.push(((timestamp, pid), entry.path()));
        }
    }
    logs.sort_by_key(|(key, _)| std::cmp::Reverse(*key));
    for (_, path) in logs.into_iter().skip(50) {
        let file = match OpenOptions::new().write(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        match file.try_lock() {
            Ok(()) => match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            },
            Err(std::fs::TryLockError::WouldBlock) => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
