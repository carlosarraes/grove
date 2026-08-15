//! The committed `.grove.toml` that tells grove how to run a repo.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

pub const FILENAME: &str = ".grove.toml";

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
    /// Skip unless this path exists, relative to `cwd`. For fixtures that may not be
    /// present — an LFS dump nobody has pulled should mean "no sample data", not a
    /// failed `up`.
    pub if_exists: Option<String>,
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
    /// Command run once against a freshly started resource, e.g. `rs.initiate()`.
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub name: String,
    /// Working directory, relative to the worktree root.
    pub cwd: Option<String>,
    /// Run once per worktree before the first start, e.g. `uv sync`, `npm install`.
    pub setup: Option<String>,
    /// Run on **every** `up`, before this service starts and after the services declared
    /// above it are answering — e.g. generating a client from this worktree's own backend.
    /// Distinct from `setup` and `[[seed]]`, which run once: generated code has to track
    /// what it was generated from, so "once" is the wrong contract for it.
    pub prepare: Option<String>,
    pub command: String,
    pub ready: Option<Ready>,
}

#[derive(Debug, Deserialize)]
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
             then write {FILENAME} at the worktree root and commit it.",
            worktree.display()
        ),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    parse(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn parse(text: &str) -> Result<Config> {
    let config: Config = toml::from_str(text)?;
    config.validate()?;
    Ok(config)
}

impl Config {
    fn validate(&self) -> Result<()> {
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
