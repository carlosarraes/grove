//! Everything that has to be true before `up` can work, checked in one pass.
//!
//! Each failure states the fix. An agent that reads "X is missing" and nothing else
//! spends a round trip guessing; one that reads "X is missing, create it at Y" does not.

use anyhow::Result;
use std::fmt;

use crate::instance::Instance;
use crate::resource;
use crate::resource::{NOFILE_LIMIT, Observation};

pub enum Verdict {
    Ok(String),
    Warn(String),
    Fail { what: String, fix: String },
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Verdict::Ok(what) => write!(f, "  ok    {what}"),
            Verdict::Warn(what) => write!(f, "  warn  {what}"),
            Verdict::Fail { what, fix } => write!(f, "  FAIL  {what}\n        {fix}"),
        }
    }
}

pub fn check(instance: &Instance) -> Vec<Verdict> {
    let mut verdicts = Vec::new();

    if instance.resolved.is_main() {
        verdicts.push(Verdict::Fail {
            what: format!(
                "this is the main worktree ({})",
                instance.resolved.worktree.display()
            ),
            fix: "grove reads secrets from here, so it will not write over them. \
                  Run from a linked worktree."
                .to_string(),
        });
    } else {
        verdicts.push(Verdict::Ok(format!(
            "worktree {} off {}",
            instance.resolved.slug,
            instance.resolved.main_worktree.display()
        )));
    }

    if instance.exposure().is_exposed() {
        verdicts.push(Verdict::Warn(format!(
            "instance is exposed on all interfaces as {}; development services and authentication bypasses may be reachable from other machines",
            instance.exposure().public_host()
        )));
    }

    for secrets in &instance.config.secrets {
        let source = instance.resolved.main_worktree.join(&secrets.from);
        if source.is_file() {
            verdicts.push(Verdict::Ok(format!(
                "{} in the main checkout",
                secrets.from
            )));
        } else {
            verdicts.push(Verdict::Fail {
                what: format!("{} is missing from the main checkout", secrets.from),
                fix: format!(
                    "create {} — a worktree never inherits gitignored files, so this is \
                     the only copy grove can read",
                    source.display()
                ),
            });
        }
    }

    for declared in &instance.config.resources {
        verdicts.push(check_resource(declared, resource::observe(declared)));
    }

    for (name, port) in &instance.entry.ports {
        verdicts.push(Verdict::Ok(format!("port {port} reserved for {name}")));
    }

    if instance.config.services.is_empty() {
        verdicts.push(Verdict::Warn(
            "this config declares no services, so `up` will start nothing".to_string(),
        ));
    }

    verdicts
}

fn check_resource(declared: &crate::config::Resource, observed: Observation) -> Verdict {
    let expected = format!("{NOFILE_LIMIT}:{NOFILE_LIMIT}");
    if observed.reachable {
        return match observed.container {
            Some(container)
                if container.running && container.nofile == Some((NOFILE_LIMIT, NOFILE_LIMIT)) =>
            {
                Verdict::Ok(format!(
                    "{} answering on {}, container {}, nofile={expected}",
                    declared.name,
                    declared.port,
                    short_id(&container.id)
                ))
            }
            Some(container) if container.running => {
                let actual = container
                    .nofile
                    .map(|(soft, hard)| format!("{soft}:{hard}"))
                    .unwrap_or_else(|| "not explicitly set".to_string());
                Verdict::Warn(format!(
                    "{} answering on {}, container {} has nofile observed {actual}, expected {expected}. \
                     Preserve needed data, then deliberately remove and recreate it to adopt the limit.",
                    declared.name,
                    declared.port,
                    short_id(&container.id)
                ))
            }
            Some(container) => Verdict::Warn(format!(
                "{} answers on {}, but stopped container {} (exit {}) still owns Grove's name. \
                 Preserve needed data, then remove it before Grove ever needs to recreate the resource.",
                declared.name,
                declared.port,
                short_id(&container.id),
                container.exit_code
            )),
            None => Verdict::Ok(format!(
                "{} answering on {} (external or unobserved; Docker launch limits unavailable{})",
                declared.name,
                declared.port,
                observed
                    .docker_error
                    .as_deref()
                    .map(|error| format!(": {error}"))
                    .unwrap_or_default()
            )),
        };
    }

    if let Some(container) = observed.container {
        let logs = resource::logs(declared, 10)
            .map(|body| body.trim().to_string())
            .unwrap_or_else(|error| format!("resource logs unavailable: {error:#}"));
        let state = if container.running {
            "running"
        } else {
            "stopped"
        };
        return Verdict::Fail {
            what: format!(
                "{} is not answering on {}; managed container {} is {state} (exit {})",
                declared.name,
                declared.port,
                short_id(&container.id),
                container.exit_code
            ),
            fix: format!(
                "the existing container name blocks `grove up`; first preserve any needed data, then remove \
                 and recreate the container. Last resource log lines:\n{logs}"
            ),
        };
    }

    if let Some(error) = observed.docker_error
        && declared.image.is_some()
    {
        return Verdict::Fail {
            what: format!(
                "{} is not answering on {}, and Docker cannot inspect or start it",
                declared.name, declared.port
            ),
            fix: format!("make Docker available, then run `grove up` again: {error}"),
        };
    }

    if let Some(image) = &declared.image {
        Verdict::Warn(format!(
            "{} is not running; `grove up` will start {image} on port {}",
            declared.name, declared.port
        ))
    } else {
        Verdict::Fail {
            what: format!("{} is not answering on {}", declared.name, declared.port),
            fix: format!(
                "start it yourself — `{}` declares no image for grove to run",
                declared.name
            ),
        }
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(12).collect()
}

/// Print the report and return whether everything needed to start is in place.
pub fn report(verdicts: &[Verdict]) -> Result<bool> {
    let mut healthy = true;
    for verdict in verdicts {
        println!("{verdict}");
        if matches!(verdict, Verdict::Fail { .. }) {
            healthy = false;
        }
    }
    Ok(healthy)
}
