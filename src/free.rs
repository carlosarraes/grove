//! Release a branch from stale worktrees so `git worktree add` can take it.
//!
//! Detaching keeps the branch ref, its commits and the worktree folder; nothing here
//! removes a worktree or deletes a branch. Every check fails closed: a worktree whose
//! state cannot be read is kept.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

const EXIT_FREE: i32 = 0;
const EXIT_HELD: i32 = 1;
const EXIT_USAGE: i32 = 2;

#[derive(Debug, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub locked: bool,
}

/// Parse `git worktree list --porcelain`; its first entry is always the main checkout.
pub fn parse_porcelain(text: &str) -> Vec<Worktree> {
    let mut worktrees = Vec::new();
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            worktrees.push(Worktree {
                path: PathBuf::from(path),
                branch: None,
                locked: false,
            });
        } else if let Some(current) = worktrees.last_mut() {
            if let Some(branch) = line.strip_prefix("branch refs/heads/") {
                current.branch = Some(branch.to_string());
            } else if line == "locked" || line.starts_with("locked ") {
                current.locked = true;
            }
        }
    }
    worktrees
}

/// The first process whose working directory is `root` or inside it.
pub fn holder(cwds: &[(u32, PathBuf)], root: &Path) -> Option<u32> {
    cwds.iter()
        .find(|(_, cwd)| cwd.starts_with(root))
        .map(|(pid, _)| *pid)
}

/// Every other process's working directory: /proc on Linux, lsof elsewhere.
pub fn process_cwds() -> Result<Vec<(u32, PathBuf)>> {
    let own = std::process::id();
    let mut cwds = if cfg!(target_os = "linux") {
        proc_cwds()?
    } else {
        lsof_cwds()?
    };
    cwds.retain(|(pid, _)| *pid != own);
    Ok(cwds)
}

fn proc_cwds() -> Result<Vec<(u32, PathBuf)>> {
    let mut cwds = Vec::new();
    for entry in std::fs::read_dir("/proc").context("reading /proc to find busy worktrees")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        // Processes of other users, or ones that just exited, have no readable cwd.
        if let Ok(cwd) = std::fs::read_link(entry.path().join("cwd")) {
            cwds.push((pid, cwd));
        }
    }
    Ok(cwds)
}

fn lsof_cwds() -> Result<Vec<(u32, PathBuf)>> {
    let output = Command::new("lsof")
        .args(["-a", "-d", "cwd", "-Fn"])
        .output()
        .context("running lsof to find busy worktrees; install lsof or run on Linux")?;
    Ok(parse_lsof_cwds(&String::from_utf8_lossy(&output.stdout)))
}

/// lsof's `-F` output: a `p<pid>` line, then `n<path>` for each cwd it lists.
pub fn parse_lsof_cwds(text: &str) -> Vec<(u32, PathBuf)> {
    let mut pid = None;
    let mut cwds = Vec::new();
    for line in text.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.parse().ok();
        } else if let (Some(path), Some(pid)) = (line.strip_prefix('n'), pid) {
            cwds.push((pid, PathBuf::from(path)));
        }
    }
    cwds
}

fn git(cwd: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .with_context(|| format!("running `git {}`", args.join(" ")))
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Run `grove free`; returns the exit code (0 free, 1 still held, 2 usage error).
pub fn run(cwd: &Path, branch: &str, dry_run: bool) -> Result<i32> {
    if branch.trim().is_empty() {
        eprintln!("error: branch name is empty; usage: grove free <branch>");
        return Ok(EXIT_USAGE);
    }
    let top = git(cwd, &["rev-parse", "--show-toplevel"])?;
    if !top.status.success() {
        eprintln!(
            "error: not inside a git worktree ({}); run grove free from the repo",
            cwd.display()
        );
        return Ok(EXIT_USAGE);
    }
    let current = canonical(Path::new(String::from_utf8_lossy(&top.stdout).trim()));

    let listing = git(cwd, &["worktree", "list", "--porcelain"])?;
    if !listing.status.success() {
        bail!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&listing.stderr).trim()
        );
    }
    let worktrees = parse_porcelain(&String::from_utf8_lossy(&listing.stdout));
    let main = worktrees.first().map(|w| canonical(&w.path));

    let mut cwds = None;
    let mut still_held = false;
    for worktree in worktrees
        .iter()
        .filter(|w| w.branch.as_deref() == Some(branch))
    {
        let path = canonical(&worktree.path);
        let shown = worktree.path.display();
        let verdict = if Some(&path) == main.as_ref() {
            Err("main checkout".to_string())
        } else if path == current {
            Err("current worktree".to_string())
        } else if worktree.locked {
            Err("locked".to_string())
        } else {
            match dirty(&worktree.path)? {
                Some(reason) => Err(reason),
                None => {
                    if cwds.is_none() {
                        cwds = Some(process_cwds()?);
                    }
                    match holder(cwds.as_deref().unwrap_or_default(), &path) {
                        Some(pid) => Err(format!("in use by pid {pid}")),
                        None => Ok(()),
                    }
                }
            }
        };
        match verdict {
            Err(reason) => {
                println!("kept {shown}: {reason}");
                still_held = true;
            }
            Ok(()) if dry_run => println!("would free {shown}"),
            Ok(()) => match detach(&worktree.path) {
                Ok(()) => println!("freed {shown}"),
                Err(reason) => {
                    println!("kept {shown}: {reason}");
                    still_held = true;
                }
            },
        }
    }
    Ok(if still_held { EXIT_HELD } else { EXIT_FREE })
}

/// Why the worktree cannot be trusted to be clean, or `None` when it is.
fn dirty(path: &Path) -> Result<Option<String>> {
    if !path.is_dir() {
        return Ok(Some(
            "missing on disk; run `git worktree prune`".to_string(),
        ));
    }
    let status = git(path, &["status", "--porcelain"])?;
    if !status.status.success() {
        return Ok(Some(format!(
            "git status failed: {}",
            String::from_utf8_lossy(&status.stderr).trim()
        )));
    }
    Ok((!status.stdout.is_empty()).then(|| "dirty".to_string()))
}

fn detach(path: &Path) -> std::result::Result<(), String> {
    let out = git(path, &["switch", "--detach"]).map_err(|e| format!("{e:#}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git switch --detach failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_yields_branch_and_lock_state() {
        let text = "worktree /r\nHEAD aaa\nbranch refs/heads/main\n\n\
                    worktree /r/a\nHEAD bbb\nbranch refs/heads/feature/x\nlocked\n\n\
                    worktree /r/b\nHEAD ccc\ndetached\nlocked in use elsewhere\n\n\
                    worktree /r/c\nHEAD ddd\nbranch refs/heads/other\nprunable gitdir gone\n";
        let parsed = parse_porcelain(text);
        let summary: Vec<_> = parsed
            .iter()
            .map(|w| (w.path.to_str().unwrap(), w.branch.as_deref(), w.locked))
            .collect();
        assert_eq!(
            summary,
            [
                ("/r", Some("main"), false),
                ("/r/a", Some("feature/x"), true),
                ("/r/b", None, true),
                ("/r/c", Some("other"), false),
            ]
        );
    }

    #[test]
    fn holder_matches_the_root_and_anything_below_it_but_not_siblings() {
        let cwds = vec![
            (10, PathBuf::from("/r/ab")),
            (11, PathBuf::from("/r/a/src/deep")),
        ];
        assert_eq!(holder(&cwds, Path::new("/r/a")), Some(11));
        assert_eq!(holder(&cwds, Path::new("/r/ab")), Some(10));
        assert_eq!(holder(&cwds, Path::new("/r/c")), None);
    }

    #[test]
    fn lsof_output_pairs_each_cwd_with_its_pid() {
        let text = "p100\nfcwd\nn/home/u/a\np200\nfcwd\nn/tmp/b c\n";
        assert_eq!(
            parse_lsof_cwds(text),
            [
                (100, PathBuf::from("/home/u/a")),
                (200, PathBuf::from("/tmp/b c"))
            ]
        );
    }

    #[test]
    fn a_real_process_started_in_a_directory_is_detected_there() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut child = Command::new("sleep")
            .arg("60")
            .current_dir(&root)
            .spawn()
            .unwrap();

        let found = holder(&process_cwds().unwrap(), &root);

        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(found, Some(child.id()));
    }
}
