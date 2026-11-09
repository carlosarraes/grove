//! Remove idle, grove-linked git worktrees, with a read-only default.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Output};

use crate::footprint::{human_size, unique_reclaimable};
use crate::registry::{Entry, Registry};
use crate::{instance, last_up, resolve, supervise};

pub fn run(cwd: &Path, selectors: &[String], yes: bool) -> Result<()> {
    let registry = instance::registry()?;
    let entries = registry.list()?;
    let mut chosen = BTreeMap::new();
    if selectors.is_empty() {
        for entry in entries {
            if entry.worktree.try_exists()? {
                chosen.insert(entry.worktree.clone(), entry);
            }
        }
    } else {
        for selector in selectors {
            let path = cwd.join(selector);
            let matches: Vec<_> = entries
                .iter()
                .filter(|e| e.slug == *selector || e.worktree == path)
                .collect();
            let entry = match matches.as_slice() {
                [entry] => *entry,
                [] => bail!(
                    "no instance named or recorded at `{selector}`; `grove ls` lists instances"
                ),
                _ => bail!("multiple instances match `{selector}`; select a worktree path instead"),
            };
            if entry.worktree.try_exists()? {
                chosen.insert(entry.worktree.clone(), entry.clone());
            }
        }
    }

    let (mut removable, mut skipped, mut removed, mut total) = (0, 0, 0, 0u64);
    for (path, entry) in chosen {
        let (mut reason, mut lock) = inspect(&entry, &registry, false)?;
        let bytes = unique_reclaimable(&path);
        let size = human_size(bytes);
        if yes && reason.is_none() {
            (reason, lock) = inspect(&entry, &registry, true)?;
        }
        if let Some(reason) = reason {
            println!("skip {} {size}  {reason}", path.display());
            skipped += 1;
            continue;
        }
        removable += 1;
        if yes {
            let resolved = resolve::resolve(&path)?;
            let output = Command::new("git")
                .current_dir(&resolved.main_worktree)
                .args(["worktree", "remove"])
                .arg(&path)
                .output()
                .with_context(|| format!("remove {}", path.display()))?;
            if !output.status.success() {
                println!("skip {} {size}  removal failed", path.display());
                footer(removable, skipped, removed, total, yes);
                bail!(
                    "git worktree remove {} failed: {}",
                    path.display(),
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            // The path is normally gone; its pre-removal private blocks are the freed bytes.
            let freed = if path.try_exists()? {
                bytes.saturating_sub(unique_reclaimable(&path))
            } else {
                bytes
            };
            registry.release(&path)?;
            removed += 1;
            total += freed;
            println!(
                "ok   {} {size}  removed, freed {}",
                path.display(),
                human_size(freed)
            );
        } else {
            total += bytes;
            println!("ok   {} {size}  would free {size}", path.display());
        }
        drop(lock);
    }
    footer(removable, skipped, removed, total, yes);
    Ok(())
}

fn footer(removable: usize, skipped: usize, removed: usize, bytes: u64, yes: bool) {
    let action = if yes { "freed" } else { "would free" };
    println!(
        "{removable} removable, {skipped} skipped, {removed} removed; {action} {} unique bytes",
        human_size(bytes)
    );
}

fn git(path: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .with_context(|| format!("git {} in {}", args.join(" "), path.display()))
}

fn inspect(
    entry: &Entry,
    registry: &Registry,
    retain_lock: bool,
) -> Result<(Option<String>, Option<File>)> {
    let path = &entry.worktree;
    let resolved = resolve::resolve(path)?;
    let skip = |reason: &str| Ok((Some(reason.to_owned()), None));
    if resolved.is_main() {
        return skip("primary worktree");
    }
    let directory = instance::state_dir()?
        .join(resolved.state_key())
        .join(&resolved.slug);
    let lock = match OpenOptions::new()
        .write(true)
        .open(directory.join("up.lock"))
    {
        Ok(file) => match file.try_lock() {
            Ok(()) if retain_lock => Some(file),
            Ok(()) => None,
            Err(TryLockError::WouldBlock) => return skip("instance lock held"),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        },
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if last_up::read(&directory).is_some_and(|last| last.running()) {
        return skip("grove up is running");
    }
    let fresh = registry
        .get(path)?
        .context("instance disappeared from registry")?;
    if fresh.services.values().any(supervise::is_alive) {
        return skip("running stack");
    }
    let output = match Command::new("lsof").args(["-t", "+D"]).arg(path).output() {
        Ok(output) => output,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return skip("lsof not found; refusing to delete");
        }
        Err(error) => return Err(error.into()),
    };
    let pids: Vec<u32> = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .map(str::parse::<u32>)
        .collect::<std::result::Result<_, _>>()?;
    let pids: Vec<_> = pids
        .into_iter()
        .filter(|pid| *pid != std::process::id())
        .collect();
    if !pids.is_empty() {
        return skip(&format!(
            "open files (pid {})",
            pids.iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !output.status.success() && !(output.status.code() == Some(1) && output.stdout.is_empty()) {
        bail!(
            "lsof failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let status = git(path, &["status", "--porcelain"])?;
    if !status.status.success() {
        bail!("git status failed for {}", path.display());
    }
    if !status.stdout.is_empty() {
        return skip("uncommitted changes");
    }
    if !git(path, &["rev-parse", "--abbrev-ref", "@{upstream}"])?
        .status
        .success()
    {
        return skip("no upstream");
    }
    let ahead = git(path, &["rev-list", "--count", "@{upstream}..HEAD"])?;
    if !ahead.status.success() {
        bail!("git rev-list failed for {}", path.display());
    }
    if String::from_utf8_lossy(&ahead.stdout)
        .trim()
        .parse::<u64>()?
        > 0
    {
        return skip("unpushed commits");
    }
    Ok((None, lock))
}
