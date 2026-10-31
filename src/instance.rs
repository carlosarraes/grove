//! One worktree's instance: its ports, its rendered config, its running services.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::config::{self, Config};
use crate::exposure::Exposure;
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
    recently_created_resources: BTreeSet<String>,
    /// The mode used by rendering and commands in this process. It starts at the
    /// persisted mode, but `up` can target a new one before committing the transition.
    target_exposure: Exposure,
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

/// An incomplete receipt keeps the expected output across failed repair attempts.
#[derive(Deserialize, Serialize)]
struct SetupReceipt {
    version: u8,
    identity: String,
    venv: bool,
    complete: bool,
}

impl SetupReceipt {
    fn read(path: &Path) -> Result<Option<Self>> {
        let body = match std::fs::read(path) {
            Ok(body) => body,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("reading setup receipt"),
        };
        // Legacy and unknown receipts lack a usable output contract. Run setup once.
        Ok(serde_json::from_slice::<Self>(&body)
            .ok()
            .filter(|receipt| receipt.version == 1))
    }

    fn write(&self, path: &Path) -> Result<()> {
        let temporary = path.with_extension("pending");
        std::fs::write(&temporary, serde_json::to_vec(self)?)?;
        std::fs::rename(&temporary, path).context("saving setup receipt")
    }
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
/// What `setup` did for one service, so `up` knows whether the tree on disk is new and
/// which store entry it came from.
pub enum SetupOutcome {
    Skipped,
    Installed,
    Linked { hash: String },
    Stored { hash: String },
}

#[derive(Clone, Copy)]
struct SetupMode {
    no_cache: bool,
    quiet_command: bool,
}

pub enum SetupInstallPlan {
    NoCache,
    LinkOnly,
    AlreadyInPlace,
    FullInstall(String),
}

/// A directory, a file, or a symlink — whatever `setup` or an earlier link left there.
fn remove_path(path: &Path) -> Result<()> {
    let meta = path.symlink_metadata()?;
    if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
    .with_context(|| format!("removing {}", path.display()))
}

/// Restore the previous install if setup or publication fails.
struct InstallChange {
    path: PathBuf,
    backup: PathBuf,
    committed: bool,
}

impl InstallChange {
    fn begin(path: &Path, backup: PathBuf) -> Result<Self> {
        if backup.symlink_metadata().is_ok() {
            bail!("unfinished dependency backup at {}", backup.display());
        }
        if path.symlink_metadata().is_ok() {
            std::fs::rename(path, &backup).with_context(|| {
                format!("moving {} to {}; the install and Grove state directory must be on the same filesystem", path.display(), backup.display())
            })?;
        }
        Ok(Self {
            path: path.to_owned(),
            backup,
            committed: false,
        })
    }

    fn commit(mut self, service: &str) {
        self.committed = true;
        let cleanup =
            crate::timing::measure_wait(&format!("{service}: old install cleanup"), || match self
                .backup
                .symlink_metadata()
            {
                Ok(_) => remove_path(&self.backup),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            });
        if let Err(error) = cleanup {
            eprintln!(
                "warning: {service}: old install cleanup failed: {error:#}; \
                 backup retained at {}. The new dependencies are ready.",
                self.backup.display()
            );
        }
    }
}

impl InstallChange {
    fn rollback(&mut self) -> Result<()> {
        if self.path.symlink_metadata().is_ok() {
            remove_path(&self.path)?;
        }
        if self.backup.symlink_metadata().is_ok() {
            std::fs::rename(&self.backup, &self.path).with_context(|| {
                format!(
                    "restoring {} from {}",
                    self.path.display(),
                    self.backup.display()
                )
            })?;
        }
        self.committed = true;
        Ok(())
    }
}

impl Drop for InstallChange {
    fn drop(&mut self) {
        if !self.committed
            && let Err(error) = self.rollback()
        {
            eprintln!(
                "dependency rollback failed: {error:#}; backup at {}",
                self.backup.display()
            );
        }
    }
}

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
    recently_created: &BTreeSet<String>,
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
            match recently_created.iter().next() {
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
            if let Some(name) = recently_created.iter().next() {
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

/// Hold from before Instance::open through startup so a waiting `up` reads fresh handles.
pub fn lock_up(cwd: &Path) -> Result<std::fs::File> {
    let resolved = resolve::resolve(cwd)?;
    let directory = state_dir()?.join(resolved.state_key()).join(&resolved.slug);
    std::fs::create_dir_all(&directory)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(directory.join("up.lock"))?;
    crate::timing::measure_wait("instance up lock wait", || Ok(lock.lock()?))?;
    Ok(lock)
}

/// Stop a `grove up` still running for the instance at `cwd`, and wait until it is gone.
/// Returns its pid when there was one. `down` calls this before tearing down: an up that
/// outlives `down` would start services after `down` reported them stopped.
///
/// Delivers SIGINT to the up and every descendant, exactly what Ctrl-C at its terminal
/// does, so the run log reads it as interrupted. Escalates to SIGKILL after 20s; errors
/// rather than returning while the up still holds its log. Safe to repeat: with no up
/// running it does nothing.
pub fn stop_running_up(cwd: &Path) -> Result<Option<u32>> {
    let resolved = resolve::resolve(cwd)?;
    let directory = state_dir()?.join(resolved.state_key()).join(&resolved.slug);
    let running = |dir: &Path| crate::last_up::read(dir).filter(|last| last.running());
    let Some(pid) = running(&directory).and_then(|last| last.pid) else {
        return Ok(None);
    };
    let wait = |limit: std::time::Duration| {
        let deadline = std::time::Instant::now() + limit;
        while std::time::Instant::now() < deadline {
            if running(&directory).is_none() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        running(&directory).is_none()
    };
    for (signal, limit) in [(libc::SIGINT, 20), (libc::SIGKILL, 5)] {
        // The tree first, then the up: once the up exits its children are reparented
        // and can no longer be found from its pid.
        for target in descendants(pid).into_iter().rev().chain([pid]) {
            // SAFETY: plain kill(2); a pid that already exited just returns ESRCH.
            unsafe {
                libc::kill(target as libc::pid_t, signal);
            }
        }
        if wait(std::time::Duration::from_secs(limit)) {
            return Ok(Some(pid));
        }
    }
    bail!(
        "grove up (pid {pid}) for {} is still running after SIGINT and SIGKILL; \
         nothing was torn down. Check `ps -p {pid}` and run grove down again",
        resolved.slug
    )
}

/// Every process below `root`, parents before children, from one `ps` snapshot.
fn descendants(root: u32) -> Vec<u32> {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=", "-o", "ppid="])
        .output()
    else {
        return Vec::new();
    };
    let pairs: Vec<(u32, u32)> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
        })
        .collect();
    let mut found = vec![root];
    let mut index = 0;
    while index < found.len() {
        let parent = found[index];
        found.extend(
            pairs
                .iter()
                .filter(|(_, ppid)| *ppid == parent)
                .map(|(pid, _)| *pid),
        );
        index += 1;
    }
    found.remove(0);
    found
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
        let config = config::load_for(&resolved.worktree, &resolved.main_worktree)?;
        let registry = registry()?;
        let entry = registry.reserve(&resolved, &config.ports.names)?;
        let state_dir = state_dir()?;
        let target_exposure = entry.exposure.clone();
        let mut instance = Instance {
            resolved,
            config,
            entry,
            registry,
            state_dir,
            recently_created_resources: BTreeSet::new(),
            target_exposure,
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

    /// What the newest `grove up` here ended with, from its saved run log.
    pub fn last_up(&self) -> Option<crate::last_up::LastUp> {
        crate::last_up::read(&self.instance_dir())
    }

    /// Services whose setup has never finished in this worktree. A local install counts
    /// once its receipt says complete; a cached one once its marker exists. A marker that
    /// predates receipts counts as complete — it was only ever written on success.
    pub fn setup_incomplete(&self) -> Vec<String> {
        self.config
            .services
            .iter()
            .filter(|s| s.setup.is_some())
            .filter(|s| {
                let marker = self.instance_dir().join(format!(".setup-{}", s.name));
                match std::fs::read(&marker) {
                    Err(_) => true,
                    Ok(_) if s.cache.is_some() => false,
                    Ok(body) => serde_json::from_slice::<SetupReceipt>(&body)
                        .is_ok_and(|receipt| !receipt.complete),
                }
            })
            .map(|s| s.name.clone())
            .collect()
    }

    /// Whether `up` would only link dependency trees or build one, by the same test
    /// `run_setup` makes: the cache key from the service's lockfiles, then whether the
    /// worktree already holds that tree or the store has a published entry for it. Read
    /// only — it takes no cache lock, so a concurrent publish can only turn a full
    /// install into a link, never the reverse.
    pub fn install_plan(&self, no_cache: bool) -> crate::load::Install {
        use crate::load::Install;
        let plans = self.setup_install_plan(no_cache);
        let cached: Vec<_> = plans
            .iter()
            .filter(|(_, plan)| !matches!(plan, SetupInstallPlan::NoCache))
            .collect();
        if cached.is_empty() {
            return Install::Full("no cached dependencies to link".to_string());
        }
        for (_, plan) in &cached {
            if let SetupInstallPlan::FullInstall(reason) = plan {
                return Install::Full(reason.clone());
            }
        }
        if cached
            .iter()
            .any(|(_, plan)| matches!(plan, SetupInstallPlan::LinkOnly))
        {
            Install::LinkOnly("lockfile matches store".to_string())
        } else {
            Install::LinkOnly("dependencies already in place".to_string())
        }
    }

    /// Per-service view of the same cache decision used by `install_plan` and setup.
    pub fn setup_install_plan(&self, no_cache: bool) -> Vec<(String, SetupInstallPlan)> {
        let store = crate::store::dir();
        self.config
            .services
            .iter()
            .filter_map(|service| {
                let setup = service.setup.as_ref()?;
                let Some(cache) = &service.cache else {
                    return Some((service.name.clone(), SetupInstallPlan::NoCache));
                };
                let plan = if no_cache {
                    SetupInstallPlan::FullInstall(format!("--no-cache rebuilds {}", service.name))
                } else if let Err(error) = &store {
                    SetupInstallPlan::FullInstall(format!("cannot locate the store: {error}"))
                } else {
                    let cwd = match &service.cwd {
                        Some(dir) => self.resolved.worktree.join(dir),
                        None => self.resolved.worktree.clone(),
                    };
                    match crate::store::key(&cwd, cache, setup) {
                        Err(_) => SetupInstallPlan::FullInstall(format!(
                            "{} lockfile unreadable",
                            service.name
                        )),
                        Ok(hash) => {
                            let marker =
                                self.instance_dir().join(format!(".setup-{}", service.name));
                            if cwd.join(&cache.path).is_dir()
                                && std::fs::read_to_string(&marker).ok().as_deref()
                                    == Some(hash.as_str())
                            {
                                SetupInstallPlan::AlreadyInPlace
                            } else if crate::store::entry(store.as_ref().expect("checked"), &hash)
                                .is_dir()
                            {
                                SetupInstallPlan::LinkOnly
                            } else {
                                SetupInstallPlan::FullInstall(format!(
                                    "{} lockfile not in store, {hash}",
                                    service.name
                                ))
                            }
                        }
                    }
                };
                Some((service.name.clone(), plan))
            })
            .collect()
    }

    pub fn db_name(&self) -> Option<String> {
        self.entry.db_name.clone()
    }

    pub fn exposure(&self) -> &Exposure {
        &self.target_exposure
    }

    fn render_context(&self) -> RenderContext {
        RenderContext {
            slug: self.resolved.slug.clone(),
            ports: self.entry.ports.clone(),
            db_name: self.entry.db_name.clone(),
            main_worktree: self.resolved.main_worktree.clone(),
            exposure: self.target_exposure.clone(),
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
                exposure: self.target_exposure.clone(),
            };
            let name = render::value(template, &bare)
                .with_context(|| format!("rendering db_name {template:?}"))?;
            self.entry.db_name = Some(bounded_database_name(&name));
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

    /// Render one requested network mode and, when it changed, replace the entire running
    /// stack. The target is committed only after rendering succeeds; after that point a
    /// startup failure must retain it because the generated files already describe it.
    pub fn render_for_up(&mut self, target: Exposure) -> Result<()> {
        let changed = target != self.entry.exposure;
        self.target_exposure = target;
        self.render()?;
        if !changed {
            return Ok(());
        }

        self.down()?;
        self.registry
            .set_exposure(&self.resolved.worktree, self.target_exposure.clone())?;
        self.entry.exposure = self.target_exposure.clone();
        Ok(())
    }

    /// Bring up every declared datastore, reusing anything already answering on its port.
    pub fn resources(&mut self) -> Result<Vec<String>> {
        let mut started = Vec::new();
        for resource in &self.config.resources {
            let result = crate::resource::ensure(resource)?;
            if result.started {
                started.push(resource.name.clone());
            }
            if result.created {
                self.recently_created_resources
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
                &self.recently_created_resources,
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

            for resource in &self.config.resources {
                if resource.name == "mongo"
                    || resource.db_name.is_some()
                    || resource.image.as_deref().is_some_and(|image| {
                        image
                            .rsplit('/')
                            .next()
                            .unwrap_or(image)
                            .split([':', '@'])
                            .next()
                            == Some("mongo")
                    })
                {
                    crate::timing::measure(
                        &format!("seed {}: Mongo readiness", seed.name),
                        || {
                            crate::mongo::wait_ready(
                                resource.port,
                                &cwd,
                                std::time::Duration::from_secs(180),
                            )
                        },
                    )?;
                }
            }
            let instance_dir = self.instance_dir();
            std::fs::create_dir_all(&instance_dir)?;
            std::fs::set_permissions(&instance_dir, std::fs::Permissions::from_mode(0o700))?;
            let log = self.instance_dir().join(format!("seed-{}.log", seed.name));
            use std::os::unix::fs::OpenOptionsExt;
            let sink = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(&log)
                .with_context(|| format!("opening {}", log.display()))?;
            std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o600))?;
            let errors = sink.try_clone().context("duplicating the log handle")?;

            eprintln!("seed {}: running (log: {})", seed.name, log.display());
            crate::timing::measure(&format!("seed {}", seed.name), || {
                let status = crate::timing::with_heartbeat(
                    std::time::Duration::from_secs(15),
                    |elapsed| {
                        let log_status = std::fs::metadata(&log).ok().and_then(|metadata| {
                            let modified = metadata.modified().ok()?;
                            let since_last_write = std::time::SystemTime::now()
                                .duration_since(modified)
                                .unwrap_or_default();
                            Some((metadata.len(), since_last_write))
                        });
                        eprintln!(
                            "seed {}: {}",
                            seed.name,
                            format_seed_heartbeat(elapsed, log_status)
                        )
                    },
                    || {
                        use std::os::unix::process::CommandExt;
                        let timeout = parse_duration(&seed.timeout)?;
                        let signals = supervise::SeedSignals::capture()?;
                        let began = std::time::Instant::now();
                        let mut child = std::process::Command::new("sh")
                            .arg("-c")
                            .arg(&command)
                            .current_dir(&cwd)
                            .envs(&environment)
                            .stdout(std::process::Stdio::from(sink))
                            .stderr(std::process::Stdio::from(errors))
                            .process_group(0)
                            .spawn()
                            .with_context(|| format!("starting seed {}", seed.name))?;
                        loop {
                            let signal = signals.pending();
                            if signal != 0 {
                                let group = rustix::process::Pid::from_child(&child);
                                let _ = rustix::process::kill_process_group(
                                    group,
                                    rustix::process::Signal::KILL,
                                );
                                child.wait().context("reaping cancelled seed")?;
                                drop(signals);
                                // SAFETY: cleanup is complete and the original disposition is restored.
                                unsafe {
                                    libc::raise(signal);
                                }
                                bail!("seed {} cancelled by signal {signal}", seed.name);
                            }
                            if let Some(status) = child.try_wait()? {
                                break Ok(status);
                            }
                            if began.elapsed() >= timeout {
                                let group = rustix::process::Pid::from_child(&child);
                                let _ = rustix::process::kill_process_group(
                                    group,
                                    rustix::process::Signal::KILL,
                                );
                                child.wait().context("reaping timed-out seed")?;
                                bail!(
                                    "seed {} timed out after {:.1}s; full output in {}\nraise seeds.{}.timeout in .grove.toml, or check mongo with grove health",
                                    seed.name,
                                    began.elapsed().as_secs_f64(),
                                    log.display(),
                                    seed.name
                                );
                            }
                            std::thread::sleep(
                                std::time::Duration::from_millis(25)
                                    .min(timeout.saturating_sub(began.elapsed())),
                            );
                        }
                    },
                )?;

                if !status.success() {
                    let mut message = format!(
                        "seed {} failed\nfull output in {}",
                        seed.name,
                        log.display()
                    );
                    if let Some(error) = std::fs::read_to_string(&log)
                        .ok()
                        .and_then(|body| first_error(&body))
                    {
                        message.push_str(&format!("\nfirst error: {error}"));
                    }
                    message.push('\n');
                    bail!("{message}");
                }
                Ok(())
            })?;
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
    pub fn up(
        &mut self,
        fresh: bool,
        no_cache: bool,
        measure_footprint: bool,
    ) -> Result<Vec<SeedOutcome>> {
        let mut stopped = BTreeSet::new();
        let result = self.up_inner(fresh, no_cache, measure_footprint, &mut stopped);
        if let Err(mut error) = result {
            for service in self.config.services.clone() {
                if stopped.contains(&service.name)
                    && let Err(recovery) = self.start_service(&service, false)
                {
                    error = error.context(format!(
                        "{}: service recovery also failed: {recovery:#}",
                        service.name
                    ));
                }
            }
            return Err(error);
        }
        result
    }

    /// Run only dependency setup, restoring any live services stopped for safe replacement.
    pub fn setup_only(
        &mut self,
        no_cache: bool,
        measure_footprint: bool,
    ) -> Result<Vec<(crate::config::Service, SetupOutcome)>> {
        let mut stopped = BTreeSet::new();
        let result = self.setup_dependencies(no_cache, measure_footprint, true, &mut stopped);
        match result {
            Ok(outcomes) => {
                let mut recovery_error: Option<anyhow::Error> = None;
                for service in self.config.services.clone() {
                    if stopped.contains(&service.name)
                        && let Err(error) = self.start_service(&service, false)
                    {
                        let error = anyhow::anyhow!("{}: {error:#}", service.name);
                        recovery_error = Some(match recovery_error {
                            Some(previous) => previous.context(error.to_string()),
                            None => error,
                        });
                    }
                }
                match recovery_error {
                    Some(error) => Err(error),
                    None => Ok(outcomes),
                }
            }
            Err(mut error) => {
                for service in self.config.services.clone() {
                    if stopped.contains(&service.name)
                        && let Err(recovery) = self.start_service(&service, false)
                    {
                        error = error.context(format!(
                            "{}: service recovery also failed: {recovery:#}",
                            service.name
                        ));
                    }
                }
                Err(error)
            }
        }
    }

    fn up_inner(
        &mut self,
        fresh: bool,
        no_cache: bool,
        measure_footprint: bool,
        stopped: &mut BTreeSet<String>,
    ) -> Result<Vec<SeedOutcome>> {
        self.refuse_taken_ports()?;
        self.setup_dependencies(no_cache, measure_footprint, false, stopped)?;
        let seeded = self.seed(false)?;
        for service in self.config.services.clone() {
            self.start_service(&service, fresh)?;
            stopped.remove(&service.name);
        }
        Ok(seeded)
    }

    fn setup_dependencies(
        &mut self,
        no_cache: bool,
        measure_footprint: bool,
        quiet_command: bool,
        stopped: &mut BTreeSet<String>,
    ) -> Result<Vec<(crate::config::Service, SetupOutcome)>> {
        let mut keys = Vec::new();
        let mut outcomes = Vec::new();
        for service in self.config.services.clone() {
            if let Some(setup) = &service.setup {
                let cwd = match &service.cwd {
                    Some(dir) => self.resolved.worktree.join(dir),
                    None => self.resolved.worktree.clone(),
                };
                let log = self.instance_dir().join(format!("{}.log", service.name));
                if !quiet_command {
                    eprintln!("{}: checking dependency setup", service.name);
                }
                let outcome =
                    crate::timing::measure(&format!("{}: dependency setup", service.name), || {
                        self.run_setup(
                            &service,
                            setup,
                            &cwd,
                            &log,
                            stopped,
                            SetupMode {
                                no_cache,
                                quiet_command,
                            },
                        )
                    })
                    .with_context(|| format!("{}: dependency setup failed", service.name))?;
                match &outcome {
                    SetupOutcome::Skipped | SetupOutcome::Installed => {}
                    SetupOutcome::Linked { hash } | SetupOutcome::Stored { hash } => {
                        keys.push((service.name.clone(), hash.clone()));
                    }
                }
                if !quiet_command {
                    eprintln!("{}: dependency setup complete", service.name);
                }
                outcomes.push((service, outcome));
            }
        }
        for (name, hash) in keys {
            self.entry.cache_keys.insert(name, hash);
        }
        self.registry.record(&self.entry)?;
        // Measure after setup, before anything below can fail, when explicitly requested.
        // A requested scan also runs when setup was skipped or absent.
        if measure_footprint {
            eprintln!("measuring dependency footprint");
            self.entry.disk_bytes = crate::timing::measure("dependency footprint", || {
                Ok(crate::timing::with_heartbeat(
                    std::time::Duration::from_secs(15),
                    |elapsed| {
                        eprintln!(
                            "still measuring dependency footprint ({}s)",
                            elapsed.as_secs()
                        )
                    },
                    || crate::footprint::measure(&self.resolved.worktree),
                ))
            })?;
            self.registry.record(&self.entry)?;
        }
        Ok(outcomes)
    }

    fn start_service(&mut self, service: &crate::config::Service, fresh: bool) -> Result<()> {
        let context = self.render_context();
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
            crate::timing::measure(&format!("{}: prepare", service.name), || {
                self.run_prepare(&service.name, &command, &cwd, &log)
            })?;
        }

        let Some(command) = &service.command else {
            return Ok(());
        };
        let already = self.entry.services.get(&service.name).copied();
        if let Some(handle) = already {
            if fresh {
                supervise::stop(&handle)?;
            } else if supervise::is_alive(&handle) {
                return Ok(());
            }
        }

        // Before starting, not after: the readiness probe cannot tell this service's
        // answer from another process's on the same port. A reservation is stable
        // across restarts and nothing re-checks it, so a port that already answers is
        // held by something else — and a service started on it would die on bind
        // while grove reported the other process's answers as readiness.
        if let Some(ready) = &service.ready {
            let url = render::value(&ready.http, &context)?;
            if supervise::probe(&url, std::time::Duration::from_secs(1)) {
                bail!(
                    "{} not started: {url} already answers, so its port is already \
                 in use by another process\n\
                 `grove ls` names the instances holding ports; anything else \
                 on that port is not grove's to stop",
                    service.name
                );
            }
        }

        let command = render::value(command, &context)
            .with_context(|| format!("rendering the command for {}", service.name))?;
        let environment = self.environment()?;
        let handle =
            crate::timing::measure_wait(&format!("{}: process start", service.name), || {
                supervise::spawn(&command, &cwd, &environment, &log)
            })?;
        self.entry.services.insert(service.name.clone(), handle);
        self.registry.record(&self.entry)?;

        if let Some(ready) = &service.ready {
            let url = render::value(&ready.http, &context)?;
            let timeout = parse_duration(&ready.timeout)?;
            crate::timing::measure_wait(&format!("{}: readiness", service.name), || {
                supervise::wait_ready(&handle, &url, timeout).map_err(|why| {
                    let tail = tail(&log, 30).unwrap_or_default();
                    // A worktree deleted mid-up makes the service fail on whatever it
                    // reads next, so its own output points the reader the wrong way.
                    let gone = if self.resolved.worktree.exists() {
                        String::new()
                    } else {
                        format!(
                            "\nthe worktree {} no longer exists: something removed it while \
                             grove up was running, so the output below is a symptom",
                            self.resolved.worktree.display()
                        )
                    };
                    match why {
                        // Spelled out rather than wrapped: "never became ready" reads as
                        // slow, and a reader waits. This one died, and the tail says why.
                        supervise::NotReady::Exited { url } => anyhow::anyhow!(
                            "{} exited before answering on {url}{gone}\n\
                         full output in {}\n{tail}",
                            service.name,
                            log.display()
                        ),
                        timed_out => anyhow::anyhow!(
                            "{} never became ready: {timed_out}{gone}\nTCP check: {} (observed after timeout)\nfull output in {}\n{tail}",
                            service.name,
                            supervise::tcp_observation(&url),
                            log.display()
                        ),
                    }
                })
            })?;
        }
        Ok(())
    }

    /// A live process must not retain the dependency tree that this transaction removes.
    fn replace_install<T>(
        &mut self,
        service: &crate::config::Service,
        built: &Path,
        stopped: &mut BTreeSet<String>,
        install: impl FnOnce(&Self) -> Result<T>,
    ) -> Result<T> {
        let backups = self.instance_dir().join("dependency-backups");
        std::fs::create_dir_all(&backups)?;
        if backups
            .canonicalize()?
            .starts_with(self.resolved.worktree.canonicalize()?)
        {
            bail!("dependency backups require a Grove state directory outside the worktree");
        }
        let backup = backups.join(format!("{}-{}", service.name, std::process::id()));
        let running = self
            .entry
            .services
            .get(&service.name)
            .copied()
            .filter(supervise::is_alive);
        if let Some(handle) = running {
            crate::timing::measure_wait(
                &format!("{}: stop before dependency replacement", service.name),
                || {
                    supervise::stop(&handle)?;
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                    while supervise::is_alive(&handle) && std::time::Instant::now() < deadline {
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                    if supervise::is_alive(&handle) {
                        bail!(
                            "{} did not stop; dependencies were not replaced",
                            service.name
                        );
                    }
                    Ok(())
                },
            )?;
        }
        if running.is_some() {
            stopped.insert(service.name.clone());
        }
        (|| {
            let mut change = InstallChange::begin(built, backup)?;
            match install(self) {
                Ok(value) => {
                    change.commit(&service.name);
                    Ok(value)
                }
                Err(error) => {
                    if let Err(restore) = change.rollback() {
                        stopped.remove(&service.name);
                        return Err(
                            error.context(format!("dependency rollback failed: {restore:#}"))
                        );
                    }
                    Err(error)
                }
            }
        })()
    }

    /// When none of this instance's services is running, nothing should be listening on
    /// any of its ports. A reservation is stable across restarts and nothing else
    /// re-checks it, so a port taken in the meantime would leave a service dying on bind
    /// while another process answered in its place. Checked first, before a setup that
    /// can run for minutes, and by bind rather than probe so a listener that speaks no
    /// HTTP is caught too. With something of its own running the ports cannot be told
    /// apart here; the per-service readiness check covers that case.
    fn refuse_taken_ports(&self) -> Result<()> {
        if self.entry.services.values().any(supervise::is_alive) {
            return Ok(());
        }
        for (name, port) in &self.entry.ports {
            if !crate::ports::is_free(*port) {
                bail!(
                    "port {port} ({name}) is already in use by another process, and none \
                     of this instance's services is running to explain it\n\
                     `grove ls` names the instances holding ports; anything else on \
                     {port} is not grove's to stop"
                );
            }
        }
        Ok(())
    }

    /// Dependency installs run once per worktree, tracked by a marker beside the logs.
    /// With a cache declared, "run" may mean linking a tree the store already holds —
    /// a worktree on a lockfile some other worktree installed pays seconds, not minutes.
    fn run_setup(
        &mut self,
        service: &crate::config::Service,
        setup: &str,
        cwd: &Path,
        log: &Path,
        stopped: &mut BTreeSet<String>,
        mode: SetupMode,
    ) -> Result<SetupOutcome> {
        let name = &service.name;
        let marker = self.instance_dir().join(format!(".setup-{name}"));
        std::fs::create_dir_all(self.instance_dir())?;
        let instance_lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(marker.with_extension("lock"))?;
        crate::timing::measure_wait(&format!("{name}: setup lock wait"), || {
            Ok(instance_lock.lock()?)
        })?;
        let step = Step {
            service: name,
            kind: "setup",
            doing: "installing dependencies",
            done: "dependencies installed",
        };
        let Some(cache) = &service.cache else {
            let identity = setup_identity(service, setup, cwd)?;
            let previous = SetupReceipt::read(&marker)?;
            let venv = cwd.join(".venv");
            let location = cwd.canonicalize()?;
            let path_record = venv.join(".grove-setup-path");
            let usable = local_venv_usable(&venv)?;
            let expects_venv =
                previous.as_ref().is_some_and(|receipt| receipt.venv) || venv.try_exists()?;
            let path_matches = match std::fs::read(&path_record) {
                Ok(recorded) => recorded == location.as_os_str().as_encoded_bytes(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(error).context("reading venv setup path"),
            };
            let rebuild_venv = expects_venv && (!usable || !path_matches);
            if !mode.no_cache
                && !rebuild_venv
                && previous
                    .as_ref()
                    .is_some_and(|receipt| receipt.complete && receipt.identity == identity)
            {
                return Ok(SetupOutcome::Skipped);
            }
            SetupReceipt {
                version: 1,
                identity: identity.clone(),
                venv: expects_venv,
                complete: false,
            }
            .write(&marker)?;
            let install = |instance: &Self| -> Result<()> {
                instance.run_step(&step, setup, cwd, log, mode.quiet_command)?;
                if setup_identity(service, setup, cwd)? != identity {
                    bail!(
                        "{name}: setup changed its setup_inputs; refusing to mark setup complete"
                    );
                }
                let produced_venv = local_venv_usable(&venv)?;
                if (expects_venv || venv.try_exists()?) && !produced_venv {
                    bail!(
                        "{name}: setup did not restore a usable .venv with pyvenv.cfg and an executable bin/python"
                    );
                }
                if produced_venv {
                    std::fs::write(&path_record, location.as_os_str().as_encoded_bytes())?;
                }
                SetupReceipt {
                    version: 1,
                    identity: identity.clone(),
                    venv: produced_venv,
                    complete: true,
                }
                .write(&marker)?;
                Ok(())
            };
            if rebuild_venv {
                if !mode.quiet_command {
                    eprintln!("{name}: rebuilding missing, incomplete or relocated .venv");
                }
                self.replace_install(service, &venv, stopped, install)?;
            } else {
                install(self)?;
            }
            return Ok(SetupOutcome::Installed);
        };

        let store = crate::store::dir()?;
        let hash = crate::store::key(cwd, cache, setup).map_err(|error| {
            if mode.quiet_command {
                anyhow::anyhow!("cache key could not be computed")
            } else {
                error
            }
        })?;
        let built = cwd.join(&cache.path);
        if !mode.no_cache
            && built.is_dir()
            && std::fs::read_to_string(&marker).ok().as_deref() == Some(hash.as_str())
        {
            return Ok(SetupOutcome::Skipped);
        }
        if marker.exists() {
            std::fs::remove_file(&marker)?;
        }
        let entry = crate::store::entry(&store, &hash);
        if !mode.quiet_command {
            eprintln!("{name}: checking dependency cache ({hash})");
        }
        let mut lease =
            crate::timing::measure_wait(&format!("{name}: shared cache lock wait"), || {
                crate::store::lock_key(&store, &hash, true)
            })?;
        if mode.no_cache || !entry.exists() {
            drop(lease);
            lease =
                crate::timing::measure_wait(&format!("{name}: exclusive cache lock wait"), || {
                    crate::store::lock_key(&store, &hash, false)
                })?;
            if mode.no_cache {
                crate::timing::measure_wait(&format!("{name}: cache eviction"), || {
                    crate::store::evict(&store, &hash)
                })?;
            }
        }
        let _lease = lease;
        if entry.exists() {
            let staged = built.with_file_name(format!(
                ".{}.grove-stage-{}",
                built.file_name().unwrap().to_string_lossy(),
                std::process::id()
            ));
            std::fs::create_dir_all(staged.parent().expect("dependency parent"))?;
            if staged.try_exists()? {
                bail!(
                    "unfinished dependency staging directory at {}",
                    staged.display()
                );
            }
            let began = std::time::Instant::now();
            if !mode.quiet_command {
                eprintln!("{name}: linking dependencies from the store ({hash})");
            }
            let result: Result<crate::store::Linked> = (|| {
                let linked = crate::store::link_entry(&store, &hash, &staged)
                    .with_context(|| format!("linking {} from the store", built.display()))?;
                self.replace_install(service, &built, stopped, |_| {
                    crate::timing::measure_wait(&format!("{name}: dependency replacement"), || {
                        std::fs::rename(&staged, &built)?;
                        Ok(())
                    })
                })?;
                Ok(linked)
            })();
            if staged.exists() {
                let _ = std::fs::remove_dir_all(&staged);
            }
            let linked: crate::store::Linked = result?;
            crate::timing::measure_wait(&format!("{name}: store entry size"), || {
                crate::store::record_bytes(&store, &hash)
            })?;
            if !mode.quiet_command {
                eprintln!(
                    "{name}: dependencies {} from the store ({hash}, {}s)",
                    match linked.method {
                        crate::store::LinkMethod::Hardlink => "linked",
                        crate::store::LinkMethod::Copy => "copied",
                        crate::store::LinkMethod::Clone => "cloned",
                    },
                    began.elapsed().as_secs()
                );
            }
            std::fs::write(&marker, &hash)?;
            return Ok(SetupOutcome::Linked { hash });
        }

        let promoted = self.replace_install(service, &built, stopped, |instance| {
        instance.run_step(&step, setup, cwd, log, mode.quiet_command)?;
        if crate::store::key(cwd, cache, setup)? != hash {
            bail!(
                "{name}: setup changed its cache key files; refusing to publish dependencies under stale inputs"
            );
        }
        if !built.is_dir() {
            bail!("{name}: setup left no {} directory to cache", cache.path);
        }
        let promoted = crate::timing::measure_wait(&format!("{name}: cache publication"), || {
            crate::store::promote(&built, &store, &hash)
        })
        .with_context(|| format!("storing {}", built.display()))?;
            Ok(promoted)
        })?;
        if !mode.quiet_command {
            eprintln!(
                "{name}: dependencies stored as {hash}{}",
                if promoted.shared {
                    ""
                } else {
                    " (private copy)"
                }
            );
        }
        std::fs::write(&marker, &hash)?;
        Ok(SetupOutcome::Stored { hash })
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
            false,
        )
    }

    /// One blocking command with its output on disk and a heartbeat on the terminal.
    fn run_step(
        &self,
        step: &Step<'_>,
        command: &str,
        cwd: &Path,
        log: &Path,
        quiet_command: bool,
    ) -> Result<()> {
        let Step {
            service,
            kind,
            doing,
            done,
        } = *step;
        std::fs::create_dir_all(self.instance_dir())?;
        if quiet_command {
            eprintln!("{service}: {doing}");
        } else {
            eprintln!("{service}: {doing} ({command})");
        }

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
            .with_context(|| {
                if quiet_command {
                    format!("running {kind} for {service}")
                } else {
                    format!("running {kind} for {service}: {command}")
                }
            })?;

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
            if quiet_command {
                bail!(
                    "{kind} for {service} failed; full output in {}",
                    log.display()
                );
            } else {
                bail!(
                    "{kind} for {service} failed: {command}\n\
                     full output in {}\n{}",
                    log.display(),
                    tail(log, 30).unwrap_or_default()
                );
            }
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
            .filter(|s| s.command.is_some())
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

fn first_error(log: &str) -> Option<String> {
    let lines: Vec<&str> = log.lines().collect();
    for (index, line) in lines.iter().enumerate() {
        let frame = line.trim();
        let Some(path) = frame
            .strip_prefix("File \"")
            .and_then(|rest| rest.split_once('\"').map(|(path, _)| path))
        else {
            continue;
        };
        if path.contains("/site-packages/")
            || path.contains("/dist-packages/")
            || path.contains("/.venv/lib/python")
        {
            continue;
        }
        let source = lines
            .get(index + 1)
            .map(|source| source.trim())
            .filter(|source| !source.is_empty());
        return Some(match source {
            Some(source) => format!("{frame}: {source}"),
            None => frame.to_owned(),
        });
    }

    lines
        .iter()
        .rev()
        .map(|line| line.trim())
        .find(|line| {
            line.starts_with("Error:") || line.starts_with("error:") || exception_line(line)
        })
        .map(str::to_owned)
}

fn exception_line(line: &str) -> bool {
    let name = line.split_once(':').map_or(line, |(name, _)| name);
    let parts = name.split('.');
    let valid = parts.clone().all(|part| {
        !part.is_empty()
            && part
                .chars()
                .all(|character| character == '_' || character.is_ascii_alphanumeric())
    });
    valid && (name.ends_with("Error") || name.ends_with("Exception") || name.ends_with("Cancelled"))
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
    let milliseconds = amount
        .checked_mul(multiplier)
        .with_context(|| format!("{text:?} is too large a duration"))?;
    Ok(std::time::Duration::from_millis(milliseconds))
}

/// Only local Python environments participate in relocation recovery.
fn local_venv_exists(venv: &Path) -> Result<bool> {
    let metadata = match venv.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("inspecting local venv"),
    };
    if metadata.file_type().is_symlink() {
        bail!(
            "{} is a symlink; Grove requires a worktree-local .venv",
            venv.display()
        );
    }
    Ok(venv.join("pyvenv.cfg").try_exists()?)
}

fn local_venv_usable(venv: &Path) -> Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    if !local_venv_exists(venv)? {
        return Ok(false);
    }
    match std::fs::metadata(venv.join("bin/python")) {
        Ok(metadata) => Ok(metadata.is_file() && metadata.permissions().mode() & 0o111 != 0),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("inspecting venv Python entry point"),
    }
}

/// Keep uncached installs local while tracking the files that decide their contents.
fn setup_identity(service: &crate::config::Service, setup: &str, cwd: &Path) -> Result<String> {
    use std::hash::{DefaultHasher, Hash, Hasher};

    let Some(inputs) = &service.setup_inputs else {
        return Ok(setup.to_owned());
    };
    let mut hash = DefaultHasher::new();
    setup.hash(&mut hash);
    for input in inputs {
        let path = cwd.join(input);
        let bytes = std::fs::read(&path).with_context(|| {
            format!(
                "{}: reading setup_inputs file {}",
                service.name,
                path.display()
            )
        })?;
        input.hash(&mut hash);
        bytes.hash(&mut hash);
    }
    Ok(format!("setup-inputs-v1:{:016x}", hash.finish()))
}

fn format_seed_heartbeat(
    elapsed: std::time::Duration,
    log: Option<(u64, std::time::Duration)>,
) -> String {
    let base = format!("still running ({}s", elapsed.as_secs());
    match log {
        Some((bytes, since_last_write)) => {
            format!(
                "{base}; log {}, last write {}s ago)",
                crate::footprint::human_size(bytes),
                since_last_write.as_secs()
            )
        }
        None => format!("{base})"),
    }
}

// Keep valid existing names unchanged. Hash the full rendered name, including the slug.
fn bounded_database_name(name: &str) -> String {
    const LIMIT: usize = 63;
    if name.len() <= LIMIT {
        return name.to_owned();
    }
    // FNV-1a has a stable byte representation across processes and Grove versions.
    let hash = name.bytes().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    });
    let mut end = LIMIT - 17; // Separator plus sixteen hexadecimal digits.
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}_{hash:016x}", &name[..end])
}

#[cfg(test)]
mod seed_marker_tests {
    use super::{SeedDecision, SeedMarker, StoredSeedMarker, first_error, seed_decision};
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

    #[test]
    fn seed_heartbeat_formats_log_size_and_age() {
        assert_eq!(
            super::format_seed_heartbeat(
                std::time::Duration::from_secs(480),
                Some((41 * 1024, std::time::Duration::from_secs(3)))
            ),
            "still running (480s; log 41K, last write 3s ago)"
        );
    }

    #[test]
    fn seed_heartbeat_formats_human_size_units() {
        assert_eq!(
            super::format_seed_heartbeat(
                std::time::Duration::from_secs(15),
                Some((1024, std::time::Duration::ZERO))
            ),
            "still running (15s; log 1.0K, last write 0s ago)"
        );
        assert_eq!(
            super::format_seed_heartbeat(
                std::time::Duration::from_secs(15),
                Some((1024 * 1024, std::time::Duration::ZERO))
            ),
            "still running (15s; log 1.0M, last write 0s ago)"
        );
        assert_eq!(
            super::format_seed_heartbeat(
                std::time::Duration::from_secs(15),
                Some((42, std::time::Duration::ZERO))
            ),
            "still running (15s; log 42B, last write 0s ago)"
        );
    }

    #[test]
    fn seed_heartbeat_falls_back_when_log_metadata_is_unavailable() {
        assert_eq!(
            super::format_seed_heartbeat(std::time::Duration::from_secs(480), None),
            "still running (480s)"
        );
    }

    #[test]
    fn first_error_prefers_seed_frames_over_python_library_frames() {
        let log = r#"Traceback (most recent call last):
  File "/work/seed.py", line 12, in <module>
    insert()
  File "/work/.venv/lib/python3.12/site-packages/pymongo/collection.py", line 1, in insert
    raise ValueError("bad")
ValueError: bad"#;

        assert_eq!(
            first_error(log).as_deref(),
            Some("File \"/work/seed.py\", line 12, in <module>: insert()")
        );
    }

    #[test]
    fn first_error_returns_error_line_when_there_is_no_traceback() {
        assert_eq!(
            first_error("starting\nError: boom\ncleanup"),
            Some("Error: boom".into())
        );
    }

    #[test]
    fn first_error_is_none_for_clean_logs() {
        assert_eq!(first_error("seed started\nseed finished"), None);
    }
}
