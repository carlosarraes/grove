//! One copy of what `setup` produces per set of inputs, shared into every worktree by
//! hardlink. npm copies bytes on every install; uv links files out of a cache. This is
//! the uv model for any tool: the declared key files decide the tree, the store holds
//! it once, and a worktree is a private set of names over shared blocks.

use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::config::Cache;

/// Where the store lives. `GROVE_CACHE_DIR` overrides it for the same reason
/// `GROVE_STATE_DIR` exists: the test suite must not share a store with the machine.
pub fn dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("GROVE_CACHE_DIR")
        && !dir.is_empty()
    {
        return Ok(PathBuf::from(dir));
    }
    let base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".cache"),
    };
    Ok(base.join("grove/store"))
}

/// The tree for one key. A fixed leaf name, so a `path` of `node_modules` in one repo
/// and `.venv` in another never collide on how they are stored.
pub fn entry(store: &Path, hash: &str) -> PathBuf {
    store.join(hash).join("tree")
}

/// The identity of a tree: what produces it and what it is produced from. The setup
/// command is part of it because `npm ci` and `npm install --legacy-peer-deps` make
/// different trees from one lockfile, and the path because two caches in one service
/// directory must not share an entry.
pub fn key(cwd: &Path, cache: &Cache, setup: &str) -> Result<String> {
    let mut hash = Fnv64::new();
    hash.write(b"grove-cache-v2");
    hash.write(cache.path.as_bytes());
    hash.write(setup.as_bytes());
    for name in &cache.key {
        let path = cwd.join(name);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == ErrorKind::NotFound => bail!(
                "no {name} in {} — it is in this service's cache key, and a tree whose \
                 inputs are missing cannot be identified, let alone shared",
                cwd.display()
            ),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        hash.write(name.as_bytes());
        hash.write(&bytes);
    }
    Ok(format!("{:016x}", hash.finish()))
}

pub struct Linked {
    /// False when the blocks had to be copied — another filesystem, or one that refuses
    /// links — so the caller can say the worktree got a copy rather than a share.
    pub hardlinked: bool,
    pub files: u64,
}

/// Recreate `from` at `to`: directories made, regular files hardlinked, symlinks
/// recreated as symlinks. Never followed: `node_modules/.bin` is relative links into the
/// tree itself, and a link followed here would become a copy that drifts.
pub fn link_tree(from: &Path, to: &Path) -> Result<Linked> {
    let mut linked = Linked {
        hardlinked: true,
        files: 0,
    };
    link_dir(from, to, &mut linked)?;
    Ok(linked)
}

fn link_dir(from: &Path, to: &Path, state: &mut Linked) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for item in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let item = item?;
        let src = item.path();
        let dst = to.join(item.file_name());
        let meta = src
            .symlink_metadata()
            .with_context(|| format!("reading {}", src.display()))?;
        if meta.is_symlink() {
            state.files += 1;
            let target = std::fs::read_link(&src)?;
            std::os::unix::fs::symlink(target, &dst)
                .with_context(|| format!("linking {}", dst.display()))?;
        } else if meta.is_dir() {
            link_dir(&src, &dst, state)?;
        } else {
            state.files += 1;
            if state.hardlinked {
                match std::fs::hard_link(&src, &dst) {
                    Ok(()) => continue,
                    // Once one file cannot be linked none of them can, so stop trying
                    // and copy the rest rather than paying a failed syscall per file.
                    Err(e)
                        if matches!(
                            e.kind(),
                            ErrorKind::CrossesDevices
                                | ErrorKind::PermissionDenied
                                | ErrorKind::TooManyLinks
                        ) =>
                    {
                        state.hardlinked = false;
                    }
                    Err(e) => return Err(e).with_context(|| format!("linking {}", dst.display())),
                }
            }
            std::fs::copy(&src, &dst).with_context(|| format!("copying {}", dst.display()))?;
        }
    }
    Ok(())
}

pub struct Promoted {
    /// True when the worktree now shares the store's blocks. False when it kept a
    /// private copy: it lost a race to another `up` with the same key, or the store is
    /// on another filesystem.
    pub shared: bool,
}

/// Publish a complete tree and its inventory by atomic rename. The caller holds
/// the exclusive entry lease, and the original install stays intact on failure.
pub fn promote(built: &Path, store: &Path, hash: &str) -> Result<Promoted> {
    let target = entry(store, hash);
    if target.exists() {
        return Ok(Promoted { shared: false });
    }
    std::fs::create_dir_all(store)?;
    let staging = store.join(format!("{hash}.tmp-{}", std::process::id()));
    std::fs::create_dir(&staging)?;
    let result = (|| {
        let linked = link_tree(built, &staging.join("tree"))?;
        std::fs::write(staging.join("files"), linked.files.to_string())?;
        std::fs::rename(&staging, target.parent().expect("entry parent"))?;
        Ok(Promoted {
            shared: linked.hardlinked,
        })
    })();
    if staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}

/// Shared leases allow simultaneous links. A refresh or GC takes the exclusive lease.
/// Lock files live outside the store and must never be unlinked while clients run.
pub fn lock_key(store: &Path, hash: &str, shared: bool) -> Result<File> {
    let locks = store.with_extension("locks");
    std::fs::create_dir_all(&locks)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(locks.join(hash))?;
    if shared {
        lock.lock_shared()?;
    } else {
        lock.lock()?;
    }
    Ok(lock)
}

/// Check the entry inventory after linking, before the caller replaces its install.
pub fn link_entry(store: &Path, hash: &str, to: &Path) -> Result<Linked> {
    let count = std::fs::read_to_string(store.join(hash).join("files"))
        .with_context(|| {
            format!("store entry {hash} has no complete inventory; run grove up --no-cache")
        })?
        .trim()
        .parse::<u64>()
        .context("invalid store inventory")?;
    let linked = link_tree(&entry(store, hash), to)?;
    if linked.files != count {
        bail!(
            "store entry {hash} is incomplete: expected {count} files, found {}; run grove up --no-cache",
            linked.files
        );
    }
    Ok(linked)
}

/// Forget one entry. Worktrees that linked from it keep their blocks; only the store's
/// names go.
pub fn evict(store: &Path, hash: &str) -> Result<()> {
    match std::fs::remove_dir_all(store.join(hash)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing store entry {hash}")),
    }
}

/// Remove every entry not in `referenced`, returning what went. Safe by construction —
/// a worktree holds its own names for the blocks — so the only mistake possible is
/// deleting an entry a registered worktree would have linked from, which is what
/// `referenced` exists to prevent.
pub fn gc(store: &Path, referenced: &HashSet<String>) -> Result<Vec<String>> {
    let doomed = unreferenced(store, referenced)?;
    for name in &doomed {
        let _lease = lock_key(store, name, false)?;
        evict(store, name).with_context(|| format!("removing store entry {name}"))?;
    }
    Ok(doomed)
}

/// What `gc` would remove, so a dry run can name it without touching it.
pub fn unreferenced(store: &Path, referenced: &HashSet<String>) -> Result<Vec<String>> {
    let entries = match std::fs::read_dir(store) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", store.display())),
    };
    let mut doomed: Vec<String> = entries
        .map(|item| Ok(item?.file_name().to_string_lossy().into_owned()))
        .collect::<Result<_>>()?;
    doomed.retain(|name| {
        name.len() == 16
            && name.bytes().all(|b| b.is_ascii_hexdigit())
            && !referenced.contains(name)
    });
    doomed.sort();
    Ok(doomed)
}

/// 64-bit FNV-1a, hand-rolled like the 32-bit one in `resolve` and for the same reason:
/// the value is persisted and named on disk, so it must not move with a dependency's
/// idea of hashing. Not cryptographic; there is no adversary, only lockfiles.
struct Fnv64(u64);

impl Fnv64 {
    fn new() -> Self {
        Fnv64(0xcbf2_9ce4_8422_2325)
    }

    /// Length-prefixed, so `("ab", "c")` and `("a", "bc")` differ.
    fn write(&mut self, bytes: &[u8]) {
        for byte in (bytes.len() as u64).to_le_bytes().iter().chain(bytes) {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}
