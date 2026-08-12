//! Shared datastores: one per machine, not one per instance.
//!
//! grove asks the port, never the runtime. The same config has to work where Docker is
//! rootful, where it runs inside a VM, and where the port is simply forwarded from
//! another host — so "is something answering?" is the only question worth asking.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::ffi::OsString;
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use crate::config::Resource;

pub const NOFILE_LIMIT: u64 = 64_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerObservation {
    pub id: String,
    pub running: bool,
    pub exit_code: i64,
    pub nofile: Option<(u64, u64)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    pub reachable: bool,
    pub container: Option<ContainerObservation>,
    pub docker_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnsureResult {
    pub started: bool,
    pub observation: Observation,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// Something already answers on the port. Use it and start nothing.
    Reuse { port: u16 },
    /// Nothing answers. These are the arguments to hand `docker`.
    Start(Vec<String>),
}

pub fn container_name(resource: &Resource) -> String {
    format!("grove-{}", resource.name)
}

pub fn decide(resource: &Resource) -> Decision {
    if is_reachable(resource.port) {
        return Decision::Reuse {
            port: resource.port,
        };
    }

    let mut argv = vec![
        "run".to_string(),
        "-d".to_string(),
        "--ulimit".to_string(),
        format!("nofile={NOFILE_LIMIT}:{NOFILE_LIMIT}"),
        "--name".to_string(),
        container_name(resource),
        "-p".to_string(),
        format!("{p}:{p}", p = resource.port),
    ];
    if let Some(image) = &resource.image {
        argv.push(image.clone());
    }
    // Anything after the image is the container's own command. `--replSet rs0` is
    // mongod's flag, not docker's; placing it earlier makes docker reject it.
    argv.extend(resource.args.iter().cloned());
    Decision::Start(argv)
}

pub fn is_reachable(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// Observe the port Grove promises to reuse and, independently, the exact container name
/// Grove claims when it starts this resource. Docker is diagnostic here: an external
/// resource remains valid when its port answers and no container can be inspected.
pub fn observe(resource: &Resource) -> Observation {
    let reachable = is_reachable(resource.port);
    let argv = ["inspect".to_string(), container_name(resource)];
    match docker_output(&argv) {
        Ok(out) if out.status.success() => {
            match parse_inspect(&String::from_utf8_lossy(&out.stdout)) {
                Ok(container) => Observation {
                    reachable,
                    container,
                    docker_error: None,
                },
                Err(error) => Observation {
                    reachable,
                    container: None,
                    docker_error: Some(format!("reading docker inspect: {error:#}")),
                },
            }
        }
        Ok(out) => {
            let error = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if is_missing_container(&error) {
                Observation {
                    reachable,
                    container: None,
                    docker_error: None,
                }
            } else {
                Observation {
                    reachable,
                    container: None,
                    docker_error: Some(error),
                }
            }
        }
        Err(error) => Observation {
            reachable,
            container: None,
            docker_error: Some(format!("{error:#}")),
        },
    }
}

/// Start the datastore if nothing is answering, then run its one-time init. Returns
/// whether anything was started, so callers can report it.
pub fn ensure(resource: &Resource) -> Result<EnsureResult> {
    match decide(resource) {
        Decision::Reuse { .. } => Ok(EnsureResult {
            started: false,
            observation: observe(resource),
        }),
        Decision::Start(argv) => {
            if resource.image.is_none() {
                bail!(
                    "nothing is answering on port {} and resource `{}` declares no image to start",
                    resource.port,
                    resource.name
                );
            }
            docker(&argv).with_context(|| {
                format!(
                    "starting `{}` on port {}. If you provide {} yourself, start it and \
                     run `grove up` again.",
                    resource.name, resource.port, resource.name
                )
            })?;
            wait_reachable(resource)?;

            if let Some(init) = &resource.init {
                // Best effort: on a container that was already initialised this fails
                // harmlessly, and there is no cheap way to distinguish that from a real
                // problem without knowing the datastore.
                let _ = docker(&[
                    "exec".to_string(),
                    container_name(resource),
                    "mongosh".to_string(),
                    "--quiet".to_string(),
                    "--eval".to_string(),
                    init.clone(),
                ]);
            }
            Ok(EnsureResult {
                started: true,
                observation: observe(resource),
            })
        }
    }
}

fn wait_reachable(resource: &Resource) -> Result<()> {
    for _ in 0..100 {
        if is_reachable(resource.port) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!(
        "`{}` was started but never answered on port {}",
        resource.name,
        resource.port
    )
}

/// Arguments for a host `mongosh` addressed at the datastore's port. The port is the one
/// thing grove always knows: `ensure` found the datastore by asking it, and dropping has
/// to work for a datastore grove never started.
pub fn drop_database_command(resource: &Resource, database: &str) -> Vec<String> {
    vec![
        "--quiet".to_string(),
        format!("mongodb://localhost:{}", resource.port),
        "--eval".to_string(),
        format!("db.getSiblingDB('{database}').dropDatabase()"),
    ]
}

/// Drop an instance's database. Best effort by design: the datastore may be one grove
/// did not start and cannot reach with `docker exec`, and failing `down` over a leftover
/// database would be worse than leaving it. Says what to run by hand either way.
pub fn drop_database(resource: &Resource, database: &str) -> Result<()> {
    let argv = drop_database_command(resource, database);

    // A host client first: it reaches the datastore however it is provided.
    if let Ok(out) = std::process::Command::new("mongosh").args(&argv).output()
        && out.status.success()
    {
        return Ok(());
    }

    // Then a container grove started itself, for machines with no client installed.
    let mut inside = vec![
        "exec".to_string(),
        container_name(resource),
        "mongosh".to_string(),
    ];
    inside.extend(argv.iter().cloned());
    if docker(&inside).is_ok() {
        return Ok(());
    }

    bail!(
        "could not drop `{database}` automatically — no mongosh on PATH, and no container \
         grove started to run one in. Drop it with:\n  \
         mongosh mongodb://localhost:{} --eval \"db.getSiblingDB('{database}').dropDatabase()\"",
        resource.port
    )
}

/// The last resource log lines, used only when doctor has already found the managed
/// container unusable. Keeping this here means callers never learn Docker's argv shape.
pub fn logs(resource: &Resource, lines: usize) -> Result<String> {
    docker(&[
        "logs".to_string(),
        "--tail".to_string(),
        lines.to_string(),
        container_name(resource),
    ])
}

#[derive(Deserialize)]
struct DockerInspect {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "State")]
    state: DockerState,
    #[serde(rename = "HostConfig")]
    host_config: DockerHostConfig,
}

#[derive(Deserialize)]
struct DockerState {
    #[serde(rename = "Running")]
    running: bool,
    #[serde(rename = "ExitCode")]
    exit_code: i64,
}

#[derive(Deserialize)]
struct DockerHostConfig {
    #[serde(rename = "Ulimits", default)]
    ulimits: Option<Vec<DockerUlimit>>,
}

#[derive(Deserialize)]
struct DockerUlimit {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Soft")]
    soft: u64,
    #[serde(rename = "Hard")]
    hard: u64,
}

fn parse_inspect(json: &str) -> Result<Option<ContainerObservation>> {
    let mut inspected: Vec<DockerInspect> = serde_json::from_str(json)?;
    let Some(inspected) = inspected.pop() else {
        return Ok(None);
    };
    let nofile = inspected
        .host_config
        .ulimits
        .unwrap_or_default()
        .into_iter()
        .find(|limit| limit.name == "nofile")
        .map(|limit| (limit.soft, limit.hard));
    Ok(Some(ContainerObservation {
        id: inspected.id,
        running: inspected.state.running,
        exit_code: inspected.state.exit_code,
        nofile,
    }))
}

fn is_missing_container(error: &str) -> bool {
    error.contains("No such object") || error.contains("No such container")
}

fn docker_program() -> OsString {
    std::env::var_os("GROVE_DOCKER").unwrap_or_else(|| "docker".into())
}

fn docker_output(argv: &[String]) -> Result<std::process::Output> {
    std::process::Command::new(docker_program())
        .args(argv)
        .output()
        .context("running docker (is it installed and running?)")
}

fn docker(argv: &[String]) -> Result<String> {
    let out = docker_output(argv)?;
    if !out.status.success() {
        bail!(
            "docker {}: {}",
            argv.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::parse_inspect;

    #[test]
    fn inspect_reads_identity_state_exit_and_nofile() {
        let json = r#"[{"Id":"abcdef0123456789","State":{"Running":false,"ExitCode":133},"HostConfig":{"Ulimits":[{"Name":"nofile","Soft":64000,"Hard":64000}]}}]"#;

        let found = parse_inspect(json).expect("inspect").expect("container");

        assert_eq!(found.id, "abcdef0123456789");
        assert!(!found.running);
        assert_eq!(found.exit_code, 133);
        assert_eq!(found.nofile, Some((64_000, 64_000)));
    }

    #[test]
    fn inspect_accepts_a_container_without_an_explicit_nofile() {
        let json =
            r#"[{"Id":"old","State":{"Running":true,"ExitCode":0},"HostConfig":{"Ulimits":[]}}]"#;

        let found = parse_inspect(json).expect("inspect").expect("container");

        assert_eq!(found.nofile, None);
    }
}
