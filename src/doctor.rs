//! Everything that has to be true before `up` can work, checked in one pass.
//!
//! Each failure states the fix. An agent that reads "X is missing" and nothing else
//! spends a round trip guessing; one that reads "X is missing, create it at Y" does not.

use anyhow::Result;
use std::fmt;

use crate::instance::Instance;
use crate::resource;

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
            fix: "treeish reads secrets from here, so it will not write over them. \
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
                     the only copy treeish can read",
                    source.display()
                ),
            });
        }
    }

    for declared in &instance.config.resources {
        if resource::is_reachable(declared.port) {
            verdicts.push(Verdict::Ok(format!(
                "{} answering on {}",
                declared.name, declared.port
            )));
        } else if declared.image.is_some() {
            verdicts.push(Verdict::Warn(format!(
                "{} is not running; `treeish up` will start {} on port {}",
                declared.name,
                declared.image.as_deref().unwrap_or("it"),
                declared.port
            )));
        } else {
            verdicts.push(Verdict::Fail {
                what: format!("{} is not answering on {}", declared.name, declared.port),
                fix: format!(
                    "start it yourself — `{}` declares no image for treeish to run",
                    declared.name
                ),
            });
        }
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
