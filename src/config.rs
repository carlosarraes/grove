//! The committed `.grove.toml` that tells grove how to run a repo.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

pub const FILENAME: &str = ".grove.toml";

/// How long an instance may sit without a grove command when the repo says nothing.
pub const DEFAULT_STOP_AFTER: &str = "4h";

/// The `[idle] stop_after` value that turns the sweep off.
pub const STOP_AFTER_OFF: &str = "off";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub ports: Ports,
    #[serde(default, rename = "secrets")]
    pub secrets: Vec<Secrets>,
    #[serde(default, rename = "resource")]
    pub resources: Vec<Resource>,
    #[serde(default, rename = "service")]
    pub services: Vec<Service>,
    #[serde(default, rename = "seed")]
    pub seeds: Vec<Seed>,
    /// How long an instance of this repo may run without a grove command before the next
    /// `grove up` or `grove run` stops it. Defaults to `DEFAULT_STOP_AFTER`; `"off"` opts out.
    pub idle: Option<Idle>,
    pub admission: Option<Admission>,
}

/// Optional limits for `run --heavy` and `up --heavy`; ordinary commands do not wait.
/// `full_install` and `link_only` are the `[1-minute, 15-minute]` load limits that
/// `grove gate` and `up --gate` apply, chosen by whether setup would build or only link.
/// A start is admitted when the 1-minute load is under the first limit and either the
/// 15-minute load is under the second or the 1-minute load is under the core count (the
/// machine has recovered; the 15-minute average only lags a finished spike).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Admission {
    pub max_load: Option<f64>,
    pub timeout: String,
    pub full_install: [f64; 2],
    pub link_only: [f64; 2],
}

impl Default for Admission {
    fn default() -> Self {
        Self {
            max_load: None,
            timeout: "10m".to_string(),
            full_install: [25.0, 35.0],
            link_only: [60.0, 60.0],
        }
    }
}

/// The pile-up forms one agent at a time, and `up` and `run` are the moments the agent
/// adding to it is present — so those commands stop the siblings nobody has used, ports
/// kept, so a stopped instance comes back on the same URLs.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Idle {
    /// e.g. `"2h"`, the same grammar as `ready.timeout`, or `"off"`.
    pub stop_after: String,
}

/// Data an instance needs before it is useful — the organisation row a guarded route
/// looks up, a fixture dump. Distinct from a service's `setup`, which installs
/// dependencies: these have different failure modes and different reasons to re-run.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Seed {
    pub name: String,
    /// Working directory, relative to the worktree root.
    pub cwd: Option<String>,
    pub command: String,
    #[serde(default = "seed_timeout")]
    pub timeout: String,
    /// Skip unless this path exists, relative to `cwd`. For fixtures that may not be
    /// present — an LFS dump nobody has pulled should mean "no sample data", not a
    /// failed `up`.
    pub if_exists: Option<String>,
}

fn seed_timeout() -> String {
    "20m".to_string()
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ports {
    /// Port names this repo needs. grove assigns the numbers; templates reference them
    /// as `{{ port.<name> }}`.
    #[serde(default)]
    pub names: Vec<String>,
}

/// One env file: copied from the main checkout, overridden per instance, written into the
/// worktree. The main checkout is the source because a worktree never inherits
/// gitignored files.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Secrets {
    /// Path, relative to the main worktree, of the file to read.
    pub from: String,
    /// Path, relative to this worktree, of the file to write.
    pub into: String,
    /// Keys to override after copying. Values are templates.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resource {
    pub name: String,
    pub kind: ResourceKind,
    pub image: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub port: u16,
    /// Mongo shell command after resource startup; must be safe to retry after failure.
    pub init: Option<String>,
    /// Per-instance database name. A template.
    pub db_name: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ResourceKind {
    /// One container shared by every instance; instances are isolated by database name.
    /// grove reuses anything already answering on `port` rather than starting its own.
    DockerShared,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub name: String,
    /// Working directory, relative to the worktree root.
    pub cwd: Option<String>,
    /// Run once per worktree before the first start, e.g. `uv sync`, `npm install`.
    pub setup: Option<String>,
    /// Files relative to cwd that invalidate an uncached setup marker.
    pub setup_inputs: Option<Vec<String>>,
    /// Run on **every** `up`, before this service starts and after the services declared
    /// above it are answering — e.g. generating a client from this worktree's own backend.
    /// Distinct from `setup` and `[[seed]]`, which run once: generated code has to track
    /// what it was generated from, so "once" is the wrong contract for it.
    pub prepare: Option<String>,
    /// The process to run. Omitted, the service is setup-only: its dependencies are
    /// installed and kept current, and nothing is started — a Pulumi or tooling repo
    /// needs its `.venv` but serves nothing.
    pub command: Option<String>,
    pub ready: Option<Ready>,
    /// What `setup` produces and what decides it, so worktrees on the same inputs share
    /// one copy instead of each installing their own.
    pub cache: Option<Cache>,
}

/// `path` is what `setup` leaves behind, `key` the files that determine its contents —
/// both relative to the service's cwd. Two worktrees whose key files match byte for
/// byte get one tree, hardlinked into each.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cache {
    pub path: String,
    pub key: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ready {
    /// URL polled until it answers.
    pub http: String,
    /// How long to wait, e.g. `180s`.
    pub timeout: String,
}

pub fn load(worktree: &Path) -> Result<Config> {
    let path = worktree.join(FILENAME);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        // The common case in an unconfigured repo. Say what to do about it, so an agent
        // spends one command here instead of a diagnosis.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => anyhow::bail!(
            "no {FILENAME} in {}\n\
             This repo has not been set up for grove yet. \
             Run `grove --llm` for the schema and worked examples, \
             then write {FILENAME} at the main checkout's root: worktrees without \
             their own copy use that one, so it can stay untracked. For a repo that \
             only needs dependencies, such as a .venv, a [[service]] with setup and \
             no command is enough.",
            worktree.display()
        ),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    parse(&text).with_context(|| format!("parsing {}", path.display()))
}

/// The config for a worktree, falling back to the main checkout's copy when this worktree
/// has none and that one does. A repo keeping `.grove.toml` out of git leaves every new
/// worktree without it, since `git worktree add` carries only tracked files — and copying
/// it by hand into each is exactly the setup step grove exists to remove. Says so on
/// stderr: a config read from somewhere other than where the docs say is a surprise.
pub fn load_for(worktree: &Path, main_worktree: &Path) -> Result<Config> {
    let borrowed = worktree != main_worktree
        && !worktree.join(FILENAME).exists()
        && main_worktree.join(FILENAME).exists();
    if !borrowed {
        return load(worktree);
    }
    eprintln!(
        "note: no {FILENAME} in this worktree; using the main checkout's at {}",
        main_worktree.join(FILENAME).display()
    );
    load(main_worktree)
}

pub fn parse(text: &str) -> Result<Config> {
    let config: Config = toml::from_str(text)?;
    config.validate()?;
    Ok(config)
}

impl Config {
    /// How long an instance may go without a grove command before a sibling's `up` or
    /// `run` stops it; None when the repo opted out.
    pub fn idle_window(&self) -> Result<Option<std::time::Duration>> {
        let stop_after = self
            .idle
            .as_ref()
            .map_or(DEFAULT_STOP_AFTER, |idle| idle.stop_after.as_str());
        if stop_after == STOP_AFTER_OFF {
            return Ok(None);
        }
        crate::instance::parse_duration(stop_after)
            .map(Some)
            .with_context(|| format!("[idle] stop_after = {stop_after:?}"))
    }

    fn validate(&self) -> Result<()> {
        for seed in &self.seeds {
            let timeout = crate::instance::parse_duration(&seed.timeout)
                .with_context(|| format!("seed {}: timeout", seed.name))?;
            if timeout.is_zero() {
                anyhow::bail!("seed {}: timeout must be greater than zero", seed.name);
            }
        }
        if let Some(admission) = &self.admission {
            if admission
                .max_load
                .is_some_and(|limit| !limit.is_finite() || limit <= 0.0)
            {
                anyhow::bail!("[admission] max_load must be positive and finite");
            }
            for (name, limits) in [
                ("full_install", admission.full_install),
                ("link_only", admission.link_only),
            ] {
                if limits
                    .iter()
                    .any(|limit| !limit.is_finite() || *limit <= 0.0)
                {
                    anyhow::bail!("[admission] {name} limits must be positive and finite");
                }
            }
            let timeout = crate::instance::parse_duration(&admission.timeout)
                .context("[admission] timeout")?;
            if timeout.is_zero() {
                anyhow::bail!("[admission] timeout must be greater than zero");
            }
        }
        self.idle_window()?;
        for service in &self.services {
            if service.command.is_none() && service.ready.is_some() {
                anyhow::bail!(
                    "service {:?}: ready needs a command; a setup-only service starts nothing",
                    service.name
                );
            }
            if let Some(inputs) = &service.setup_inputs {
                if service.setup.is_none() {
                    anyhow::bail!("service {:?}: setup_inputs requires setup", service.name);
                }
                if service.cache.is_some() {
                    anyhow::bail!(
                        "service {:?}: cannot combine setup_inputs with cache; use cache.key",
                        service.name
                    );
                }
                if inputs.is_empty() {
                    anyhow::bail!(
                        "service {:?}: setup_inputs must name at least one file",
                        service.name
                    );
                }
                for relative in inputs {
                    if relative.is_empty()
                        || relative.starts_with('/')
                        || relative.split('/').any(|part| part == "..")
                    {
                        anyhow::bail!(
                            "service {:?}: setup_inputs path {relative:?} must be relative to the service's cwd and stay inside it",
                            service.name
                        );
                    }
                }
            }
            let Some(cache) = &service.cache else {
                continue;
            };
            // A cache memoizes `setup`. Declaring one without the other would be a
            // silent no-op that leaves the author believing worktrees share a tree.
            if service.setup.is_none() {
                anyhow::bail!(
                    "service {:?} declares a cache but no setup — the cache stores what \
                     setup produces, so there is nothing to store",
                    service.name
                );
            }
            if cache.key.is_empty() {
                anyhow::bail!(
                    "service {:?}: cache.key must name at least one file, or every \
                     worktree would share one tree whatever its lockfile says",
                    service.name
                );
            }
            for relative in std::iter::once(&cache.path).chain(&cache.key) {
                let inside = !relative.is_empty()
                    && !relative.starts_with('/')
                    && !relative.split('/').any(|part| part == "..");
                if !inside {
                    anyhow::bail!(
                        "service {:?}: cache path {relative:?} must be relative to the \
                         service's cwd and stay inside it",
                        service.name
                    );
                }
            }
        }
        for name in &self.ports.names {
            // A port name lands in `{{ port.<name> }}`, where a hyphen parses as
            // subtraction, and in `GROVE_PORT_<NAME>`, which must be a legal shell
            // variable. Both corrupt silently, so the name is constrained up front.
            let legal = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                && !name.starts_with(|c: char| c.is_ascii_digit());
            if !legal {
                anyhow::bail!(
                    "port name {name:?} must be lowercase letters, digits, and underscores, \
                     starting with a letter — it becomes both `{{{{ port.{name} }}}}` in \
                     templates and GROVE_PORT_<NAME> in the environment"
                );
            }
        }
        Ok(())
    }
}
