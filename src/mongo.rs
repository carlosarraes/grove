//! Check the host connection used by seeds, rather than a forwarded TCP listener.
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PYTHON_PROBE: &str = r#"
import sys
try:
    from pymongo import MongoClient, errors
except ImportError:
    sys.exit(2)
try:
    with MongoClient(sys.argv[1], serverSelectionTimeoutMS=int(sys.argv[2]),
                     connectTimeoutMS=int(sys.argv[2]), socketTimeoutMS=int(sys.argv[2])) as client:
        hello = client.admin.command('hello')
        sys.exit(0 if hello.get('ok') == 1 and hello.get('isWritablePrimary') is True else 1)
except errors.PyMongoError:
    sys.exit(1)
"#;

/// Use the seed's local venv when present, otherwise the existing host Mongo shell.
/// The deadline includes all attempts and reaps a probe that exceeds its budget.
pub fn wait_ready(port: u16, cwd: &Path, timeout: Duration) -> Result<()> {
    let began = Instant::now();
    let python = cwd.join(".venv/bin/python");
    let use_python = python
        .try_exists()
        .context("checking seed Python interpreter")?;
    eprintln!(
        "Mongo on 127.0.0.1:{port}: waiting for a writable primary (timeout {:.0}s)",
        timeout.as_secs_f64()
    );
    let mut next_notice = Duration::from_secs(15);
    loop {
        if began.elapsed() >= next_notice {
            eprintln!(
                "Mongo on 127.0.0.1:{port}: still waiting ({:.0}s)",
                began.elapsed().as_secs_f64()
            );
            next_notice += Duration::from_secs(15);
        }
        let remaining = timeout.saturating_sub(began.elapsed());
        if remaining.is_zero() {
            bail!(
                "Mongo on 127.0.0.1:{port} did not become writable within {:.1}s; seed was not started",
                timeout.as_secs_f64()
            );
        }
        let budget = remaining.min(Duration::from_secs(5));
        let millis = budget.as_millis().max(1);
        let uri = format!(
            "mongodb://127.0.0.1:{port}/?directConnection=true&serverSelectionTimeoutMS={millis}&connectTimeoutMS={millis}&socketTimeoutMS={millis}"
        );
        let mut command = if use_python {
            let mut command = Command::new(&python);
            command.args(["-c", PYTHON_PROBE, &uri, &millis.to_string()]);
            command
        } else {
            let mut command = Command::new("mongosh");
            command.args([
                "--quiet",
                "--norc",
                &uri,
                "--eval",
                "const h=db.hello(); quit(h.ok === 1 && h.isWritablePrimary === true ? 0 : 1)",
            ]);
            command
        };
        let mut child = command.current_dir(cwd).stdin(Stdio::null())
            .stdout(Stdio::null()).stderr(Stdio::null()).spawn()
            .context("starting Mongo readiness probe; provide PyMongo in the seed directory's .venv, or host mongosh when no venv exists")?;
        let attempt = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break Some(status);
            }
            if attempt.elapsed() >= budget || began.elapsed() >= timeout {
                let _ = child.kill();
                child
                    .wait()
                    .context("reaping timed-out Mongo readiness probe")?;
                break None;
            }
            std::thread::sleep(
                Duration::from_millis(25).min(budget.saturating_sub(attempt.elapsed())),
            );
        };
        if let Some(status) = status {
            if status.success() && began.elapsed() < timeout {
                return Ok(());
            }
            if use_python && status.code() == Some(2) {
                bail!(
                    "Mongo readiness needs PyMongo in {}; repair the configured setup before seeding",
                    python.display()
                );
            }
        }
        std::thread::sleep(Duration::from_millis(500).min(timeout.saturating_sub(began.elapsed())));
    }
}
