//! Doctor for the machine: everything on it that is costing someone, and what ends it.
//!
//! Read-only on purpose. The reader is usually deciding whether the machine is the
//! problem, and a report that changes the machine while being read is one nobody can
//! reason from. Every FAIL carries the command that fixes it, the way `doctor` does.

use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::doctor::Verdict;
use crate::footprint::human_size;
use crate::registry::{Entry, human_age};
use crate::{load, ports, registry, store};

/// The window past which a running instance counts as forgotten. Generous on purpose:
/// an instance someone stepped away from for lunch has to survive it, because whoever
/// reads the report cannot tell a forgotten box from a colleague's.
pub const IDLE_WINDOW: &str = "2h";

pub fn check(cwd: &Path) -> Result<Vec<Verdict>> {
    let entries = crate::instance::registry()?.list()?;
    let now = registry::now();
    let mut verdicts = listeners(&entries);
    verdicts.push(orphans(&entries));
    verdicts.push(disk(cwd, &entries));
    verdicts.push(idle(&entries, now)?);
    verdicts.extend(store_state(&entries)?);
    verdicts.extend(unmanaged(cwd, &entries));
    verdicts.push(memory());
    verdicts.push(machine(&entries));
    Ok(verdicts)
}

/// The finding this command exists for: a listener on a grove port with no running
/// instance behind it — a `down` that missed a child, a server from an older grove, a
/// squatter. grove cannot stop it, because no handle it holds names that process; so the
/// report names it, and the command that ends it.
fn listeners(entries: &[Entry]) -> Vec<Verdict> {
    let running: HashSet<u16> = entries
        .iter()
        .filter(|e| e.is_running())
        .flat_map(|e| e.ports.values().copied())
        .collect();
    let reserved: BTreeMap<u16, &Entry> = entries
        .iter()
        .flat_map(|e| e.ports.values().map(move |p| (*p, e)))
        .collect();
    // The shared datastore listens inside the range on every machine, and it is nobody's
    // stray. Each instance records where its database lives, so no config is needed to
    // tell a datastore from a server a `down` missed.
    let datastores: BTreeMap<u16, &str> = entries
        .iter()
        .filter_map(|e| e.db_resource.as_ref())
        .map(|db| (db.port, db.name.as_str()))
        .collect();
    let mut verdicts: Vec<Verdict> = datastores
        .iter()
        .map(|(port, name)| {
            if ports::is_free(*port) {
                Verdict::Warn(format!(
                    "datastore {name} recorded on port {port} is not answering; grove up starts it"
                ))
            } else {
                Verdict::Ok(format!("datastore {name} listening on port {port}"))
            }
        })
        .collect();
    let range = ports::range();
    let strays: Vec<u16> = range
        .clone()
        .filter(|port| {
            !running.contains(port) && !datastores.contains_key(port) && !ports::is_free(*port)
        })
        .collect();
    if strays.is_empty() {
        verdicts.push(Verdict::Ok(format!(
            "nothing listens on {}..{} without a running instance",
            range.start, range.end
        )));
        return verdicts;
    }
    verdicts.extend(strays.into_iter().map(|port| {
        let owner = listener(port);
        let who = match &owner {
            Some(o) => format!(" (pid {}, {})", o.pid, o.command),
            None => String::new(),
        };
        let reserved_by = match reserved.get(&port) {
            Some(e) if !e.worktree.exists() => {
                format!("; reserved by {}, whose worktree is gone", e.slug)
            }
            Some(e) => format!("; reserved by {} (stopped)", e.slug),
            None => "; not a port grove reserved".to_string(),
        };
        let fix = match &owner {
            Some(o) => format!(
                "kill -TERM -{}   # its process group; no grove handle reaches it",
                o.pgid
            ),
            None => format!("lsof -iTCP:{port} -sTCP:LISTEN   # names the process"),
        };
        Verdict::Fail {
            what: format!(
                "port {port} is listening{who} but no instance is running there{reserved_by}"
            ),
            fix,
        }
    }));
    verdicts
}

struct Listener {
    pid: u32,
    pgid: u32,
    command: String,
}

/// Who holds a port, via lsof and ps — both on macOS and Linux — or None when neither
/// answers, which the caller reports as such rather than guessing.
fn listener(port: u16) -> Option<Listener> {
    let out = Command::new("lsof")
        .args(["-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
        .output()
        .ok()?;
    let pid: u32 = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()?;
    let ps = Command::new("ps")
        .args(["-o", "pgid=,command=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&ps.stdout);
    let (pgid, command) = line.trim().split_once(char::is_whitespace)?;
    Some(Listener {
        pid,
        pgid: pgid.trim().parse().ok()?,
        command: command.trim().chars().take(60).collect(),
    })
}

fn orphans(entries: &[Entry]) -> Verdict {
    let gone: Vec<&str> = entries
        .iter()
        .filter(|e| !e.worktree.exists())
        .map(|e| e.slug.as_str())
        .collect();
    if gone.is_empty() {
        return Verdict::Ok("no orphaned instances".to_string());
    }
    Verdict::Fail {
        what: format!(
            "{} orphaned instance{}: {} — their worktrees are gone and their ports stay reserved",
            gone.len(),
            if gone.len() == 1 { "" } else { "s" },
            some_of(&gone)
        ),
        fix: "grove prune".to_string(),
    }
}

/// Free space on the volume the caller is standing on, alongside what grove's own
/// instances hold there. Load says nothing about disk, and a full disk is the other way a
/// crowd costs someone.
fn disk(cwd: &Path, entries: &[Entry]) -> Verdict {
    let held: u64 = entries
        .iter()
        .filter(|e| e.worktree.exists())
        .filter_map(|e| e.disk_bytes)
        .sum();
    let Some((total, free)) = df(cwd) else {
        return Verdict::Warn(format!(
            "could not read free space for {}; grove's instances hold {} in dependencies",
            cwd.display(),
            human_size(held)
        ));
    };
    let percent = (free * 100).checked_div(total).unwrap_or(100);
    let store = store::dir()
        .ok()
        .filter(|d| d.exists())
        .map(|d| crate::footprint::tree_size(&d));
    let what = format!(
        "{} free of {} ({percent}%) on the volume holding {}; instances hold {} in private dependencies, the store {}",
        human_size(free),
        human_size(total),
        cwd.display(),
        human_size(held),
        store
            .map(human_size)
            .unwrap_or_else(|| "nothing".to_string())
    );
    if percent < 10 {
        Verdict::Fail {
            what,
            fix: "git worktree remove finished worktrees, then grove prune; grove ls shows what each instance holds".to_string(),
        }
    } else if percent < 20 {
        Verdict::Warn(what)
    } else {
        Verdict::Ok(what)
    }
}

/// (total, free) in bytes from `df -kP`, the one output shape both platforms share.
fn df(path: &Path) -> Option<(u64, u64)> {
    let out = Command::new("df").arg("-kP").arg(path).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let fields: Vec<&str> = text.lines().nth(1)?.split_whitespace().collect();
    let total: u64 = fields.get(1)?.parse().ok()?;
    let free: u64 = fields.get(3)?.parse().ok()?;
    Some((total * 1024, free * 1024))
}

fn idle(entries: &[Entry], now: u64) -> Result<Verdict> {
    let window = crate::instance::parse_duration(IDLE_WINDOW)?.as_secs();
    let mut stale: Vec<(String, u64)> = entries
        .iter()
        .filter(|e| e.is_running())
        .filter_map(|e| Some((e.slug.clone(), e.idle_seconds(now)?)))
        .filter(|(_, age)| *age >= window)
        .collect();
    stale.sort_by_key(|(_, age)| std::cmp::Reverse(*age));
    if stale.is_empty() {
        return Ok(Verdict::Ok(format!(
            "no running instance idle over {IDLE_WINDOW}"
        )));
    }
    let named: Vec<String> = stale
        .iter()
        .map(|(slug, age)| format!("{slug} ({})", human_age(*age)))
        .collect();
    let named: Vec<&str> = named.iter().map(String::as_str).collect();
    Ok(Verdict::Warn(format!(
        "{} running instance{} idle over {IDLE_WINDOW}: {}; grove down --idle {IDLE_WINDOW} stops them, keeping their ports",
        stale.len(),
        if stale.len() == 1 { "" } else { "s" },
        some_of(&named)
    )))
}

fn store_state(entries: &[Entry]) -> Result<Vec<Verdict>> {
    let dir = store::dir()?;
    let count = std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
    let referenced: HashSet<String> = entries
        .iter()
        .filter(|e| e.worktree.exists())
        .flat_map(|e| e.cache_keys.values().cloned())
        .collect();
    let stray = store::unreferenced(&dir, &referenced)
        .with_context(|| format!("reading the store at {}", dir.display()))?;
    let mut verdicts = vec![Verdict::Ok(format!(
        "store holds {count} {} at {}",
        if count == 1 { "entry" } else { "entries" },
        dir.display()
    ))];
    if !stray.is_empty() {
        verdicts.push(Verdict::Warn(format!(
            "{} store {} no worktree references; grove prune removes them",
            stray.len(),
            if stray.len() == 1 { "entry" } else { "entries" }
        )));
    }
    Ok(verdicts)
}

/// Worktrees of the repo the caller is standing in that never ran `grove up`. They hold
/// whatever was installed by hand, which nothing in this report can see or share.
/// Skipped outside a repository, since there is no repo to ask.
fn unmanaged(cwd: &Path, entries: &[Entry]) -> Option<Verdict> {
    let resolved = crate::resolve::resolve(cwd).ok()?;
    let out = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(&resolved.main_worktree)
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let known: HashSet<&Path> = entries.iter().map(|e| e.worktree.as_path()).collect();
    let listed = String::from_utf8_lossy(&out.stdout);
    let strangers: Vec<PathBuf> = listed
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .map(PathBuf::from)
        .filter(|p| *p != resolved.main_worktree && p.exists() && !known.contains(p.as_path()))
        .collect();
    let main = resolved.main_worktree.display();
    Some(if strangers.is_empty() {
        Verdict::Ok(format!("every worktree of {main} has a grove instance"))
    } else {
        Verdict::Warn(format!(
            "{} worktree{} of {main} never ran grove up, so nothing here sees what they hold",
            strangers.len(),
            if strangers.len() == 1 { "" } else { "s" }
        ))
    })
}

/// Memory availability and swap usage, with the kernel pressure level on macOS.
pub struct Memory {
    pub free_percent: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    /// macOS reports 1 (normal), 2 (warning), or 4 (critical). Linux has no value here.
    pub pressure_level: Option<u32>,
}

impl Memory {
    pub fn sample() -> Option<Memory> {
        if cfg!(target_os = "macos") {
            let pressure = Command::new("memory_pressure").output().ok()?;
            let swap = Command::new("sysctl").arg("vm.swapusage").output().ok()?;
            let level = Command::new("sysctl")
                .args(["-n", "kern.memorystatus_vm_pressure_level"])
                .output()
                .ok()?;
            Memory::from_macos(
                &String::from_utf8_lossy(&pressure.stdout),
                &String::from_utf8_lossy(&swap.stdout),
                &String::from_utf8_lossy(&level.stdout),
            )
        } else {
            Memory::from_meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)
        }
    }

    /// `memory_pressure`'s last line and `sysctl vm.swapusage`, as printed.
    pub fn from_macos(pressure: &str, swap: &str, level: &str) -> Option<Memory> {
        let pressure_level = match level.trim().parse().ok()? {
            level @ (1 | 2 | 4) => level,
            _ => return None,
        };
        let free_percent = pressure
            .lines()
            .find_map(|l| l.strip_prefix("System-wide memory free percentage:"))?
            .trim()
            .trim_end_matches('%')
            .parse()
            .ok()?;
        let field = |name: &str| -> Option<u64> {
            let rest = swap.split(&format!("{name} = ")).nth(1)?;
            let token = rest.split_whitespace().next()?;
            let (number, unit) = token.split_at(token.len() - 1);
            let scale: f64 = match unit {
                "K" => 1024.0,
                "M" => 1024.0 * 1024.0,
                "G" => 1024.0 * 1024.0 * 1024.0,
                _ => return None,
            };
            Some((number.parse::<f64>().ok()? * scale) as u64)
        };
        Some(Memory {
            free_percent,
            swap_used: field("used")?,
            swap_total: field("total")?,
            pressure_level: Some(pressure_level),
        })
    }

    /// `/proc/meminfo`. MemAvailable rather than MemFree, for the same reason as above.
    pub fn from_meminfo(text: &str) -> Option<Memory> {
        let kb = |name: &str| -> Option<u64> {
            text.lines()
                .find_map(|l| l.strip_prefix(name))?
                .trim()
                .trim_start_matches(':')
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        };
        let total = kb("MemTotal")?;
        let available = kb("MemAvailable")?;
        let swap_total = kb("SwapTotal")?;
        let swap_free = kb("SwapFree")?;
        Some(Memory {
            free_percent: (available * 100).checked_div(total)?,
            swap_used: swap_total.saturating_sub(swap_free) * 1024,
            swap_total: swap_total * 1024,
            pressure_level: None,
        })
    }

    fn swap_percent(&self) -> Option<u64> {
        (self.swap_used * 100).checked_div(self.swap_total)
    }

    /// macOS uses kernel pressure because its allocated swap can grow.
    /// Linux uses its configured swap capacity, or available memory without swap.
    pub fn exhausted(&self) -> bool {
        if let Some(level) = self.pressure_level {
            return level == 4;
        }
        match self.swap_percent() {
            Some(used) => used >= 90,
            None => self.free_percent < 5,
        }
    }

    fn strained(&self) -> bool {
        if let Some(level) = self.pressure_level {
            return level >= 2;
        }
        self.swap_percent().is_some_and(|used| used >= 70) || self.free_percent < 10
    }
}

fn memory() -> Verdict {
    let Some(m) = Memory::sample() else {
        return Verdict::Warn("could not read memory and swap on this platform".to_string());
    };
    let what = format!(
        "memory {}% free, swap {} of {} used",
        m.free_percent,
        human_size(m.swap_used),
        human_size(m.swap_total)
    );
    if m.exhausted() {
        Verdict::Fail {
            what: format!("{what} — memory pressure is critical"),
            fix:
                "grove down --idle 2h   # idle dev servers with file watchers are the usual weight"
                    .to_string(),
        }
    } else if m.strained() {
        Verdict::Warn(what)
    } else {
        Verdict::Ok(what)
    }
}

fn machine(entries: &[Entry]) -> Verdict {
    let running = entries.iter().filter(|e| e.is_running()).count();
    let load = load::sample();
    let headline = match &load {
        Some(l) => format!(
            "load {:.1} on {} cores, {running} instances running",
            l.one, l.cores
        ),
        None => format!("{running} instances running"),
    };
    if load::should_warn(load.as_ref(), running) {
        Verdict::Warn(format!(
            "{headline} — oversubscribed; grove ls names what to stop"
        ))
    } else {
        Verdict::Ok(headline)
    }
}

/// The first few names and a count for the rest: a report is read at a glance, and a
/// line of forty slugs is a line nobody reads.
fn some_of(names: &[&str]) -> String {
    const SHOWN: usize = 6;
    if names.len() <= SHOWN {
        return names.join(", ");
    }
    format!(
        "{}, and {} more",
        names[..SHOWN].join(", "),
        names.len() - SHOWN
    )
}

/// The same findings for an agent deciding in code: level, what, and the fix where
/// there is one.
pub fn json(verdicts: &[Verdict]) -> serde_json::Value {
    let findings: Vec<serde_json::Value> = verdicts
        .iter()
        .map(|v| match v {
            Verdict::Ok(what) => serde_json::json!({ "level": "ok", "what": what, "fix": null }),
            Verdict::Warn(what) => {
                serde_json::json!({ "level": "warn", "what": what, "fix": null })
            }
            Verdict::Fail { what, fix } => {
                serde_json::json!({ "level": "fail", "what": what, "fix": fix })
            }
        })
        .collect();
    serde_json::Value::Array(findings)
}
