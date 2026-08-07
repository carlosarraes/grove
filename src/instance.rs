//! One worktree's instance: its ports, its rendered config, its running services.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::{self, Config};
use crate::registry::{Entry, Registry};
use crate::render::{self, Context as RenderContext};
use crate::resolve::{self, Resolved};
use crate::supervise::{self};

pub struct Instance {
    pub resolved: Resolved,
    pub config: Config,
    pub entry: Entry,
    registry: Registry,
    state_dir: PathBuf,
}

pub struct ServiceStatus {
    pub name: String,
    pub running: bool,
}

/// Where treeish keeps the registry and per-service logs. `TREEISH_STATE_DIR` overrides
/// it, which is what lets the test suite run instances without touching real state.
pub fn state_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("TREEISH_STATE_DIR")
        && !dir.is_empty()
    {
        return Ok(PathBuf::from(dir));
    }
    let base = match std::env::var_os("XDG_STATE_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".local/state")
        }
    };
    Ok(base.join("treeish"))
}

pub fn registry() -> Result<Registry> {
    Ok(Registry::at(state_dir()?.join("registry.json")))
}

impl Instance {
    /// Resolve the worktree, load its config, and reserve its ports.
    pub fn open(cwd: &Path) -> Result<Self> {
        let resolved = resolve::resolve(cwd)?;
        let config = config::load(&resolved.worktree)?;
        let registry = registry()?;
        let entry = registry.reserve(&resolved, &config.ports.names)?;
        let state_dir = state_dir()?;
        Ok(Instance {
            resolved,
            config,
            entry,
            registry,
            state_dir,
        })
    }

    /// Mutating commands refuse in the main checkout: rendering there would overwrite the
    /// real `.env.local` that every instance reads from.
    pub fn refuse_in_main(&self) -> Result<()> {
        if self.resolved.is_main() {
            bail!(
                "this is the main worktree ({})\n\
                 treeish reads secrets from here, so it will not write over them. \
                 Run it from a linked worktree, or pass --allow-main if you are certain.",
                self.resolved.main_worktree.display()
            );
        }
        Ok(())
    }

    fn instance_dir(&self) -> PathBuf {
        self.state_dir
            .join(self.resolved.state_key())
            .join(&self.resolved.slug)
    }

    pub fn log_path(&self, service: &str) -> PathBuf {
        self.instance_dir().join(format!("{service}.log"))
    }

    pub fn db_name(&self) -> Option<String> {
        self.entry.db_name.clone()
    }

    fn render_context(&self) -> RenderContext {
        RenderContext {
            slug: self.resolved.slug.clone(),
            ports: self.entry.ports.clone(),
            db_name: self.entry.db_name.clone(),
            main_worktree: self.resolved.main_worktree.clone(),
        }
    }

    /// The variables treeish itself exports, on top of whatever the rendered env files
    /// carry. A command run through `treeish run` can address this instance without
    /// having to read a file to find its ports.
    pub fn environment(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::from([
            ("TREEISH_SLUG".to_string(), self.resolved.slug.clone()),
            (
                "TREEISH_WORKTREE".to_string(),
                self.resolved.worktree.display().to_string(),
            ),
        ]);
        for (name, port) in &self.entry.ports {
            env.insert(
                format!("TREEISH_PORT_{}", name.to_uppercase()),
                port.to_string(),
            );
        }
        if let Some(db) = &self.entry.db_name {
            env.insert("TREEISH_DB_NAME".to_string(), db.clone());
        }
        env
    }

    /// Resolve `db_name` from the first resource that declares one, then write every env
    /// file this repo needs.
    pub fn render(&mut self) -> Result<Vec<PathBuf>> {
        if let Some(template) = self
            .config
            .resources
            .iter()
            .find_map(|r| r.db_name.as_deref())
        {
            let bare = RenderContext {
                slug: self.resolved.slug.clone(),
                ports: self.entry.ports.clone(),
                db_name: None,
                main_worktree: self.resolved.main_worktree.clone(),
            };
            let name = render::value(template, &bare)
                .with_context(|| format!("rendering db_name {template:?}"))?;
            self.entry.db_name = Some(name);
            self.registry.record(&self.entry)?;
        }

        render::all(&self.config, &self.resolved, &self.render_context())
    }

    /// Bring up every declared datastore, reusing anything already answering on its port.
    pub fn resources(&self) -> Result<Vec<String>> {
        let mut started = Vec::new();
        for resource in &self.config.resources {
            if crate::resource::ensure(resource)? {
                started.push(resource.name.clone());
            }
        }
        Ok(started)
    }

    /// Drop this instance's database. Best effort: see `resource::drop_database`.
    pub fn purge_database(&self) -> Result<()> {
        let Some(database) = &self.entry.db_name else {
            return Ok(());
        };
        let Some(resource) = self.config.resources.iter().find(|r| r.db_name.is_some()) else {
            return Ok(());
        };
        crate::resource::drop_database(resource, database)
    }

    /// Start every service that is not already running, waiting for each readiness probe.
    pub fn up(&mut self, fresh: bool) -> Result<()> {
        let context = self.render_context();

        for service in &self.config.services {
            let already = self.entry.services.get(&service.name).copied();
            if let Some(handle) = already {
                if fresh {
                    supervise::stop(&handle)?;
                } else if supervise::is_alive(&handle) {
                    continue;
                }
            }

            let cwd = match &service.cwd {
                Some(dir) => self.resolved.worktree.join(dir),
                None => self.resolved.worktree.clone(),
            };
            let log = self.instance_dir().join(format!("{}.log", service.name));

            if let Some(setup) = &service.setup {
                self.run_setup(&service.name, setup, &cwd, &log)?;
            }

            let command = render::value(&service.command, &context)
                .with_context(|| format!("rendering the command for {}", service.name))?;
            let handle = supervise::spawn(&command, &cwd, &self.environment(), &log)?;
            self.entry.services.insert(service.name.clone(), handle);
            self.registry.record(&self.entry)?;

            if let Some(ready) = &service.ready {
                let url = render::value(&ready.http, &context)?;
                let timeout = parse_duration(&ready.timeout)?;
                supervise::wait_ready(&url, timeout).with_context(|| {
                    format!(
                        "{} never became ready\n{}",
                        service.name,
                        tail(&log, 30).unwrap_or_default()
                    )
                })?;
            }
        }

        Ok(())
    }

    /// Dependency installs run once per worktree, tracked by a marker beside the logs.
    fn run_setup(&self, name: &str, setup: &str, cwd: &Path, log: &Path) -> Result<()> {
        let marker = self.instance_dir().join(format!(".setup-{name}"));
        if marker.exists() {
            return Ok(());
        }
        eprintln!("first run in this worktree — {name}: {setup}");
        std::fs::create_dir_all(self.instance_dir())?;

        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(setup)
            .current_dir(cwd)
            .status()
            .with_context(|| format!("running setup for {name}: {setup}"))?;
        if !status.success() {
            bail!(
                "setup for {name} failed: {setup}\n{}",
                tail(log, 30).unwrap_or_default()
            );
        }
        std::fs::write(&marker, setup)?;
        Ok(())
    }

    pub fn down(&mut self) -> Result<()> {
        for handle in self.entry.services.values() {
            supervise::stop(handle)?;
        }
        self.entry.services.clear();
        self.registry.record(&self.entry)?;
        Ok(())
    }

    pub fn release(&self) -> Result<()> {
        self.registry.release(&self.resolved.worktree)
    }

    pub fn status(&self) -> Vec<ServiceStatus> {
        self.config
            .services
            .iter()
            .map(|s| ServiceStatus {
                name: s.name.clone(),
                running: self
                    .entry
                    .services
                    .get(&s.name)
                    .is_some_and(supervise::is_alive),
            })
            .collect()
    }
}

/// The last `lines` lines of a log, for pasting into an error. A failure an agent cannot
/// diagnose from the message costs it a round trip and a chunk of context.
pub fn tail(path: &Path, lines: usize) -> Option<String> {
    let body = std::fs::read_to_string(path).ok()?;
    let collected: Vec<&str> = body.lines().collect();
    let start = collected.len().saturating_sub(lines);
    Some(collected[start..].join("\n"))
}

fn parse_duration(text: &str) -> Result<std::time::Duration> {
    let trimmed = text.trim();
    let (value, multiplier) = match trimmed.strip_suffix("ms") {
        Some(v) => (v, 1u64),
        None => match trimmed.strip_suffix('s') {
            Some(v) => (v, 1000),
            None => match trimmed.strip_suffix('m') {
                Some(v) => (v, 60_000),
                None => (trimmed, 1000),
            },
        },
    };
    let amount: u64 = value
        .trim()
        .parse()
        .with_context(|| format!("{text:?} is not a duration like \"180s\""))?;
    Ok(std::time::Duration::from_millis(amount * multiplier))
}
