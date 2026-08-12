//! Shared datastores: one per machine, not one per instance.
//!
//! grove asks the port, never the runtime. The same config has to work where Docker is
//! rootful, where it runs inside a VM, and where the port is simply forwarded from
//! another host — so "is something answering?" is the only question worth asking.

use anyhow::{Context, Result, bail};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use crate::config::Resource;

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
        "nofile=64000:64000".to_string(),
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

/// Start the datastore if nothing is answering, then run its one-time init. Returns
/// whether anything was started, so callers can report it.
pub fn ensure(resource: &Resource) -> Result<bool> {
    match decide(resource) {
        Decision::Reuse { .. } => Ok(false),
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
            Ok(true)
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

fn docker(argv: &[String]) -> Result<String> {
    let out = std::process::Command::new("docker")
        .args(argv)
        .output()
        .context("running docker (is it installed and running?)")?;
    if !out.status.success() {
        bail!(
            "docker {}: {}",
            argv.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
