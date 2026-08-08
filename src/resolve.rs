//! Locating the pieces of an instance from wherever grove was invoked.
//!
//! Everything hangs off `git worktree list --porcelain` rather than path shape, so the
//! several worktree layouts in the wild all resolve identically.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// Root of the worktree grove was invoked from.
    pub worktree: PathBuf,
    /// The main worktree — the only place gitignored secrets reliably live.
    pub main_worktree: PathBuf,
    /// Instance name, `[a-z0-9_]`, derived from the worktree directory. Used verbatim in
    /// database names, so it stays inside what Mongo and Postgres both accept.
    pub slug: String,
}

impl Resolved {
    /// True when grove was invoked from the main checkout. Mutating commands refuse
    /// here: rendering would overwrite the real `.env.local` every instance reads from.
    pub fn is_main(&self) -> bool {
        self.worktree == self.main_worktree
    }

    /// Filesystem-safe identity for the repo, shared by all its worktrees — the directory
    /// name plus a hash of the full path, so two checkouts of the same repo in different
    /// places never share a registry entry. Same shape pi-review already uses.
    pub fn state_key(&self) -> String {
        let name = self
            .main_worktree
            .file_name()
            .map(|n| slugify(&n.to_string_lossy()))
            .unwrap_or_else(|| "repo".to_string());
        format!("{name}-{:08x}", fnv1a(self.main_worktree.as_os_str()))
    }
}

/// FNV-1a, 32-bit. Hand-rolled because this value is persisted in the registry and in
/// state directory names: `DefaultHasher` carries no cross-version stability guarantee.
fn fnv1a(s: &std::ffi::OsStr) -> u32 {
    use std::os::unix::ffi::OsStrExt;
    let mut hash: u32 = 0x811c_9dc5;
    for byte in s.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

pub fn resolve(cwd: &Path) -> Result<Resolved> {
    let stdout = git(cwd, &["worktree", "list", "--porcelain"])?;
    let main_worktree = stdout
        .lines()
        .find_map(|l| l.strip_prefix("worktree "))
        .map(PathBuf::from)
        .context("`git worktree list --porcelain` listed no worktrees")?;

    let worktree = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"])?.trim());

    let slug = worktree
        .file_name()
        .map(|n| slugify(&n.to_string_lossy()))
        .context("worktree path has no final component")?;

    Ok(Resolved {
        worktree,
        main_worktree,
        slug,
    })
}

fn slugify(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .with_context(|| format!("running `git {}`", args.join(" ")))?;

    if !out.status.success() {
        bail!(
            "not a git repository: {}\n{}",
            cwd.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    String::from_utf8(out.stdout).context("git emitted non-UTF-8 output")
}
