//! Explicit cleanup of idle Mongo testcontainers. Reports by default.
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CString, OsString};
use std::io::{ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const INSPECT: &str = r#"{"id":{{json .Id}},"name":{{json .Name}},"running":{{json .State.Running}},"started":{{json .State.StartedAt}},"image":{{json .Config.Image}},"label":{{json (index .Config.Labels "org.testcontainers")}},"ports":{{json .NetworkSettings.Ports}}}"#;

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct Container {
    id: String,
    name: String,
    running: bool,
    started: String,
    image: String,
    label: Option<String>,
    ports: BTreeMap<String, Option<Vec<Binding>>>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct Binding {
    #[serde(rename = "HostIp")]
    host_ip: String,
    #[serde(rename = "HostPort")]
    host_port: String,
}

fn docker() -> OsString {
    std::env::var_os("GROVE_DOCKER").unwrap_or_else(|| "docker".into())
}

fn nonblocking(reader: &impl AsRawFd) -> Result<()> {
    // SAFETY: the descriptor belongs to a live child pipe. Existing flags are preserved.
    unsafe {
        let flags = libc::fcntl(reader.as_raw_fd(), libc::F_GETFL);
        if flags < 0 || libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0
        {
            return Err(std::io::Error::last_os_error()).context("setting cleanup pipe mode");
        }
    }
    Ok(())
}

fn drain(reader: &mut impl Read, bytes: &mut Vec<u8>) -> Result<bool> {
    let mut buffer = [0; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                if bytes.len() + count > 1024 * 1024 {
                    bail!("cleanup probe output exceeded its limit");
                }
                bytes.extend_from_slice(&buffer[..count]);
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

/// A slow daemon or host probe is unknown evidence, never permission to delete.
fn output(command: &mut Command) -> Result<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .context("starting cleanup probe")?;
    let pid = child.id();
    let result = (|| {
        let mut stdout = child.stdout.take().context("missing probe stdout")?;
        let mut stderr = child.stderr.take().context("missing probe stderr")?;
        nonblocking(&stdout)?;
        nonblocking(&stderr)?;
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let began = Instant::now();
        let mut status = None;
        loop {
            if began.elapsed() >= Duration::from_secs(10) {
                bail!("cleanup probe timed out after 10s");
            }
            let out_closed = drain(&mut stdout, &mut out)?;
            let err_closed = drain(&mut stderr, &mut err)?;
            if status.is_none() {
                status = child.try_wait()?;
            }
            if let Some(status) = status
                && out_closed
                && err_closed
            {
                return Ok(Output {
                    status,
                    stdout: out,
                    stderr: err,
                });
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    })();
    // SAFETY: the child has its own process group. Only this invocation and its
    // descendants in that group are signalled. Pipe reads also obey the deadline.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    let _ = child.wait();
    result
}

fn inspect(id: &str) -> Result<Container> {
    let out = output(Command::new(docker()).args(["inspect", "--format", INSPECT, id]))?;
    if !out.status.success() {
        bail!("container inspection failed");
    }
    let container: Container =
        serde_json::from_slice(&out.stdout).context("invalid container metadata")?;
    if container.id.len() != 64 || !container.id.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("container inspection did not return a full ID");
    }
    if !container.id.starts_with(id) {
        bail!("container identity changed");
    }
    Ok(container)
}

fn started_at(text: &str) -> Result<u64> {
    let (prefix, suffix) = text
        .split_at_checked(19)
        .context("invalid start timestamp")?;
    if suffix != "Z"
        && !(suffix.starts_with('.')
            && suffix.ends_with('Z')
            && suffix.len() >= 3
            && suffix.len() <= 11
            && suffix[1..suffix.len() - 1]
                .bytes()
                .all(|b| b.is_ascii_digit()))
    {
        bail!("invalid start timestamp");
    }
    let input = CString::new(prefix)?;
    // SAFETY: strptime and timegm receive initialized, writable tm storage and a
    // NUL-terminated input. The normalized fields must match to reject invalid dates.
    unsafe {
        let mut parsed: libc::tm = std::mem::zeroed();
        let end = libc::strptime(input.as_ptr(), c"%Y-%m-%dT%H:%M:%S".as_ptr(), &mut parsed);
        if end.is_null() || *end != 0 {
            bail!("invalid start timestamp");
        }
        let fields = (
            parsed.tm_year,
            parsed.tm_mon,
            parsed.tm_mday,
            parsed.tm_hour,
            parsed.tm_min,
            parsed.tm_sec,
        );
        let timestamp = libc::timegm(&mut parsed);
        if timestamp < 0
            || fields
                != (
                    parsed.tm_year,
                    parsed.tm_mon,
                    parsed.tm_mday,
                    parsed.tm_hour,
                    parsed.tm_min,
                    parsed.tm_sec,
                )
        {
            bail!("invalid start timestamp");
        }
        Ok(timestamp as u64)
    }
}

fn eligible(container: &Container) -> Result<u64> {
    if !container.running {
        bail!("not running");
    }
    if container.name.trim_start_matches('/').starts_with("grove-") {
        bail!("Grove-owned resource");
    }
    if container.label.as_deref() != Some("true") {
        bail!("not labelled as testcontainers");
    }
    let image = container.image.split([':', '@']).next().unwrap_or("");
    if !["mongo", "library/mongo", "docker.io/library/mongo"].contains(&image) {
        bail!("not a recognized Mongo image");
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let age = now
        .checked_sub(started_at(&container.started)?)
        .context("start timestamp is in the future")?;
    if age <= 45 * 60 {
        bail!("running for no more than 45 minutes");
    }
    Ok(age)
}

fn disconnected(container: &Container) -> Result<()> {
    let bindings = container
        .ports
        .get("27017/tcp")
        .and_then(Option::as_ref)
        .filter(|ports| !ports.is_empty())
        .context("no published Mongo port to verify")?;
    let out = output(Command::new(docker()).args([
        "exec",
        &container.id,
        "cat",
        "/proc/net/tcp",
        "/proc/net/tcp6",
    ]))?;
    if !out.status.success() || !out.stderr.is_empty() {
        bail!("container socket evidence unavailable");
    }
    let text = std::str::from_utf8(&out.stdout)?;
    let mut headers = 0;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.first() == Some(&"sl") && fields.get(1) == Some(&"local_address") {
            headers += 1;
            continue;
        }
        if fields.len() < 4 {
            bail!("invalid container socket table");
        }
        let (address, port) = fields[1]
            .rsplit_once(':')
            .context("invalid socket address")?;
        if ![8, 32].contains(&address.len())
            || !address.bytes().all(|b| b.is_ascii_hexdigit())
            || port.len() != 4
            || fields[3].len() != 2
        {
            bail!("invalid container socket row");
        }
        let port = u16::from_str_radix(port, 16)?;
        let state = u8::from_str_radix(fields[3], 16)?;
        if port == 27017 && state == 1 {
            bail!("established Mongo connection inside container");
        }
    }
    if headers != 2 {
        bail!("incomplete container socket evidence");
    }
    let ports: BTreeSet<u16> = bindings
        .iter()
        .map(|binding| binding.host_port.parse().context("invalid published port"))
        .collect::<Result<_>>()?;
    for port in ports {
        if port == 0 {
            bail!("invalid published port");
        }
        let out = output(Command::new("lsof").args([
            "-nP",
            "-a",
            &format!("-iTCP:{port}"),
            "-sTCP:ESTABLISHED",
            "-Fpn",
        ]))?;
        if out.status.code() != Some(1) || !out.stdout.is_empty() || !out.stderr.is_empty() {
            bail!("host has an established connection or socket evidence is unavailable");
        }
    }
    Ok(())
}

pub fn run(remove: bool) -> Result<()> {
    let out = output(Command::new(docker()).args([
        "ps",
        "--filter",
        "label=org.testcontainers=true",
        "--format",
        "{{.ID}}",
    ]))?;
    if !out.status.success() {
        bail!("cannot list testcontainers");
    }
    for id in std::str::from_utf8(&out.stdout)?.split_whitespace() {
        if id.is_empty() || !id.bytes().all(|c| c.is_ascii_hexdigit()) {
            bail!("invalid container ID in listing");
        }
        let container = match inspect(id) {
            Ok(container) => container,
            Err(error) => {
                println!("keep {id}: {error:#}");
                continue;
            }
        };
        let name = container.name.trim_start_matches('/');
        let age = match eligible(&container).and_then(|age| disconnected(&container).map(|()| age))
        {
            Ok(age) => age,
            Err(error) => {
                println!("keep {name}: {error:#}");
                continue;
            }
        };
        println!(
            "candidate {name}: {} minutes running, no established Mongo connections",
            age / 60
        );
        if remove {
            let recheck = (|| -> Result<()> {
                let current = inspect(&container.id)?;
                if current != container {
                    bail!("container state changed");
                }
                eligible(&current)?;
                disconnected(&current)?;
                if inspect(&container.id)? != container {
                    bail!("container state changed during probes");
                }
                Ok(())
            })();
            if let Err(error) = recheck {
                println!("keep {name}: {error:#}");
                continue;
            }
            let out = output(Command::new(docker()).args(["rm", "-f", &container.id]))?;
            if !out.status.success() {
                bail!("failed to remove {name} ({})", container.id);
            }
            println!("removed {name} ({})", container.id);
        }
    }
    if !remove {
        println!("Report only. Use --remove-stale to recheck and remove eligible containers.");
    }
    Ok(())
}
