//! Which worktree holds which ports, persisted across runs and across processes.
//!
//! `ports::allocate` reads live machine state, so on its own it cannot promise an
//! instance the same ports twice. Writing the decision down is what makes it stable, and
//! the file lock is what keeps two simultaneous `grove up` calls from picking the same
//! block before either has started listening.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use crate::ports;
use crate::resolve::Resolved;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub worktree: PathBuf,
    pub slug: String,
    pub ports: BTreeMap<String, u16>,
    /// Live services, by name. Persisted because `down` runs in a different process than
    /// the `up` that started them.
    #[serde(default)]
    pub services: BTreeMap<String, crate::supervise::Handle>,
    #[serde(default)]
    pub db_name: Option<String>,
    /// Unix seconds at the last command that meant someone was working here. Optional
    /// because entries written before this existed have no answer, and guessing one would
    /// make every instance on an upgraded machine look abandoned.
    #[serde(default)]
    pub last_used: Option<u64>,
    /// Where this instance's service logs live. Persisted because it derives from
    /// `Resolved::state_key`, which hangs off the main worktree — a path `ls` has no other
    /// way to reach, since it reads the registry and never resolves a worktree.
    #[serde(default)]
    pub instance_dir: Option<PathBuf>,
}

impl Entry {
    /// Seconds since the last evidence that anyone was using this instance, or None when
    /// there is no evidence either way.
    ///
    /// Two sources, because neither is sufficient alone. `last_used` misses an agent
    /// forty minutes into browser-driven QA, which issues no grove commands while being
    /// maximally busy; the service logs catch exactly that, because a backend serving it
    /// is writing request lines the whole time.
    ///
    /// It errs toward looking busy — a chatty reload watcher keeps an idle instance off
    /// the list. For a number that decides what gets killed, that is the right direction
    /// to be wrong in.
    pub fn idle_seconds(&self, now: u64) -> Option<u64> {
        let logged = self
            .services
            .keys()
            .filter_map(|name| self.log_touched(name))
            .max();
        let freshest = self.last_used.into_iter().chain(logged).max()?;
        Some(now.saturating_sub(freshest))
    }

    fn log_touched(&self, service: &str) -> Option<u64> {
        let dir = self.instance_dir.as_ref()?;
        dir.join(format!("{service}.log"))
            .metadata()
            .ok()?
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs())
    }

    /// Any service still answering. A recorded pid says only that something started once.
    pub fn is_running(&self) -> bool {
        self.services.values().any(crate::supervise::is_alive)
    }
}

/// An age as a person would say it, for a column read at a glance while deciding what to
/// stop. Two units at most: "1h30m" is worth the extra word, "1h30m12s" is not.
pub fn human_age(seconds: u64) -> String {
    let (days, hours) = (seconds / 86_400, seconds % 86_400 / 3600);
    let (minutes, secs) = (seconds % 3600 / 60, seconds % 60);
    match (days, hours, minutes) {
        (0, 0, 0) => format!("{secs}s"),
        (0, 0, _) => format!("{minutes}m"),
        (0, _, 0) => format!("{hours}h"),
        (0, _, _) => format!("{hours}h{minutes}m"),
        (_, 0, _) => format!("{days}d"),
        (_, _, _) => format!("{days}d{hours}h"),
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    instances: BTreeMap<String, Entry>,
}

pub struct Registry {
    path: PathBuf,
}

impl Registry {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Registry { path: path.into() }
    }

    /// Default location: `$XDG_STATE_HOME/grove/registry.json`.
    pub fn default_path() -> Result<PathBuf> {
        let base = match std::env::var_os("XDG_STATE_HOME") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => {
                let home = std::env::var_os("HOME").context("HOME is not set")?;
                PathBuf::from(home).join(".local/state")
            }
        };
        Ok(base.join("grove/registry.json"))
    }

    /// Ports for this worktree, allocating a block the first time and returning the same
    /// block every time after.
    pub fn reserve(&self, resolved: &Resolved, names: &[String]) -> Result<Entry> {
        self.with_lock(|state| {
            let key = key(&resolved.worktree);

            if let Some(existing) = state.instances.get(&key)
                && names.iter().all(|n| existing.ports.contains_key(n))
            {
                return Ok(existing.clone());
            }

            // Everything another instance already holds, whether or not it is listening
            // yet — a reserved-but-not-started block is invisible to a bind test.
            let taken: HashSet<u16> = state
                .instances
                .iter()
                .filter(|(k, _)| *k != &key)
                .flat_map(|(_, e)| e.ports.values().copied())
                .collect();

            let entry = Entry {
                worktree: resolved.worktree.clone(),
                slug: resolved.slug.clone(),
                ports: ports::allocate_avoiding(
                    &resolved.state_key(),
                    &resolved.slug,
                    names,
                    &taken,
                )?,
                services: BTreeMap::new(),
                db_name: None,
                last_used: None,
                instance_dir: None,
            };
            state.instances.insert(key, entry.clone());
            Ok(entry)
        })
    }

    /// Persist an entry that changed — new service pids, a resolved database name.
    pub fn record(&self, entry: &Entry) -> Result<()> {
        self.with_lock(|state| {
            let mut entry = entry.clone();
            // The idle clock only ever moves forward. Callers hold an `Entry` read before
            // they started working, and writing it back verbatim would roll the touch
            // that command just made back to whatever it was minutes earlier.
            if let Some(stored) = state.instances.get(&key(&entry.worktree)) {
                entry.last_used = entry.last_used.max(stored.last_used);
            }
            state.instances.insert(key(&entry.worktree), entry);
            Ok(())
        })
    }

    /// Mark this instance as worked in. Separate from `record` because it must not
    /// overwrite service pids a concurrent `up` has just written.
    pub fn touch(&self, worktree: &Path) -> Result<()> {
        self.with_lock(|state| {
            if let Some(entry) = state.instances.get_mut(&key(worktree)) {
                entry.last_used = Some(now());
            }
            Ok(())
        })
    }

    pub fn get(&self, worktree: &Path) -> Result<Option<Entry>> {
        self.with_lock(|state| Ok(state.instances.get(&key(worktree)).cloned()))
    }

    pub fn list(&self) -> Result<Vec<Entry>> {
        self.with_lock(|state| Ok(state.instances.values().cloned().collect()))
    }

    pub fn release(&self, worktree: &Path) -> Result<()> {
        self.with_lock(|state| {
            state.instances.remove(&key(worktree));
            Ok(())
        })
    }

    /// Drop entries whose worktree is gone, so a deleted worktree stops holding ports.
    /// Returns what was dropped, so callers can report it rather than reclaiming silently.
    pub fn reap(&self) -> Result<Vec<Entry>> {
        self.with_lock(|state| {
            let gone: Vec<String> = state
                .instances
                .iter()
                .filter(|(_, e)| !e.worktree.exists())
                .map(|(k, _)| k.clone())
                .collect();
            Ok(gone
                .into_iter()
                .filter_map(|k| state.instances.remove(&k))
                .collect())
        })
    }

    /// Read-modify-write under an exclusive lock on a sidecar file. The lock is a
    /// separate file because the registry itself is replaced by rename, which would
    /// otherwise hand the next writer a lock on an unlinked inode.
    fn with_lock<T>(&self, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        let lock_path = self.path.with_extension("lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        lock.lock()
            .with_context(|| format!("locking {}", lock_path.display()))?;

        let result = (|| {
            let mut state = self.read()?;
            let out = f(&mut state)?;
            self.write(&state)?;
            Ok(out)
        })();

        let _ = lock.unlock();
        result
    }

    fn read(&self) -> Result<State> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) if text.trim().is_empty() => Ok(State::default()),
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    fn write(&self, state: &State) -> Result<()> {
        let text = serde_json::to_string_pretty(state)?;
        let temporary = self.path.with_extension("json.new");
        std::fs::write(&temporary, text)
            .with_context(|| format!("writing {}", temporary.display()))?;
        std::fs::rename(&temporary, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
    }
}

fn key(worktree: &Path) -> String {
    worktree.to_string_lossy().into_owned()
}
