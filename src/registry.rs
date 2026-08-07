//! Which worktree holds which ports, persisted across runs and across processes.
//!
//! `ports::allocate` reads live machine state, so on its own it cannot promise an
//! instance the same ports twice. Writing the decision down is what makes it stable, and
//! the file lock is what keeps two simultaneous `treeish up` calls from picking the same
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

    /// Default location: `$XDG_STATE_HOME/treeish/registry.json`.
    pub fn default_path() -> Result<PathBuf> {
        let base = match std::env::var_os("XDG_STATE_HOME") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => {
                let home = std::env::var_os("HOME").context("HOME is not set")?;
                PathBuf::from(home).join(".local/state")
            }
        };
        Ok(base.join("treeish/registry.json"))
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
            };
            state.instances.insert(key, entry.clone());
            Ok(entry)
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
