//! Remove idle, grove-linked git worktrees, with a read-only default.

use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::footprint::{human_size, unique_reclaimable};
use crate::registry::Registry;
use crate::{instance, last_up, resolve, supervise};

pub fn run(
    cwd: &Path,
    selectors: &[String],
    repos: &[PathBuf],
    yes: bool,
    min_age: &str,
) -> Result<()> {
    let min_age = (min_age, instance::parse_duration(min_age)?);
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

    let mut roots = BTreeSet::new();
    let mut common_roots: BTreeMap<String, PathBuf> = BTreeMap::new();
    for path in chosen.keys().chain(repos.iter()) {
        let common = git(
            path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        if !common.status.success() {
            if repos.contains(path) {
                bail!("not a git repository: {}", path.display());
            }
            continue;
        }
        let common = String::from_utf8(common.stdout)?.trim().to_owned();
        if let Some(root) = common_roots.get(&common) {
            roots.insert(root.clone());
        } else {
            let root = resolve::resolve(path)?.main_worktree;
            roots.insert(root.clone());
            common_roots.insert(common, root);
        }
    }
    let mut candidates = BTreeMap::new();
    let mut states = BTreeMap::new();
    let mut pr_cache = BTreeMap::new();
    for root in &roots {
        let listing = git(root, &["worktree", "list", "--porcelain"])?;
        if !listing.status.success() {
            bail!("cannot list worktrees in {}", root.display());
        }
        for tree in crate::free::parse_porcelain(&String::from_utf8_lossy(&listing.stdout)) {
            if selectors.is_empty()
                || chosen.contains_key(&tree.path)
                || repos
                    .iter()
                    .any(|p| resolve::resolve(p).is_ok_and(|r| r.main_worktree == *root))
            {
                candidates.insert(tree.path.clone(), (root.clone(), tree.locked));
            }
        }
        states.insert(
            root.clone(),
            lookup_prs(root, |repo| {
                pr_cache
                    .entry(repo.to_owned())
                    .or_insert_with(|| github_prs(repo).map_err(|e| e.to_string()))
                    .clone()
                    .map_err(anyhow::Error::msg)
            })
            .map_err(|e| e.to_string()),
        );
    }
    let processes = process_paths()?;
    let items: Vec<_> = candidates.iter().collect();
    let queue = std::sync::Mutex::new(items.iter());
    let inspections = std::thread::scope(
        |scope| -> Result<BTreeMap<PathBuf, (Option<String>, u64)>> {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let queue = &queue;
                    let registry = &registry;
                    let processes = &processes;
                    let states = &states;
                    scope.spawn(move || -> Result<Vec<_>> {
                        std::iter::from_fn(|| queue.lock().unwrap().next())
                            .map(|(path, (root, locked))| {
                                let (reason, _) = inspect(
                                    path,
                                    root,
                                    *locked,
                                    registry,
                                    false,
                                    processes,
                                    &states[root],
                                    min_age,
                                )?;
                                let bytes = if reason.is_none() {
                                    unique_reclaimable(path)
                                } else {
                                    0
                                };
                                Ok(((*path).clone(), (reason, bytes)))
                            })
                            .collect()
                    })
                })
                .collect();
            let mut results = BTreeMap::new();
            for handle in handles {
                results.extend(
                    handle
                        .join()
                        .map_err(|_| anyhow::anyhow!("worktree inspection panicked"))??,
                );
            }
            Ok(results)
        },
    )?;
    let mut pruned = BTreeSet::new();
    let (mut removable, mut skipped, mut removed, mut total) = (0, 0, 0, 0u64);
    for (path, (root, locked)) in candidates {
        let entry = chosen.get(&path);
        let prs = &states[&root];
        let (mut reason, bytes) = inspections[&path].clone();
        let mut lock = None;
        let size = human_size(bytes);
        if yes && reason.is_none() {
            (reason, lock) = inspect(
                &path,
                &root,
                locked,
                &registry,
                true,
                &process_paths()?,
                prs,
                min_age,
            )?;
        }
        if let Some(reason) = reason {
            println!("skip {} {size}  {reason}", path.display());
            skipped += 1;
            continue;
        }
        removable += 1;
        if yes {
            let output = Command::new("git")
                .current_dir(&root)
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
            pruned.insert(root);
            if entry.is_some() {
                registry.release(&path)?;
            }
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
    for repo in pruned {
        let result = git(&repo, &["worktree", "prune"])?;
        if !result.status.success() {
            bail!("git worktree prune failed in {}", repo.display());
        }
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

#[allow(clippy::too_many_arguments)]
fn inspect(
    path: &Path,
    root: &Path,
    locked: bool,
    registry: &Registry,
    retain_lock: bool,
    processes: &[(u32, PathBuf)],
    prs: &std::result::Result<Vec<Pr>, String>,
    min_age: (&str, Duration),
) -> Result<(Option<String>, Option<File>)> {
    let skip = |reason: &str| Ok((Some(reason.to_owned()), None));
    if path == root {
        return skip("primary worktree");
    }
    if locked || (retain_lock && worktree_locked(root, path)?) {
        return skip("locked worktree");
    }
    if std::env::var_os("HOME").is_some_and(|home| {
        path.starts_with(PathBuf::from(home).join(".local/state/ferry/worktrees"))
    }) {
        return skip("ferry worktree");
    }
    if !resolve::is_worktree(path) {
        return skip("worktree unavailable");
    }
    let resolved = resolve::Resolved {
        worktree: path.to_owned(),
        main_worktree: root.to_owned(),
        slug: path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect(),
    };
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
    if registry
        .get(path)?
        .is_some_and(|fresh| fresh.services.values().any(supervise::is_alive))
    {
        return skip("running stack");
    }
    if let Some(pid) = crate::free::holder(processes, path) {
        return skip(&format!("open files (pid {pid})"));
    }
    match age_guard(path, min_age, SystemTime::now()) {
        Ok(Some(reason)) => return skip(&reason),
        Ok(None) => {}
        Err(error) => return skip(&format!("activity age unknown ({error})")),
    }
    let status = git(path, &["status", "--porcelain"])?;
    if !status.status.success() {
        bail!("git status failed for {}", path.display());
    }
    if !status.stdout.is_empty() {
        return skip("uncommitted changes");
    }
    let upstream = git(path, &["rev-parse", "--abbrev-ref", "@{upstream}"])?
        .status
        .success();
    if upstream {
        let ahead = git(path, &["rev-list", "--count", "@{upstream}..HEAD"])?;
        if !ahead.status.success() {
            return skip("cannot read upstream");
        }
        if String::from_utf8_lossy(&ahead.stdout).trim() == "0" {
            return Ok((None, lock));
        }
    }
    match pr_eligibility(path, prs) {
        Ok(()) => Ok((None, lock)),
        Err(reason) => skip(&format!(
            "{}; {reason}",
            if upstream {
                "unpushed commits"
            } else {
                "no upstream"
            }
        )),
    }
}

fn age_guard(path: &Path, min_age: (&str, Duration), now: SystemTime) -> Result<Option<String>> {
    if min_age.1.is_zero() {
        return Ok(None);
    }
    let admin = git(path, &["rev-parse", "--absolute-git-dir"])?;
    if !admin.status.success() {
        bail!("cannot locate worktree git directory");
    }
    let admin = PathBuf::from(String::from_utf8(admin.stdout)?.trim());
    let commit = git(path, &["log", "-1", "--format=%ct", "HEAD"])?;
    if !commit.status.success() {
        bail!("cannot read HEAD commit time");
    }
    let timestamp = String::from_utf8(commit.stdout)?.trim().parse::<u64>()?;
    let mut newest = UNIX_EPOCH
        .checked_add(Duration::from_secs(timestamp))
        .context("HEAD timestamp out of range")?;
    for file in [path.to_owned(), path.join(".git"), admin.join("index")] {
        let modified = std::fs::metadata(&file)
            .with_context(|| format!("stat {}", file.display()))?
            .modified()?;
        newest = newest.max(modified);
    }
    match std::fs::read_to_string(admin.join("logs/HEAD")) {
        Ok(log) => {
            for line in log.lines() {
                let timestamp = line
                    .split('\t')
                    .next()
                    .context("invalid HEAD reflog")?
                    .split_whitespace()
                    .rev()
                    .nth(1)
                    .context("missing HEAD reflog timestamp")?
                    .parse::<u64>()?;
                newest = newest.max(
                    UNIX_EPOCH
                        .checked_add(Duration::from_secs(timestamp))
                        .context("reflog timestamp out of range")?,
                );
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(
        (now.duration_since(newest).unwrap_or_default() <= min_age.1)
            .then(|| format!("active in the last {}", min_age.0)),
    )
}

fn worktree_locked(root: &Path, path: &Path) -> Result<bool> {
    let out = git(root, &["worktree", "list", "--porcelain"])?;
    if !out.status.success() {
        bail!("cannot check worktree lock");
    }
    Ok(
        crate::free::parse_porcelain(&String::from_utf8_lossy(&out.stdout))
            .iter()
            .any(|t| t.path == path && t.locked),
    )
}

#[derive(Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pr {
    number: u64,
    state: String,
    head_ref_name: String,
    head_ref_oid: String,
}

fn github_repo(url: &str) -> Option<String> {
    let name = url
        .trim()
        .strip_prefix("git@github.com:")
        .or_else(|| url.trim().strip_prefix("https://github.com/"))
        .or_else(|| url.trim().strip_prefix("ssh://git@github.com/"))?;
    let name = name.trim_end_matches('/').trim_end_matches(".git");
    let parts: Vec<_> = name.split('/').collect();
    (parts.len() == 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        }))
    .then(|| name.to_owned())
}

fn lookup_prs(root: &Path, lookup: impl FnOnce(&str) -> Result<Vec<Pr>>) -> Result<Vec<Pr>> {
    let remote = git(root, &["remote", "get-url", "origin"])?;
    let repo =
        github_repo(&String::from_utf8_lossy(&remote.stdout)).context("origin is not on GitHub")?;
    lookup(&repo)
}

fn github_prs(repo: &str) -> Result<Vec<Pr>> {
    let out = Command::new("gh")
        .args([
            "pr",
            "list",
            "-R",
            repo,
            "--state",
            "all",
            "--limit",
            "4000",
            "--json",
            "number,state,headRefName,headRefOid",
        ])
        .output()
        .context("gh unavailable")?;
    if !out.status.success() {
        bail!("gh failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let prs: Vec<Pr> = serde_json::from_slice(&out.stdout)?;
    Ok(prs)
}

fn pr_eligibility(
    path: &Path,
    prs: &std::result::Result<Vec<Pr>, String>,
) -> std::result::Result<(), String> {
    let check = || -> Result<()> {
        let prs = prs
            .as_ref()
            .map_err(|e| anyhow::anyhow!("pr state unknown ({e})"))?;
        let branch = git(path, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
        let branches = if branch.status.success() {
            branch
        } else {
            git(
                path,
                &["branch", "--contains", "HEAD", "--format=%(refname:short)"],
            )?
        };
        if !branches.status.success() {
            bail!("cannot read branches");
        }
        let names = String::from_utf8_lossy(&branches.stdout);
        let matching: Vec<_> = prs
            .iter()
            .filter(|pr| names.lines().any(|name| name == pr.head_ref_name))
            .collect();
        if let Some(pr) = matching.iter().find(|pr| pr.state == "OPEN") {
            bail!("pr #{} open", pr.number);
        }
        let closed: Vec<_> = matching
            .iter()
            .filter(|pr| pr.state == "MERGED" || pr.state == "CLOSED")
            .collect();
        if closed.is_empty() {
            bail!("no merged/closed PR");
        }
        let reachable = git(path, &["rev-list", "-n1", "HEAD", "--not", "--remotes"])?;
        if reachable.status.success() && reachable.stdout.is_empty() {
            return Ok(());
        }
        for pr in closed {
            if git(
                path,
                &["merge-base", "--is-ancestor", "HEAD", &pr.head_ref_oid],
            )?
            .status
            .success()
            {
                return Ok(());
            }
        }
        bail!("unpushed commits")
    };
    check().map_err(|e| e.to_string())
}

fn process_paths() -> Result<Vec<(u32, PathBuf)>> {
    if !cfg!(target_os = "linux") {
        return crate::free::process_cwds();
    }
    let mut paths = Vec::new();
    for entry in std::fs::read_dir("/proc")?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        if let Ok(path) = std::fs::read_link(entry.path().join("cwd")) {
            paths.push((pid, path));
        }
        if let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) {
            for fd in fds.flatten() {
                if let Ok(path) = std::fs::read_link(fd.path()) {
                    paths.push((pid, path));
                }
            }
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_remotes_are_strict() {
        assert_eq!(
            github_repo("git@github.com:acme/repo.git"),
            Some("acme/repo".into())
        );
        assert_eq!(
            github_repo("https://github.com/acme/repo.git"),
            Some("acme/repo".into())
        );
        assert_eq!(github_repo("https://example.com/acme/repo"), None);
    }
}

#[cfg(test)]
mod pr_tests {
    use super::*;

    fn command(path: &Path, args: &[&str]) -> String {
        let out = git(path, args).unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn pr_evidence_covers_pushed_unpushed_detached_and_open() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        command(repo, &["init", "-b", "topic"]);
        command(repo, &["config", "user.email", "test@example.com"]);
        command(repo, &["config", "user.name", "Test"]);
        command(repo, &["commit", "--allow-empty", "-m", "base"]);
        let head = command(repo, &["rev-parse", "HEAD"]);
        let remote = repo.join("remote.git");
        command(repo, &["init", "--bare", remote.to_str().unwrap()]);
        command(repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
        command(repo, &["push", "origin", "topic"]);
        let prs = |state: &str, oid: &str| -> std::result::Result<Vec<Pr>, String> {
            Ok(serde_json::from_value(serde_json::json!([{"number":42,"state":state,"headRefName":"topic","headRefOid":oid}])).unwrap())
        };
        assert!(pr_eligibility(repo, &prs("CLOSED", &head)).is_ok());
        assert!(
            pr_eligibility(
                repo,
                &prs("CLOSED", "0000000000000000000000000000000000000000")
            )
            .is_ok()
        );
        assert!(pr_eligibility(repo, &Ok(Vec::new())).is_err());
        assert!(pr_eligibility(repo, &prs("MERGED", &head)).is_ok());
        assert!(
            pr_eligibility(repo, &prs("OPEN", &head))
                .unwrap_err()
                .contains("pr #42 open")
        );
        command(repo, &["commit", "--allow-empty", "-m", "local"]);
        assert!(pr_eligibility(repo, &prs("CLOSED", &head)).is_err());
        let local = command(repo, &["rev-parse", "HEAD"]);
        assert!(pr_eligibility(repo, &prs("CLOSED", &local)).is_ok());
        command(repo, &["commit", "--allow-empty", "-m", "PR descendant"]);
        let descendant = command(repo, &["rev-parse", "HEAD"]);
        command(repo, &["checkout", "--detach", &local]);
        assert!(pr_eligibility(repo, &prs("CLOSED", &descendant)).is_ok());
        command(repo, &["branch", "second", &local]);
        let mut mixed = prs("CLOSED", &descendant).unwrap();
        mixed.push(Pr {
            number: 43,
            state: "OPEN".into(),
            head_ref_name: "second".into(),
            head_ref_oid: local.clone(),
        });
        assert!(
            pr_eligibility(repo, &Ok(mixed))
                .unwrap_err()
                .contains("pr #43 open")
        );
        assert!(pr_eligibility(repo, &prs("CLOSED", &local)).is_ok());
        assert!(pr_eligibility(repo, &prs("OPEN", &local)).is_err());
        let error = pr_eligibility(repo, &Err("gh authentication failed".into())).unwrap_err();
        assert!(error.contains("pr state unknown (gh authentication failed)"));
        command(
            repo,
            &[
                "remote",
                "set-url",
                "origin",
                "git@github.com:acme/repo.git",
            ],
        );
        assert!(lookup_prs(repo, |_| anyhow::bail!("gh failed")).is_err());
        assert_eq!(
            lookup_prs(repo, |_| prs("CLOSED", &local).map_err(anyhow::Error::msg))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn process_cwd_blocks_a_worktree() {
        let temp = tempfile::tempdir().unwrap();
        let mut child = Command::new("sleep")
            .arg("30")
            .current_dir(temp.path())
            .spawn()
            .unwrap();
        let paths = process_paths().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(crate::free::holder(&paths, temp.path()), Some(child.id()));
    }
}

#[cfg(test)]
mod age_tests {
    use super::*;

    const OLD: u64 = 1_600_000_000;

    fn fixture(commit_time: u64) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("main");
        std::fs::create_dir(&root).unwrap();
        for args in [
            vec!["init"],
            vec!["config", "user.name", "Test"],
            vec!["config", "user.email", "test@example.com"],
            vec!["commit", "--allow-empty", "-m", "base"],
        ] {
            let output = Command::new("git")
                .current_dir(&root)
                .args(args)
                .env("GIT_COMMITTER_DATE", format!("{commit_time} +0000"))
                .env("GIT_AUTHOR_DATE", format!("{commit_time} +0000"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let tree = temp.path().join("tree");
        assert!(
            git(
                &root,
                &["worktree", "add", "-b", "topic", tree.to_str().unwrap()]
            )
            .unwrap()
            .status
            .success()
        );
        let admin = git(&tree, &["rev-parse", "--absolute-git-dir"]).unwrap();
        let admin = PathBuf::from(String::from_utf8(admin.stdout).unwrap().trim());
        reset_activity(&tree, &admin);
        (temp, tree, admin)
    }

    fn set_modified(path: &Path, seconds: u64) {
        File::open(path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds)),
            )
            .unwrap();
    }

    fn reset_activity(tree: &Path, admin: &Path) {
        let head = git(tree, &["rev-parse", "HEAD"]).unwrap();
        let head = String::from_utf8(head.stdout).unwrap();
        std::fs::write(
            admin.join("logs/HEAD"),
            format!(
                "{0} {0} Test <test@example.com> {OLD} +0000\tcheckout\n",
                head.trim()
            ),
        )
        .unwrap();
        for path in [tree.to_owned(), tree.join(".git"), admin.join("index")] {
            set_modified(&path, OLD);
        }
    }

    #[test]
    fn each_activity_signal_protects_an_otherwise_old_worktree() {
        let (_temp, tree, admin) = fixture(OLD);
        let now = UNIX_EPOCH + Duration::from_secs(OLD + 172_800);
        let threshold = ("24h", Duration::from_secs(86_400));
        assert!(age_guard(&tree, threshold, now).unwrap().is_none());
        for path in [tree.clone(), tree.join(".git"), admin.join("index")] {
            set_modified(&path, OLD + 172_800);
            assert_eq!(
                age_guard(&tree, threshold, now).unwrap().as_deref(),
                Some("active in the last 24h")
            );
            reset_activity(&tree, &admin);
        }
        let log = admin.join("logs/HEAD");
        let contents = std::fs::read_to_string(&log).unwrap();
        std::fs::write(
            &log,
            contents.replace(&OLD.to_string(), &(OLD + 172_800).to_string()),
        )
        .unwrap();
        assert!(age_guard(&tree, threshold, now).unwrap().is_some());
        assert!(
            age_guard(&tree, ("0", Duration::ZERO), now)
                .unwrap()
                .is_none()
        );
        std::fs::write(&log, "malformed\n").unwrap();
        assert!(age_guard(&tree, threshold, now).is_err());
    }

    #[test]
    fn commit_time_protects_even_with_old_metadata_and_no_reflog() {
        let (_temp, tree, admin) = fixture(OLD + 172_800);
        std::fs::remove_file(admin.join("logs/HEAD")).unwrap();
        let now = UNIX_EPOCH + Duration::from_secs(OLD + 172_800);
        assert!(
            age_guard(&tree, ("1d", Duration::from_secs(86_400)), now)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn threshold_and_future_activity_are_protected() {
        let (_temp, tree, _admin) = fixture(OLD);
        let threshold = ("1d", Duration::from_secs(86_400));
        assert!(
            age_guard(
                &tree,
                threshold,
                UNIX_EPOCH + Duration::from_secs(OLD + 86_400)
            )
            .unwrap()
            .is_some()
        );
        assert!(
            age_guard(
                &tree,
                threshold,
                UNIX_EPOCH + Duration::from_secs(OLD + 86_401)
            )
            .unwrap()
            .is_none()
        );
        assert!(
            age_guard(&tree, threshold, UNIX_EPOCH + Duration::from_secs(OLD - 1))
                .unwrap()
                .is_some()
        );
    }
}
