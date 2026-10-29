//! What the most recent `grove up` for an instance ended with, read back from the run
//! log `up` already keeps. A setup-only instance that never got past admission looks
//! exactly like a healthy stopped one unless something says so.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub enum Outcome {
    Succeeded,
    Failed {
        code: i32,
        error: Option<String>,
    },
    /// Exit 130: someone pressed Ctrl-C. Not a fault in the instance, but setup may
    /// still be unfinished, so it is reported rather than hidden.
    Interrupted,
    /// Still holding its log lock: an `up` is running right now.
    Running,
    /// No exit line and nobody holding the log: the run was killed outright.
    Unfinished,
}

pub struct LastUp {
    pub log: PathBuf,
    pub at: SystemTime,
    pub outcome: Outcome,
    /// When the run began, from the log's name.
    pub started: Option<SystemTime>,
    /// The wrapper's pid, from the log's name.
    pub pid: Option<u32>,
    /// The newest progress line, so a slow step is not mistaken for a hung one.
    pub phase: Option<String>,
}

impl LastUp {
    pub fn failed(&self) -> bool {
        matches!(self.outcome, Outcome::Failed { .. } | Outcome::Unfinished)
    }

    pub fn running(&self) -> bool {
        matches!(self.outcome, Outcome::Running)
    }

    /// `last up 23:31: startup admission: timed out after 602.2s`
    pub fn interrupted(&self) -> bool {
        matches!(self.outcome, Outcome::Interrupted)
    }

    pub fn describe(&self) -> String {
        let when = clock(self.at);
        match &self.outcome {
            Outcome::Succeeded => format!("last up {when}: ok"),
            Outcome::Running => {
                let since = self.started.map(clock).unwrap_or(when);
                match self.pid {
                    Some(pid) => format!("grove up still running here (since {since}, pid {pid})"),
                    None => format!("grove up still running here (since {since})"),
                }
            }
            Outcome::Interrupted => format!("last up {when}: interrupted (Ctrl-C)"),
            Outcome::Unfinished => format!("last up {when}: killed before it finished"),
            Outcome::Failed {
                error: Some(error), ..
            } => format!("last up {when}: {error}"),
            Outcome::Failed { code, error: None } => {
                format!("last up {when}: failed with exit code {code}")
            }
        }
    }

    pub fn exit_code(&self) -> Option<i32> {
        match self.outcome {
            Outcome::Succeeded => Some(0),
            Outcome::Failed { code, .. } => Some(code),
            Outcome::Interrupted => Some(130),
            _ => None,
        }
    }

    pub fn error(&self) -> Option<&str> {
        match &self.outcome {
            Outcome::Failed { error, .. } => error.as_deref(),
            _ => None,
        }
    }
}

/// The newest run log under `<instance_dir>/up-logs`, or None if `up` never logged here.
pub fn read(instance_dir: &Path) -> Option<LastUp> {
    let newest = std::fs::read_dir(instance_dir.join("up-logs"))
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let (timestamp, pid) = name.to_str()?.strip_suffix(".log")?.split_once('-')?;
            let key = (timestamp.parse::<u128>().ok()?, pid.parse::<u32>().ok()?);
            Some((key, entry.path()))
        })
        .max_by_key(|(key, _)| *key)?;
    let ((nanos, pid), newest) = newest;
    let started = u64::try_from(nanos)
        .ok()
        .map(|n| SystemTime::UNIX_EPOCH + std::time::Duration::from_nanos(n));
    let body = std::fs::read_to_string(&newest).ok()?;
    let at = std::fs::metadata(&newest).and_then(|m| m.modified()).ok()?;
    let code = body
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("grove up exit code: "))
        .and_then(|code| code.trim().parse::<i32>().ok());
    let outcome = match code {
        Some(0) => Outcome::Succeeded,
        Some(130) => Outcome::Interrupted,
        Some(code) => Outcome::Failed {
            code,
            error: body
                .lines()
                .rev()
                .find_map(|line| line.strip_prefix("Error: "))
                .map(summarize),
        },
        None if held(&newest) => Outcome::Running,
        None => Outcome::Unfinished,
    };
    let phase = body
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("timing: "))
        .map(str::to_string);
    Some(LastUp {
        log: newest,
        at,
        outcome,
        started,
        pid: Some(pid),
        phase,
    })
}

/// The headline of an error, not its remedy: the part before the first `;`, capped.
fn summarize(error: &str) -> String {
    let head = error.split(';').next().unwrap_or(error).trim();
    if head.chars().count() > 120 {
        format!("{}…", head.chars().take(119).collect::<String>())
    } else {
        head.to_string()
    }
}

/// The wrapper holds an exclusive lock on its log for the whole run.
fn held(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

/// Local wall-clock time: `23:31` today, `Oct 28 23:31` otherwise.
fn clock(at: SystemTime) -> String {
    let local = |t: SystemTime| -> Option<libc::tm> {
        let secs = t.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_secs() as libc::time_t;
        // SAFETY: localtime_r writes only into the tm it is handed.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        let ok = unsafe { !libc::localtime_r(&secs, &mut tm).is_null() };
        ok.then_some(tm)
    };
    let (Some(then), Some(now)) = (local(at), local(SystemTime::now())) else {
        return "at an unknown time".to_string();
    };
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let time = format!("{:02}:{:02}", then.tm_hour, then.tm_min);
    if (then.tm_year, then.tm_yday) == (now.tm_year, now.tm_yday) {
        time
    } else {
        format!(
            "{} {} {time}",
            MONTHS[then.tm_mon.clamp(0, 11) as usize],
            then.tm_mday
        )
    }
}
