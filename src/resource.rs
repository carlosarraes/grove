//! Shared datastores: one per machine, not one per instance.
//!
//! grove asks the port, never the runtime. The same config has to work where Docker is
//! rootful, where it runs inside a VM, and where the port is simply forwarded from
//! another host — so "is something answering?" is the only question worth asking.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
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
    pub image: Option<String>,
    pub cmd: Option<Vec<String>>,
    pub port_bindings: BTreeMap<String, Option<Vec<PortBinding>>>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct PortBinding {
    #[serde(rename = "HostIp")]
    pub host_ip: String,
    #[serde(rename = "HostPort")]
    pub host_port: String,
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
    pub created: bool,
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

/// Serialize startup and finish any initialization left pending by an earlier up.
/// A reachable external datastore remains usable without Docker initialization.
pub fn ensure(resource: &Resource) -> Result<EnsureResult> {
    let key: String = resource
        .name
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let directory = crate::instance::state_dir()?.join("resources").join(key);
    std::fs::create_dir_all(&directory)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(directory.join("init.lock"))?;
    crate::timing::measure_wait(
        &format!("resource {}: init lock wait", resource.name),
        || Ok(lock.lock()?),
    )?;
    let pending_path = directory.join("pending-init.json");
    let result = ensure_started(resource, &pending_path)?;
    if let Some(container) = &result.observation.container
        && let Some(drift) = args_drift(resource, container)
    {
        eprintln!(
            "warning: {}: {drift}. Preserve needed data, then deliberately remove and recreate it to adopt the args.",
            resource.name
        );
    }
    let pending: Option<String> = match std::fs::read(&pending_path) {
        Ok(bytes) => Some(
            serde_json::from_slice(&bytes).context("reading pending resource initialization")?,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("reading pending resource initialization"),
    };
    let container = result.observation.container.as_ref();
    let retry = pending
        .as_ref()
        .is_some_and(|id| container.is_some_and(|c| &c.id == id));
    if pending.is_some() && result.observation.docker_error.is_some() {
        bail!(
            "cannot verify pending initialization for resource {}: {}",
            resource.name,
            result
                .observation
                .docker_error
                .as_deref()
                .unwrap_or_default()
        );
    }
    if retry || ((result.started || result.created) && resource.init.is_some()) {
        let init = resource.init.as_ref().with_context(|| {
            format!(
                "resource {} has pending initialization but no init command",
                resource.name
            )
        })?;
        let container = container.with_context(|| {
            format!(
                "cannot identify managed container for initialization of {}",
                resource.name
            )
        })?;
        validate_existing(resource, container)?;
        if !container.running {
            bail!(
                "managed container for {} is not running; refusing initialization against an external listener",
                resource.name
            );
        }
        run_init(resource, &container.id, init).with_context(|| {
            format!(
                "initializing resource {}; initialization remains pending for the next up",
                resource.name
            )
        })?;
        std::fs::remove_file(&pending_path)
            .context("clearing completed resource initialization")?;
    }
    Ok(result)
}

/// How long init may keep meeting a mongod that is not listening yet while its container
/// stays up. Docker's port forwarder answers on the host before the process inside does,
/// and a mongod recovering from an unclean shutdown (an OOM kill, a host that slept
/// through it) replays its journal before it listens: 90s on a 10G dataset, observed.
/// A container that exits ends the wait at once, so the ceiling only bounds a hang.
const INIT_READY_TIMEOUT: Duration = Duration::from_secs(600);

fn run_init(resource: &Resource, id: &str, init: &str) -> Result<()> {
    let mut argv = vec![
        "exec".to_string(),
        // The mongo image sets HOME=/data/db, so mongosh would write and rotate its logs
        // under the data directory while the entrypoint, starting as root, chowns that
        // directory recursively. A file vanishing mid-chown exits the entrypoint 1.
        "-e".to_string(),
        "HOME=/tmp".to_string(),
        id.to_string(),
        "mongosh".to_string(),
        "--quiet".to_string(),
    ];
    // Init runs inside the container, so it must reach mongod where `args` moved it.
    if let Some(port) = mongod_port(&resource.args) {
        argv.extend(["--port".to_string(), port]);
    }
    argv.extend(["--eval".to_string(), init.to_string()]);
    let began = std::time::Instant::now();
    crate::timing::with_heartbeat(
        crate::timing::heartbeat_interval(),
        |elapsed| {
            eprintln!(
                "{}: still starting inside its container ({}s), waiting for mongosh initialization; after an unclean shutdown mongod recovers its journal before it listens",
                resource.name,
                elapsed.as_secs()
            )
        },
        || loop {
            match docker(&argv) {
                Ok(_) => return Ok(()),
                Err(error) => {
                    if is_mongo_already_initialized(&format!("{error:#}")) {
                        return Ok(());
                    }
                    if let Some(code) = exited(id) {
                        return Err(error).with_context(|| {
                            format!(
                                "`{}` exited with code {code} before it accepted connections; \
                                 `docker logs --tail 50 {}` shows why",
                                resource.name,
                                container_name(resource)
                            )
                        });
                    }
                    if !is_not_listening(&format!("{error:#}")) {
                        return Err(error);
                    }
                    let waited = began.elapsed();
                    if waited >= INIT_READY_TIMEOUT {
                        return Err(error).with_context(|| {
                            format!(
                                "`{}` was still refusing connections inside its container after {}s",
                                resource.name,
                                INIT_READY_TIMEOUT.as_secs()
                            )
                        });
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        },
    )
}

/// The exit code of a container Docker reports as stopped. Anything short of that
/// answer, including Docker failing to say, keeps the wait going.
fn exited(id: &str) -> Option<i64> {
    let out = docker_output_unmonitored(&["inspect".to_string(), id.to_string()]).ok()?;
    if !out.status.success() {
        return None;
    }
    let container = parse_inspect(&String::from_utf8_lossy(&out.stdout)).ok()??;
    (!container.running).then_some(container.exit_code)
}

/// The `--port N` or `--port=N` mongod is given in `args`; mongosh's default otherwise.
fn mongod_port(args: &[String]) -> Option<String> {
    args.iter()
        .enumerate()
        .find_map(|(i, arg)| match arg.strip_prefix("--port") {
            Some("") => args.get(i + 1).cloned(),
            Some(rest) => rest.strip_prefix('=').map(str::to_string),
            None => None,
        })
}

fn is_not_listening(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("econnrefused") || error.contains("connection refused")
}

fn is_mongo_already_initialized(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("already initialized")
        || error.contains("alreadyinitialized")
        || ["code: 23", "code:23"].iter().any(|marker| {
            error.match_indices(marker).any(|(offset, marker)| {
                error
                    .as_bytes()
                    .get(offset + marker.len())
                    .is_none_or(|byte| !byte.is_ascii_digit())
            })
        })
}

/// Configured args the managed container is not running with. No configured args
/// means the image's default command, which is never drift.
pub fn args_drift(resource: &Resource, container: &ContainerObservation) -> Option<String> {
    if resource.args.is_empty() || container.cmd.as_deref() == Some(resource.args.as_slice()) {
        return None;
    }
    if container.cmd.as_ref().is_some_and(|cmd| {
        cmd.windows(resource.args.len())
            .any(|window| window == resource.args)
    }) {
        return None;
    }
    let actual = container
        .cmd
        .as_ref()
        .map(|cmd| cmd.join(" "))
        .unwrap_or_else(|| "the image default".to_string());
    Some(format!(
        "container {} runs with args `{actual}`, expected `{}`",
        container.id.chars().take(12).collect::<String>(),
        resource.args.join(" ")
    ))
}

fn record_pending(path: &std::path::Path, id: &str) -> Result<()> {
    use std::io::Write;
    let temporary = path.with_extension("tmp");
    let mut record = std::fs::File::create(&temporary)?;
    record.write_all(&serde_json::to_vec(id)?)?;
    record.sync_all()?;
    std::fs::rename(&temporary, path).context("publishing pending resource initialization")
}

fn ensure_started(resource: &Resource, pending: &std::path::Path) -> Result<EnsureResult> {
    match decide(resource) {
        Decision::Reuse { .. } => Ok(EnsureResult {
            started: false,
            created: false,
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
            let existing = observe(resource);
            if existing.reachable {
                return Ok(EnsureResult {
                    started: false,
                    created: false,
                    observation: existing,
                });
            }
            if let Some(error) = existing.docker_error {
                bail!(
                    "cannot inspect `{}` before starting it: {error}",
                    container_name(resource)
                );
            }
            let command = match &existing.container {
                Some(container) => {
                    validate_existing(resource, container)?;
                    if container.running {
                        // Another up can have started it before the port becomes ready.
                        None
                    } else {
                        eprintln!(
                            "{}: its container had stopped (exit code {}{}); starting it",
                            resource.name,
                            container.exit_code,
                            if container.exit_code == 137 {
                                ", killed: often out of memory"
                            } else {
                                ""
                            }
                        );
                        Some(vec!["start".to_string(), container.id.clone()])
                    }
                }
                None => Some(argv),
            };
            // A stopped container's identity is already known. Record the work
            // before start so even a failed start or readiness wait remains retryable.
            if resource.init.is_some()
                && let Some(container) = &existing.container
            {
                record_pending(pending, &container.id)?;
            }
            if let Some(command) = &command {
                let output = docker(command).with_context(|| {
                    format!(
                        "starting `{}` on port {}. If you provide {} yourself, start it and \
                     run `grove up` again.",
                        resource.name, resource.port, resource.name
                    )
                })?;
                if resource.init.is_some() && existing.container.is_none() {
                    if output.is_empty() {
                        bail!("Docker returned no container ID for {}", resource.name);
                    }
                    record_pending(pending, &output)?;
                }
            }
            wait_reachable(resource)?;

            Ok(EnsureResult {
                started: command.is_some(),
                created: existing.container.is_none(),
                observation: observe(resource),
            })
        }
    }
}

fn validate_existing(resource: &Resource, container: &ContainerObservation) -> Result<()> {
    if container.image.as_deref() != resource.image.as_deref() {
        bail!(
            "existing container `{}` image does not match configured image; preserve its data and reconcile the resource configuration",
            container_name(resource)
        );
    }
    if let Some(drift) = args_drift(resource, container) {
        bail!(
            "existing container `{}` {drift}; preserve its data and reconcile the resource configuration",
            container_name(resource)
        );
    }
    let key = format!("{}/tcp", resource.port);
    let expected_port = resource.port.to_string();
    let bindings = container.port_bindings.get(&key).and_then(Option::as_ref);
    let matches = container.port_bindings.len() == 1
        && bindings.is_some_and(|bindings| {
            bindings
                .iter()
                .any(|binding| matches!(binding.host_ip.as_str(), "" | "0.0.0.0" | "127.0.0.1"))
                && bindings.iter().all(|binding| {
                    binding.host_port == expected_port
                        && matches!(
                            binding.host_ip.as_str(),
                            "" | "0.0.0.0" | "127.0.0.1" | "::" | "::1"
                        )
                })
        });
    if !matches {
        bail!(
            "existing container `{}` port binding does not match configured port {}; preserve its data and reconcile the resource configuration",
            container_name(resource),
            resource.port
        );
    }
    Ok(())
}

fn wait_reachable(resource: &Resource) -> Result<()> {
    crate::timing::progress(
        &format!(
            "resource {}: waiting for host port {}",
            resource.name, resource.port
        ),
        || {
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
        },
    )
}

/// Arguments for a host `mongosh` addressed at the datastore's port. The port is the one
/// thing grove always knows: `ensure` found the datastore by asking it, and dropping has
/// to work for a datastore grove never started.
pub fn drop_database_command(resource: &Resource, database: &str) -> Vec<String> {
    drop_database_command_at(resource.port, database)
}

pub fn drop_database_command_at(port: u16, database: &str) -> Vec<String> {
    vec![
        "--quiet".to_string(),
        format!("mongodb://localhost:{port}"),
        "--eval".to_string(),
        format!("db.getSiblingDB('{database}').dropDatabase()"),
    ]
}

/// What to tell someone whose database grove cannot reach for itself.
pub fn drop_database_hint(port: u16, database: &str) -> String {
    format!(
        "mongosh mongodb://localhost:{port} --eval \
         \"db.getSiblingDB('{database}').dropDatabase()\""
    )
}

/// Drop an instance's database. Best effort by design: the datastore may be one grove
/// did not start and cannot reach with `docker exec`, and failing `down` over a leftover
/// database would be worse than leaving it. Says what to run by hand either way.
pub fn drop_database(resource: &Resource, database: &str) -> Result<()> {
    drop_database_at(&resource.name, resource.port, database)
}

/// Drop by coordinates rather than by config, so `prune` can reach a database whose
/// worktree — and whose `.grove.toml` — has already been deleted.
pub fn drop_database_at(name: &str, port: u16, database: &str) -> Result<()> {
    let argv = drop_database_command_at(port, database);

    // A host client first: it reaches the datastore however it is provided.
    if let Ok(out) = std::process::Command::new("mongosh").args(&argv).output()
        && out.status.success()
    {
        return Ok(());
    }

    // Then a container grove started itself, for machines with no client installed.
    let mut inside = vec![
        "exec".to_string(),
        format!("grove-{name}"),
        "mongosh".to_string(),
    ];
    inside.extend(argv.iter().cloned());
    if docker(&inside).is_ok() {
        return Ok(());
    }

    bail!(
        "could not drop `{database}` automatically — no mongosh on PATH, and no container \
         grove started to run one in. Drop it with:\n  {}",
        drop_database_hint(port, database)
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
    #[serde(rename = "Config", default)]
    config: Option<DockerConfig>,
}

#[derive(Deserialize)]
struct DockerConfig {
    #[serde(rename = "Image")]
    image: String,
    #[serde(rename = "Cmd", default)]
    cmd: Option<Vec<String>>,
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
    #[serde(rename = "PortBindings", default)]
    port_bindings: Option<BTreeMap<String, Option<Vec<PortBinding>>>>,
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
        cmd: inspected
            .config
            .as_ref()
            .and_then(|config| config.cmd.clone()),
        image: inspected.config.map(|config| config.image),
        port_bindings: inspected.host_config.port_bindings.unwrap_or_default(),
    }))
}

/// Docker CLIs before 29 say "Error: No such object"; 29+ say "error: no such object".
fn is_missing_container(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("no such object") || error.contains("no such container")
}

fn docker_program() -> OsString {
    std::env::var_os("GROVE_DOCKER").unwrap_or_else(|| "docker".into())
}

fn docker_output(argv: &[String]) -> Result<std::process::Output> {
    // Resource init has one heartbeat across all mongosh attempts and retries.
    if argv.first().is_some_and(|arg| arg == "exec") {
        docker_output_unmonitored(argv)
    } else {
        let operation = argv.first().map(String::as_str).unwrap_or("command");
        crate::timing::progress(&format!("shared resource: docker {operation}"), || {
            docker_output_unmonitored(argv)
        })
    }
}

fn docker_output_unmonitored(argv: &[String]) -> Result<std::process::Output> {
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
            docker_error_message(
                &String::from_utf8_lossy(&out.stderr),
                &String::from_utf8_lossy(&out.stdout)
            )
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn docker_error_message<'a>(stderr: &'a str, stdout: &'a str) -> &'a str {
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        stderr
    } else {
        let stdout = stdout.trim();
        if stdout.is_empty() {
            "no output"
        } else {
            stdout
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{docker_error_message, ensure, is_mongo_already_initialized, parse_inspect};
    use crate::config::{Resource, ResourceKind};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn ensure_with_init_error(message: &str) -> (anyhow::Result<super::EnsureResult>, bool) {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "grove-resource-init-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let docker = root.join("docker");
        std::fs::write(
            &docker,
            format!(
                "#!/bin/sh\ncase \"$1\" in\n  inspect) printf '%s' '[{{\"Id\":\"container-id\",\"State\":{{\"Running\":true,\"ExitCode\":0}},\"Config\":{{\"Image\":\"mongo:8.0\"}},\"HostConfig\":{{\"Ulimits\":[],\"PortBindings\":{{\"27017/tcp\":[{{\"HostIp\":\"127.0.0.1\",\"HostPort\":\"27017\"}}]}}}}}}]' ;;\n  exec) echo '{}' >&2; exit 1 ;;\n  *) exit 1 ;;\nesac\n",
                message
            ),
        )
        .unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();

        let resource = Resource {
            name: "mongo".to_string(),
            kind: ResourceKind::DockerShared,
            image: Some("mongo:8.0".to_string()),
            args: Vec::new(),
            port: 27017,
            init: Some("rs.initiate()".to_string()),
            db_name: None,
        };
        let key = resource
            .name
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let pending = root.join("resources").join(key).join("pending-init.json");
        std::fs::create_dir_all(pending.parent().unwrap()).unwrap();
        std::fs::write(&pending, "\"container-id\"").unwrap();

        let previous_state = std::env::var_os("GROVE_STATE_DIR");
        let previous_docker = std::env::var_os("GROVE_DOCKER");
        unsafe {
            std::env::set_var("GROVE_STATE_DIR", &root);
            std::env::set_var("GROVE_DOCKER", &docker);
        }
        let result = ensure(&resource);
        unsafe {
            if let Some(value) = previous_state {
                std::env::set_var("GROVE_STATE_DIR", value);
            } else {
                std::env::remove_var("GROVE_STATE_DIR");
            }
            if let Some(value) = previous_docker {
                std::env::set_var("GROVE_DOCKER", value);
            } else {
                std::env::remove_var("GROVE_DOCKER");
            }
        }
        let pending_exists = pending.exists();
        std::fs::remove_dir_all(root).unwrap();
        (result, pending_exists)
    }

    #[test]
    fn ensure_clears_pending_after_mongo_already_initialized() {
        let (result, pending_exists) =
            ensure_with_init_error("MongoServerError: already initialized");

        assert!(result.is_ok(), "{result:?}");
        assert!(!pending_exists);
    }

    #[test]
    fn ensure_keeps_pending_after_other_init_errors() {
        let (result, pending_exists) = ensure_with_init_error("MongoServerError: disk failure");

        assert!(result.is_err());
        assert!(pending_exists);
    }

    #[test]
    fn docker_failure_uses_stderr_then_stdout_then_empty_message() {
        assert_eq!(docker_error_message("error", "result"), "error");
        assert_eq!(docker_error_message("  ", " result\n"), "result");
        assert_eq!(docker_error_message("", ""), "no output");
    }

    #[test]
    fn recognizes_mongo_already_initialized_errors_only() {
        assert!(is_mongo_already_initialized(
            "MongoServerError: already initialized"
        ));
        assert!(is_mongo_already_initialized("AlreadyInitialized"));
        assert!(is_mongo_already_initialized("MongoServerError code: 23"));
        assert!(!is_mongo_already_initialized("MongoServerError code: 230"));
        assert!(!is_mongo_already_initialized("MongoServerError: disk full"));
    }

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
