//! One worktree's instance: its ports, its rendered config, its running services.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
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
    recently_started_resources: BTreeSet<String>,
}

/// Whether the service's declared endpoint is actually being served. Distinct from
/// `running`, which only says a process survives: the two disagree exactly when it matters
/// most — a backend whose datastore died keeps its process and stops answering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    Answering,
    Silent,
    /// No `ready.http` declared, so grove has nothing to ask. Never report this as
    /// answering: an unasked question is not a passed check.
    Undeclared,
}

impl Readiness {
    pub fn as_str(self) -> &'static str {
        match self {
            Readiness::Answering => "answering",
            Readiness::Silent => "silent",
            Readiness::Undeclared => "undeclared",
        }
    }
}

pub struct ServiceStatus {
    pub name: String,
    pub running: bool,
    pub pid: Option<u32>,
    pub ready: Readiness,
    /// The rendered `ready.http`, so a reader told "not answering" can try it themselves.
    pub url: Option<String>,
}

pub enum SeedOutcome {
    Ran { name: String, why: Option<String> },
    Skipped { name: String, why: String },
}

const SEED_MARKER_VERSION: u8 = 1;

/// Long enough for a loaded machine to answer, short enough that `status` stays a command
/// you run without thinking. Only reached when something accepts the connection and then
/// says nothing; a closed port refuses instantly.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Deserialize, Serialize)]
struct SeedMarker {
    version: u8,
    command: String,
    #[serde(default)]
    resources: BTreeMap<String, String>,
}

enum StoredSeedMarker {
    Structured(SeedMarker),
    Legacy(String),
    Invalid,
}

enum SeedDecision {
    Run { why: Option<String> },
    Skip,
}

/// How one blocking step of `up` names itself while it runs and when it fails.
struct Step<'a> {
    service: &'a str,
    kind: &'a str,
    doing: &'a str,
    done: &'a str,
}

fn read_seed_marker(path: &Path) -> Result<Option<StoredSeedMarker>> {
    let body = match std::fs::read_to_string(path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    if !body.trim_start().starts_with('{') {
        return Ok(Some(StoredSeedMarker::Legacy(body)));
    }
    let stored = match serde_json::from_str::<SeedMarker>(&body) {
        Ok(marker) if marker.version == SEED_MARKER_VERSION => StoredSeedMarker::Structured(marker),
        Ok(_) | Err(_) => StoredSeedMarker::Invalid,
    };
    Ok(Some(stored))
}

fn seed_decision(
    stored: Option<&StoredSeedMarker>,
    command: &str,
    current_resources: &BTreeMap<String, String>,
    recently_started: &BTreeSet<String>,
    force: bool,
) -> SeedDecision {
    if force {
        return SeedDecision::Run { why: None };
    }
    match stored {
        None => SeedDecision::Run { why: None },
        Some(StoredSeedMarker::Invalid) => SeedDecision::Run {
            why: Some("seed marker was invalid".to_string()),
        },
        Some(StoredSeedMarker::Legacy(previous)) => {
            if previous != command {
                return SeedDecision::Run {
                    why: Some("command changed".to_string()),
                };
            }
            match recently_started.iter().next() {
                Some(name) => SeedDecision::Run {
                    why: Some(format!("resource {name} was recreated")),
                },
                None => SeedDecision::Skip,
            }
        }
        Some(StoredSeedMarker::Structured(previous)) => {
            if previous.command != command {
                return SeedDecision::Run {
                    why: Some("command changed".to_string()),
                };
            }
            if let Some(name) = recently_started.iter().next() {
                return SeedDecision::Run {
                    why: Some(format!("resource {name} was recreated")),
                };
            }
            for (name, id) in current_resources {
                if previous.resources.get(name) != Some(id) {
                    return SeedDecision::Run {
                        why: Some(format!("resource {name} was recreated")),
                    };
                }
            }
            SeedDecision::Skip
        }
    }
}

fn merged_resources(
    stored: Option<&StoredSeedMarker>,
    current: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut resources = match stored {
        Some(StoredSeedMarker::Structured(marker)) => marker.resources.clone(),
        _ => BTreeMap::new(),
    };
    resources.extend(current.clone());
    resources
}

fn write_seed_marker(
    path: &Path,
    command: &str,
    resources: BTreeMap<String, String>,
) -> Result<()> {
    let marker = SeedMarker {
        version: SEED_MARKER_VERSION,
        command: command.to_string(),
        resources,
    };
    let mut body = serde_json::to_string_pretty(&marker)?;
    body.push('\n');
    std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))
}

impl std::fmt::Display for SeedOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SeedOutcome::Ran { name, why: None } => write!(f, "seed {name} ... ok"),
            SeedOutcome::Ran {
                name,
                why: Some(why),
            } => write!(f, "seed {name} ... ok ({why})"),
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
        let mut instance = Instance {
            resolved,
            config,
            entry,
            registry,
            state_dir,
            recently_started_resources: BTreeSet::new(),
        };

        // Recorded here because `ls` reads the registry without resolving a worktree, and
        // this path derives from the main worktree — which the registry does not store.
        // Written once and then left alone, so the common case is a lock-free read.
        let dir = instance.instance_dir();
        if instance.entry.instance_dir.as_deref() != Some(dir.as_path()) {
            instance.entry.instance_dir = Some(dir);
            instance.registry.record(&instance.entry)?;
        }

        Ok(instance)
    }

    /// Record that work is happening here, so a later `grove down --idle` can tell this
    /// instance from one nobody has opened since Tuesday.
    pub fn touch(&self) -> Result<()> {
        self.registry.touch(&self.resolved.worktree)
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
        if let Some(resource) = self.config.resources.iter().find(|r| r.db_name.is_some()) {
            let template = resource.db_name.as_deref().expect("filtered on Some");
            let bare = RenderContext {
                slug: self.resolved.slug.clone(),
                ports: self.entry.ports.clone(),
                db_name: None,
                main_worktree: self.resolved.main_worktree.clone(),
            };
            let name = render::value(template, &bare)
                .with_context(|| format!("rendering db_name {template:?}"))?;
            self.entry.db_name = Some(name);
            // Recorded now, while the config that names the datastore is still readable.
            // `prune` needs it after the worktree has been deleted.
            self.entry.db_resource = Some(crate::registry::DbResource {
                name: resource.name.clone(),
                port: resource.port,
            });
            self.registry.record(&self.entry)?;
        }

        render::all(&self.config, &self.resolved, &self.render_context())
    }

    /// Bring up every declared datastore, reusing anything already answering on its port.
    pub fn resources(&mut self) -> Result<Vec<String>> {
        let mut started = Vec::new();
        for resource in &self.config.resources {
            if crate::resource::ensure(resource)?.started {
                started.push(resource.name.clone());
                self.recently_started_resources
                    .insert(resource.name.clone());
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

    /// Stop the named service (or all of them) so the next `up` starts it fresh.
    /// Returns what it stopped, so a caller can say which name it did not recognise.
    pub fn stop_services(&mut self, only: Option<&str>) -> Result<Vec<String>> {
        if let Some(name) = only
            && !self.config.services.iter().any(|s| s.name == name)
        {
            let known: Vec<&str> = self
                .config
                .services
                .iter()
                .map(|s| s.name.as_str())
                .collect();
            bail!("this repo declares no service named {name:?}; it has {known:?}");
        }

        let mut stopped = Vec::new();
        for (name, handle) in self.entry.services.clone() {
            if only.is_some_and(|wanted| wanted != name) {
                continue;
            }
            supervise::stop(&handle)?;
            self.entry.services.remove(&name);
            stopped.push(name);
        }
        self.registry.record(&self.entry)?;
        Ok(stopped)
    }

    /// Populate this instance's datastore. Each seed runs once per worktree unless
    /// `force`, and reports what it did — a seed that silently skipped is how you end up
    /// debugging a 403 that names authentication rather than missing data.
    pub fn seed(&self, force: bool) -> Result<Vec<SeedOutcome>> {
        let mut outcomes = Vec::new();
        let environment = self.environment()?;
        let current_resources = self.resource_incarnations();

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

            let command = render::value(&seed.command, &self.render_context())
                .with_context(|| format!("rendering the command for seed {}", seed.name))?;
            let marker = self.instance_dir().join(format!(".seed-{}", seed.name));
            let stored = read_seed_marker(&marker)?;
            let decision = seed_decision(
                stored.as_ref(),
                &command,
                &current_resources,
                &self.recently_started_resources,
                force,
            );
            if matches!(decision, SeedDecision::Skip) {
                std::fs::create_dir_all(self.instance_dir())?;
                write_seed_marker(
                    &marker,
                    &command,
                    merged_resources(stored.as_ref(), &current_resources),
                )?;
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
            write_seed_marker(
                &marker,
                &command,
                merged_resources(stored.as_ref(), &current_resources),
            )?;
            outcomes.push(SeedOutcome::Ran {
                name: seed.name.clone(),
                why: match decision {
                    SeedDecision::Run { why } => why,
                    SeedDecision::Skip => unreachable!(),
                },
            });
        }

        Ok(outcomes)
    }

    fn resource_incarnations(&self) -> BTreeMap<String, String> {
        self.config
            .resources
            .iter()
            .filter_map(|resource| {
                let observed = crate::resource::observe(resource);
                observed
                    .container
                    .map(|container| (resource.name.clone(), container.id))
            })
            .collect()
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
            let cwd = match &service.cwd {
                Some(dir) => self.resolved.worktree.join(dir),
                None => self.resolved.worktree.clone(),
            };
            let log = self.instance_dir().join(format!("{}.log", service.name));

            // Before the skip below, not after: a running service is precisely the case
            // where generated code goes stale unnoticed. Services start in declaration
            // order and each waits for its readiness probe, so by the time this runs the
            // services declared above are answering — which is what a generator reading
            // this worktree's own backend needs.
            if let Some(prepare) = &service.prepare {
                let command = render::value(prepare, &context)
                    .with_context(|| format!("rendering prepare for {}", service.name))?;
                self.run_prepare(&service.name, &command, &cwd, &log)?;
            }

            let already = self.entry.services.get(&service.name).copied();
            if let Some(handle) = already {
                if fresh {
                    supervise::stop(&handle)?;
                } else if supervise::is_alive(&handle) {
                    continue;
                }
            }

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
        self.run_step(
            &Step {
                service: name,
                kind: "setup",
                doing: "installing dependencies",
                done: "dependencies installed",
            },
            setup,
            cwd,
            log,
        )?;
        std::fs::write(&marker, setup)?;
        Ok(())
    }

    /// Code generation runs on **every** `up`, marker-free by design: what it produces has
    /// to track what it was generated from, and this one runs even when the service it
    /// belongs to is already up — which is exactly the case a stale client survives.
    fn run_prepare(&self, name: &str, prepare: &str, cwd: &Path, log: &Path) -> Result<()> {
        self.run_step(
            &Step {
                service: name,
                kind: "prepare",
                doing: "preparing",
                done: "prepared",
            },
            prepare,
            cwd,
            log,
        )
    }

    /// One blocking command with its output on disk and a heartbeat on the terminal.
    fn run_step(&self, step: &Step<'_>, command: &str, cwd: &Path, log: &Path) -> Result<()> {
        let Step {
            service,
            kind,
            doing,
            done,
        } = *step;
        std::fs::create_dir_all(self.instance_dir())?;
        eprintln!("{service}: {doing} ({command})");

        // Output goes to the service log rather than the terminal. `npm ci` and `uv sync`
        // between them emit hundreds of lines, and the port summary printed afterwards is
        // the one thing the caller actually needs to read.
        let sink = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .with_context(|| format!("opening {}", log.display()))?;
        let errors = sink.try_clone().context("duplicating the log handle")?;

        // A silent multi-minute step is indistinguishable from a hang, and an agent
        // watching it will eventually kill it. Tick while the child runs.
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .envs(self.environment()?)
            .stdout(std::process::Stdio::from(sink))
            .stderr(std::process::Stdio::from(errors))
            .spawn()
            .with_context(|| format!("running {kind} for {service}: {command}"))?;

        // First tick at 10s, then every 15s. The opening silence is what reads as a
        // hang, so it is the interval worth shortening.
        let began = std::time::Instant::now();
        let mut next_tick = 10u64;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
            let elapsed = began.elapsed().as_secs();
            if elapsed >= next_tick {
                eprintln!("{service}: still {doing} ({elapsed}s)");
                next_tick = elapsed + 15;
            }
        };
        if !status.success() {
            bail!(
                "{kind} for {service} failed: {command}\n\
                 full output in {}\n{}",
                log.display(),
                tail(log, 30).unwrap_or_default()
            );
        }
        // Closure matters as much as the ticks: a step that ends without saying so leaves
        // the reader wondering whether it finished or was skipped.
        eprintln!("{service}: {done} ({}s)", began.elapsed().as_secs());
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

    /// Unix seconds of the newest change to the worktree's own source, or None when git
    /// cannot say. Tracked plus untracked-not-ignored: that is the tree a developer edits,
    /// and it skips node_modules and .venv, which would otherwise dominate the answer.
    pub fn newest_source_change(&self) -> Option<u64> {
        let listed = |args: &[&str]| -> Vec<PathBuf> {
            std::process::Command::new("git")
                .current_dir(&self.resolved.worktree)
                .args(args)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .map(|l| self.resolved.worktree.join(l))
                        .collect()
                })
                .unwrap_or_default()
        };

        let mut paths = listed(&["ls-files"]);
        paths.extend(listed(&["ls-files", "-o", "--exclude-standard"]));

        paths
            .iter()
            .filter_map(|p| p.metadata().ok()?.modified().ok())
            .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .max()
    }

    /// Services running from before the newest edit. They keep serving the old code, and
    /// a service without hot reload gives no hint that it is doing so.
    pub fn stale_services(&self) -> Vec<String> {
        let Some(changed) = self.newest_source_change() else {
            return Vec::new();
        };
        self.entry
            .services
            .iter()
            .filter(|(_, h)| supervise::is_alive(h) && h.started_at > 0 && h.started_at < changed)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// What each declared service is doing: process state, and whether its endpoint
    /// answers. Probing costs one HTTP call per live service; a dead port on localhost
    /// refuses immediately, so the timeout only bites in the wedged case — which is
    /// precisely the case worth waiting for.
    pub fn status(&self) -> Vec<ServiceStatus> {
        let context = self.render_context();
        self.config
            .services
            .iter()
            .map(|s| {
                let running = self
                    .entry
                    .services
                    .get(&s.name)
                    .is_some_and(supervise::is_alive);
                let url = s
                    .ready
                    .as_ref()
                    .and_then(|r| render::value(&r.http, &context).ok());

                let ready = match (&url, running) {
                    (None, _) => Readiness::Undeclared,
                    // A stopped service serves nothing by definition, and probing it
                    // spends the timeout to learn what the pid already said.
                    (Some(_), false) => Readiness::Silent,
                    (Some(url), true) => {
                        if supervise::probe(url, PROBE_TIMEOUT) {
                            Readiness::Answering
                        } else {
                            Readiness::Silent
                        }
                    }
                };

                ServiceStatus {
                    name: s.name.clone(),
                    running,
                    pid: self.entry.services.get(&s.name).map(|h| h.pid),
                    ready,
                    url,
                }
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

/// `"180s"`, `"2m"`, `"6h"`, `"2d"`, `"500ms"`, or a bare number of seconds. Readiness
/// timeouts and `--idle` windows are the same kind of value written by the same hands, so
/// they parse the same way.
pub fn parse_duration(text: &str) -> Result<std::time::Duration> {
    let trimmed = text.trim();
    // "ms" is tested before "s", or every millisecond value would parse as seconds.
    let (value, multiplier) = match trimmed.strip_suffix("ms") {
        Some(v) => (v, 1u64),
        None => match trimmed.strip_suffix('s') {
            Some(v) => (v, 1000),
            None => match trimmed.strip_suffix('m') {
                Some(v) => (v, 60_000),
                None => match trimmed.strip_suffix('h') {
                    Some(v) => (v, 3_600_000),
                    None => match trimmed.strip_suffix('d') {
                        Some(v) => (v, 86_400_000),
                        None => (trimmed, 1000),
                    },
                },
            },
        },
    };
    let amount: u64 = value
        .trim()
        .parse()
        .with_context(|| format!("{text:?} is not a duration like \"180s\""))?;
    Ok(std::time::Duration::from_millis(amount * multiplier))
}

#[cfg(test)]
mod seed_marker_tests {
    use super::{SeedDecision, SeedMarker, StoredSeedMarker, seed_decision};
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn a_resource_started_now_invalidates_a_matching_legacy_marker() {
        let command = "seed the database";
        let stored = StoredSeedMarker::Legacy(command.to_string());
        let started = BTreeSet::from(["mongo".to_string()]);

        let decision = seed_decision(Some(&stored), command, &BTreeMap::new(), &started, false);

        assert!(matches!(
            decision,
            SeedDecision::Run { why: Some(why) } if why == "resource mongo was recreated"
        ));
    }

    #[test]
    fn a_resource_started_now_invalidates_a_matching_structured_marker() {
        let command = "seed the database";
        let stored = StoredSeedMarker::Structured(SeedMarker {
            version: 1,
            command: command.to_string(),
            resources: BTreeMap::from([("mongo".to_string(), "old-id".to_string())]),
        });
        let started = BTreeSet::from(["mongo".to_string()]);

        let decision = seed_decision(Some(&stored), command, &BTreeMap::new(), &started, false);

        assert!(matches!(
            decision,
            SeedDecision::Run { why: Some(why) } if why == "resource mongo was recreated"
        ));
    }
}
