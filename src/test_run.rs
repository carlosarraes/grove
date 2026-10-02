//! One command group owns a recorded inventory of test databases.
use crate::{
    config::TestMongo,
    test_mongo::{Manager, OwnedMongo},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

#[derive(Serialize, Deserialize)]
struct Record {
    id: String,
    mongo: OwnedMongo,
    pgid: Option<i32>,
    closing: bool,
    databases: Vec<String>,
}

fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn database(id: &str, ordinal: usize) -> String {
    format!("grove_test_{id}_{ordinal:08x}")
}
fn random_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
fn lock(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)?)
}
fn save(dir: &Path, record: &Record) -> Result<()> {
    let temporary = dir.join(format!("{}.tmp", record.id));
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec(record)?)?;
    file.sync_all()?;
    std::fs::rename(temporary, dir.join(format!("{}.json", record.id)))?;
    Ok(())
}
fn read(dir: &Path, id: &str) -> Result<Record> {
    let record: Record = serde_json::from_slice(&std::fs::read(dir.join(format!("{id}.json")))?)?;
    if !valid_id(id)
        || record.id != id
        || record.databases.is_empty()
        || record
            .databases
            .iter()
            .enumerate()
            .any(|(n, db)| *db != database(id, n))
    {
        bail!("invalid test run database inventory; retained for inspection");
    }
    Ok(record)
}
fn group_alive(pgid: i32) -> Result<bool> {
    if pgid <= 1 {
        bail!("invalid test run process group");
    }
    // SAFETY: signal zero only queries the recorded process group.
    if unsafe { libc::kill(-pgid, 0) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(false)
    } else {
        Err(error.into())
    }
}
fn signal_group(pgid: i32, signal: i32) {
    if pgid > 1 {
        // SAFETY: the caller supplies the group of its own child.
        unsafe {
            libc::kill(-pgid, signal);
        }
    }
}
fn remove_record(dir: &Path, id: &str) -> Result<()> {
    std::fs::remove_file(dir.join(format!("{id}.json")))?;
    // Keep lock inodes: another allocator may already have opened one.
    Ok(())
}
fn cleanup(manager: &Manager, dir: &Path, record: &Record) -> Result<()> {
    let pgid = record
        .pgid
        .context("test run has uncertain process ownership; cleanup retained")?;
    if group_alive(pgid)? {
        bail!("test run still has a live process group; cleanup retained");
    }
    manager.drop_databases(&record.mongo, &record.databases)?;
    remove_record(dir, &record.id)
}
fn reap(manager: &Manager, dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let Some(id) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|id| valid_id(id))
        else {
            continue;
        };
        let lease = lock(&dir.join(format!("{id}.active")))?;
        match lease.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => continue,
            Err(error) => return Err(error.into()),
        }
        let metadata = lock(&dir.join(format!("{id}.lock")))?;
        metadata.lock()?;
        // A completed run can remove its record before releasing its lease.
        if !path.exists() {
            continue;
        }
        match read(dir, id).and_then(|record| cleanup(manager, dir, &record)) {
            Ok(()) => {}
            Err(error) => eprintln!("warning: retained test run {id}: {error:#}"),
        }
    }
    Ok(())
}

pub fn allocate() -> Result<String> {
    let id = std::env::var("GROVE_TEST_RUN_ID")
        .ok()
        .filter(|id| valid_id(id))
        .context("test-db allocate requires an active test run")?;
    let dir = crate::instance::state_dir()?.join("test-mongo/runs");
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join(format!("{id}.active")))
        .context("test-db allocate requires an active test run")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match lease.try_lock() {
            Ok(()) => bail!("test-db allocate requires an active test run"),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(error) => return Err(error.into()),
        }
        let metadata = lock(&dir.join(format!("{id}.lock")))?;
        metadata.lock()?;
        let mut record = read(&dir, &id)?;
        if record.closing {
            bail!("test run is closing; allocation refused");
        }
        if let Some(pgid) = record.pgid {
            // SAFETY: getpgrp has no arguments or side effects.
            if pgid != unsafe { libc::getpgrp() } {
                bail!("allocator is outside the owning test run process group");
            }
            let name = database(&id, record.databases.len());
            record.databases.push(name.clone());
            save(&dir, &record)?;
            return Ok(name);
        }
        drop(metadata);
        if Instant::now() >= deadline {
            bail!("test run process registration did not complete");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

static CANCEL: AtomicI32 = AtomicI32::new(0);
extern "C" fn remember_signal(signal: libc::c_int) {
    CANCEL.store(signal, Ordering::Relaxed);
}
struct Signals(Vec<(i32, libc::sigaction)>);
impl Signals {
    fn install() -> Result<Self> {
        CANCEL.store(0, Ordering::Relaxed);
        let mut installed = Self(Vec::new());
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            // SAFETY: both sigactions are initialized; handler only stores an atomic integer.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                let mut old: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = remember_signal as *const () as libc::sighandler_t;
                libc::sigemptyset(&mut action.sa_mask);
                if libc::sigaction(signal, &action, &mut old) != 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                installed.0.push((signal, old));
            }
        }
        Ok(installed)
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        for (signal, action) in &self.0 {
            // SAFETY: these are the handlers saved by install.
            unsafe {
                libc::sigaction(*signal, action, std::ptr::null_mut());
            }
        }
    }
}

struct Foreground(Option<i32>);
impl Foreground {
    fn give_to(pgid: i32) -> Result<Self> {
        // SAFETY: terminal queries use the inherited stdin descriptor.
        let previous = unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) };
        if previous < 0 || previous != unsafe { libc::getpgrp() } {
            return Ok(Self(None));
        }
        Self::set(pgid)?;
        signal_group(pgid, libc::SIGCONT);
        Ok(Self(Some(previous)))
    }
    fn set(pgid: i32) -> Result<()> {
        // SAFETY: block SIGTTOU in this thread during terminal handoff, then restore
        // its original mask. The child retains its normal signal disposition.
        unsafe {
            let mut blocked: libc::sigset_t = std::mem::zeroed();
            let mut old: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut blocked);
            libc::sigaddset(&mut blocked, libc::SIGTTOU);
            let error = libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut old);
            if error != 0 {
                return Err(std::io::Error::from_raw_os_error(error).into());
            }
            let result = libc::tcsetpgrp(libc::STDIN_FILENO, pgid);
            let error = std::io::Error::last_os_error();
            libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
            if result != 0 {
                return Err(error).context("transferring test command terminal");
            }
        }
        Ok(())
    }
}
impl Drop for Foreground {
    fn drop(&mut self) {
        if let Some(pgid) = self.0
            && let Err(error) = Self::set(pgid)
        {
            eprintln!("warning: restoring terminal: {error:#}");
        }
    }
}

pub fn run(
    config: &TestMongo,
    cwd: &Path,
    argv: &[String],
    env: BTreeMap<String, String>,
) -> Result<ExitStatus> {
    let state = crate::instance::state_dir()?.join("test-mongo");
    let manager = Manager::new(
        state.clone(),
        std::env::var_os("GROVE_DOCKER").unwrap_or_else(|| "docker".into()),
    )?;
    let mongo = crate::timing::measure("test Mongo resource", || manager.ensure(config))?;
    let dir = state.join("runs");
    std::fs::create_dir_all(&dir)?;
    crate::timing::measure("test Mongo abandoned cleanup", || reap(&manager, &dir))?;
    let id = random_id()?;
    let lease = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(dir.join(format!("{id}.active")))?;
    lease.lock()?;
    let mut record = Record {
        id: id.clone(),
        mongo,
        pgid: None,
        closing: false,
        databases: vec![database(&id, 0)],
    };
    save(&dir, &record)?;
    let _signals = Signals::install()?;
    let mut child = match Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(cwd)
        .envs(env)
        .env("GROVE_TEST_RUN_ID", &id)
        .env("GROVE_TEST_MONGODB_URI", record.mongo.uri())
        .env("GROVE_TEST_DB_NAME", &record.databases[0])
        .env("GROVE_TEST_ALLOCATOR", std::env::current_exe()?)
        .process_group(0)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            remove_record(&dir, &id)?;
            return Err(error).context("starting test command");
        }
    };
    let pgid = i32::try_from(child.id())?;
    record.pgid = Some(pgid);
    let metadata = lock(&dir.join(format!("{id}.lock")))?;
    metadata.lock()?;
    if let Err(error) = save(&dir, &record) {
        signal_group(pgid, libc::SIGKILL);
        let _ = child.wait();
        return Err(error);
    }
    drop(metadata);
    let foreground = match Foreground::give_to(pgid) {
        Ok(terminal) => terminal,
        Err(error) => {
            signal_group(pgid, libc::SIGKILL);
            let _ = child.wait();
            return Err(error);
        }
    };
    let mut cancellation = None;
    let command_began = Instant::now();
    let status = loop {
        let signal = CANCEL.load(Ordering::Relaxed);
        if signal != 0 && cancellation.is_none() {
            signal_group(pgid, signal);
            cancellation = Some(Instant::now());
        }
        if cancellation.is_some_and(|start| start.elapsed() >= Duration::from_secs(3)) {
            signal_group(pgid, libc::SIGKILL);
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    eprintln!(
        "timing: test command: exit {} ({:.3}s)",
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)),
        command_began.elapsed().as_secs_f64()
    );
    drop(foreground);
    let result = crate::timing::measure("test Mongo cleanup", || -> Result<()> {
        let metadata = lock(&dir.join(format!("{id}.lock")))?;
        metadata.lock()?;
        let mut record = read(&dir, &id)?;
        record.closing = true;
        save(&dir, &record)?;
        if group_alive(pgid)? {
            signal_group(pgid, libc::SIGTERM);
            let deadline = Instant::now() + Duration::from_secs(3);
            while group_alive(pgid)? && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if group_alive(pgid)? {
                signal_group(pgid, libc::SIGKILL);
            }
        }
        cleanup(&manager, &dir, &record)
    });
    if let Err(error) = result {
        eprintln!("warning: retained test run {id}: {error:#}");
    }
    let signal = CANCEL.load(Ordering::Relaxed);
    if signal != 0 && status.success() {
        return Ok(ExitStatus::from_raw(signal));
    }
    Ok(status)
}
