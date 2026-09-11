//! How much disk a worktree's git-ignored files take — the dependency trees `setup`
//! installs, which is the cost `down` does not reclaim.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

/// Bytes this worktree holds alone under its git-ignored paths, or None when git cannot
/// say.
///
/// Blocks rather than lengths: node_modules is the many-tiny-files case, where a
/// thirty-byte file still occupies a four-kilobyte block, and blocks are what `df` gets
/// back after `rm -rf`. Only blocks with a single name count: a block shared with the
/// store, or with uv's cache, survives this worktree's removal and is charged to the
/// store instead (`tree_size`), once, whatever the number of worktrees linking it.
pub fn measure(worktree: &Path) -> Option<u64> {
    let listed = Command::new("git")
        .args([
            "ls-files",
            "-z",
            "-o",
            "-i",
            "--exclude-standard",
            "--directory",
        ])
        .current_dir(worktree)
        .output()
        .ok()
        .filter(|o| o.status.success())?;

    let total = listed
        .stdout
        .split(|b| *b == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            walk(
                &worktree.join(OsStr::from_bytes(entry)),
                Sharing::PrivateOnly,
                &mut HashSet::new(),
            )
        })
        .sum();
    Some(total)
}

/// Bytes under a tree grove owns outright — a store entry — counting every block once.
pub fn tree_size(root: &Path) -> u64 {
    walk(root, Sharing::CountOnce, &mut HashSet::new())
}

#[derive(Clone, Copy)]
enum Sharing {
    /// A block with more than one name belongs to whoever else names it.
    PrivateOnly,
    /// Every block, but a block with several names inside the tree once.
    CountOnce,
}

/// Advisory, so a path that vanishes or refuses to be read mid-walk counts as nothing
/// rather than failing the `up` that asked.
fn walk(path: &Path, sharing: Sharing, seen: &mut HashSet<(u64, u64)>) -> u64 {
    // Never through a symlink: `node_modules/.bin` and pnpm layouts point outside the
    // tree, and those blocks are not this worktree's to free.
    let Ok(meta) = path.symlink_metadata() else {
        return 0;
    };
    if meta.is_dir() {
        // uv hardlinks a venv out of its cache on Linux and clones it on macOS; either way
        // the blocks belong to the cache, and a clone leaves no trace for the dedup below
        // to catch. Skipping the venv whole is what keeps 340M of `.venv` from being
        // reported as cost that deleting it would not return.
        if path.join("pyvenv.cfg").exists() {
            return 0;
        }
        let Ok(entries) = std::fs::read_dir(path) else {
            return 0;
        };
        return entries
            .flatten()
            .map(|e| walk(&e.path(), sharing, seen))
            .sum();
    }
    if meta.nlink() > 1 {
        match sharing {
            Sharing::PrivateOnly => return 0,
            Sharing::CountOnce if !seen.insert((meta.dev(), meta.ino())) => return 0,
            Sharing::CountOnce => {}
        }
    }
    meta.blocks() * 512
}

/// A size as `du -h` prints it, for a column read at a glance: one decimal while the
/// number is a single digit, none once it is not.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => format!("{bytes}B"),
        _ if value < 10.0 => format!("{value:.1}{}", UNITS[unit]),
        _ => format!("{value:.0}{}", UNITS[unit]),
    }
}
