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

pub enum SeedOutcome {
    Ran { name: String },
    Skipped { name: String, why: String },
}

impl std::fmt::Display for SeedOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SeedOutcome::Ran { name } => write!(f, "seed {name} ... ok"),
            SeedOutcome::Skipped { name, why } => write!(f, "seed {name} ... skipped ({why})"),
        }
    }
}

/// Where grove keeps the registry and per-service logs. `GROVE_STATE_DIR` overrides
/// it, which is what lets the test suite run instances without touching real state.
pub fn state_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("GROVE_STATE_DIR")
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
    Ok(base.join("grove"))
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
                 grove reads secrets from here, so it will not write over them. \
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

    /// The environment a service or `grove run` command receives.
    ///
    /// Beyond grove's own variables this carries every `[secrets.set]` override, because
    /// writing them to a file is not enough: code that reads the process environment
    /// before its settings library loads the file sees the wrong value and says so
    /// confusingly — a backend logging "ENVIRONMENT not set, defaulting to production"
    /// with `ENVIRONMENT=test` sitting in the file grove just wrote.
    pub fn environment(&self) -> Result<BTreeMap<String, String>> {
        let mut env = BTreeMap::from([
            ("GROVE_SLUG".to_string(), self.resolved.slug.clone()),
            (
                "GROVE_WORKTREE".to_string(),
                self.resolved.worktree.display().to_string(),
            ),
        ]);
        for (name, port) in &self.entry.ports {
            env.insert(
                format!("GROVE_PORT_{}", name.to_uppercase()),
                port.to_string(),
            );
        }
        if let Some(db) = &self.entry.db_name {
            env.insert("GROVE_DB_NAME".to_string(), db.clone());
        }

        // Browser automation defaults to one shared session per machine, so parallel
        // instances steal each other's tab — and the resulting error page looks like a
        // bug in whichever instance was watching. An explicit setting still wins.
        if std::env::var_os("AGENT_BROWSER_SESSION").is_none() {
            env.insert(
                "AGENT_BROWSER_SESSION".to_string(),
                self.resolved.slug.clone(),
            );
        }

        let context = self.render_context();
        for secrets in &self.config.secrets {
            for (key, template) in &secrets.set {
                let value = render::value(template, &context)
                    .with_context(|| format!("rendering {key} for the environment"))?;
                // Two files setting one key to different values is ambiguous, and picking
                // silently would hand a service the wrong one.
                if let Some(existing) = env.get(key)
                    && existing != &value
                {
                    bail!(
                        "{key} is set to two different values by different [[secrets]] \
                         blocks ({existing:?} and {value:?}); give them distinct names or \
                         the same value"
                    );
                }
                env.insert(key.clone(), value);
            }
        }

        Ok(env)
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

    /// Populate this instance's datastore. Each seed runs once per worktree unless
    /// `force`, and reports what it did — a seed that silently skipped is how you end up
    /// debugging a 403 that names authentication rather than missing data.
    pub fn seed(&self, force: bool) -> Result<Vec<SeedOutcome>> {
        let mut outcomes = Vec::new();
        let environment = self.environment()?;

        for seed in &self.config.seeds {
            let cwd = match &seed.cwd {
                Some(dir) => self.resolved.worktree.join(dir),
                None => self.resolved.worktree.clone(),
            };

            if let Some(guard) = &seed.if_exists
                && !cwd.join(guard).exists()
            {
                outcomes.push(SeedOutcome::Skipped {
                    name: seed.name.clone(),
                    why: format!("{guard} not present"),
                });
                continue;
            }

            let marker = self.instance_dir().join(format!(".seed-{}", seed.name));
            if marker.exists() && !force {
                outcomes.push(SeedOutcome::Skipped {
                    name: seed.name.clone(),
                    why: "already seeded".to_string(),
                });
                continue;
            }

            std::fs::create_dir_all(self.instance_dir())?;
            let log = self.instance_dir().join(format!("seed-{}.log", seed.name));
            let sink = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log)
                .with_context(|| format!("opening {}", log.display()))?;
            let errors = sink.try_clone().context("duplicating the log handle")?;

            let command = render::value(&seed.command, &self.render_context())
                .with_context(|| format!("rendering the command for seed {}", seed.name))?;
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(&command)
                .current_dir(&cwd)
                .envs(&environment)
                .stdout(std::process::Stdio::from(sink))
                .stderr(std::process::Stdio::from(errors))
                .status()
                .with_context(|| format!("running seed {}: {command}", seed.name))?;

            if !status.success() {
                bail!(
                    "seed {} failed: {command}\nfull output in {}\n{}",
                    seed.name,
                    log.display(),
                    tail(&log, 30).unwrap_or_default()
                );
            }
            std::fs::write(&marker, &command)?;
            outcomes.push(SeedOutcome::Ran {
                name: seed.name.clone(),
            });
        }

        Ok(outcomes)
    }

    /// Start every service that is not already running, waiting for each readiness probe.
    ///
    /// Dependencies first, then seeds, then the services — a seed that writes straight to
    /// the datastore needs the dependencies but not a listening server.
    pub fn up(&mut self, fresh: bool) -> Result<Vec<SeedOutcome>> {
        for service in &self.config.services {
            if let Some(setup) = &service.setup {
                let cwd = match &service.cwd {
                    Some(dir) => self.resolved.worktree.join(dir),
                    None => self.resolved.worktree.clone(),
                };
                let log = self.instance_dir().join(format!("{}.log", service.name));
                self.run_setup(&service.name, setup, &cwd, &log)?;
            }
        }
        let seeded = self.seed(false)?;

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

            let command = render::value(&service.command, &context)
                .with_context(|| format!("rendering the command for {}", service.name))?;
            let environment = self.environment()?;
            let handle = supervise::spawn(&command, &cwd, &environment, &log)?;
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

        Ok(seeded)
    }

    /// Dependency installs run once per worktree, tracked by a marker beside the logs.
    fn run_setup(&self, name: &str, setup: &str, cwd: &Path, log: &Path) -> Result<()> {
        let marker = self.instance_dir().join(format!(".setup-{name}"));
        if marker.exists() {
            return Ok(());
        }
        std::fs::create_dir_all(self.instance_dir())?;
        eprintln!("{name}: installing dependencies ({setup})");

        // Setup output goes to the service log rather than the terminal. `npm ci` and
        // `uv sync` between them emit hundreds of lines, and the port summary printed
        // afterwards is the one thing the caller actually needs to read.
        let sink = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .with_context(|| format!("opening {}", log.display()))?;
        let errors = sink.try_clone().context("duplicating the log handle")?;

        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(setup)
            .current_dir(cwd)
            .stdout(std::process::Stdio::from(sink))
            .stderr(std::process::Stdio::from(errors))
            .status()
            .with_context(|| format!("running setup for {name}: {setup}"))?;
        if !status.success() {
            bail!(
                "setup for {name} failed: {setup}\n\
                 full output in {}\n{}",
                log.display(),
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
