//! One copy of what `setup` produces per set of inputs, shared into every worktree by
//! hardlinks or macOS directory clones. npm copies bytes on every install; uv links files out of a cache. This is
//! the uv model for any tool: the declared key files decide the tree, the store holds
//! it once, and a worktree is a private set of names over shared blocks.

use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

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

#[derive(Debug, PartialEq, Eq)]
pub enum LinkMethod {
    Hardlink,
    Copy,
    Clone,
}

pub struct Linked {
    pub method: LinkMethod,
    pub files: u64,
}

/// Recreate `from` at `to`: directories made, regular files hardlinked, symlinks
/// recreated as symlinks. Never followed: `node_modules/.bin` is relative links into the
/// tree itself, and a link followed here would become a copy that drifts.
pub fn link_tree(from: &Path, to: &Path) -> Result<Linked> {
    let mut linked = Linked {
        method: LinkMethod::Hardlink,
        files: 0,
    };
    link_dir(from, to, &mut linked, &AtomicU64::new(0))?;
    Ok(linked)
}

/// `link_tree`, publishing the running file count to `progress` for a heartbeat.
fn link_tree_counting(from: &Path, to: &Path, progress: &AtomicU64) -> Result<Linked> {
    let mut linked = Linked {
        method: LinkMethod::Hardlink,
        files: 0,
    };
    link_dir(from, to, &mut linked, progress)?;
    Ok(linked)
}

fn link_dir(from: &Path, to: &Path, state: &mut Linked, progress: &AtomicU64) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for item in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let item = item?;
        let src = item.path();
        let dst = to.join(item.file_name());
        let meta = src
            .symlink_metadata()
            .with_context(|| format!("reading {}", src.display()))?;
        progress.store(state.files, Ordering::Relaxed);
        if meta.is_symlink() {
            state.files += 1;
            let target = std::fs::read_link(&src)?;
            std::os::unix::fs::symlink(target, &dst)
                .with_context(|| format!("linking {}", dst.display()))?;
        } else if meta.is_dir() {
            link_dir(&src, &dst, state, progress)?;
        } else {
            state.files += 1;
            if state.method == LinkMethod::Hardlink {
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
                        state.method = LinkMethod::Copy;
                    }
                    Err(e) => {
                        return Err(contextualize_store_link_error(
                            e,
                            format!("linking {}", dst.display()),
                            &dst,
                        ));
                    }
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
        std::fs::write(
            staging.join("bytes"),
            crate::footprint::tree_size(&staging.join("tree")).to_string(),
        )?;
        std::fs::rename(&staging, target.parent().expect("entry parent"))?;
        Ok(Promoted {
            shared: linked.method == LinkMethod::Hardlink,
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
    let count = crate::timing::measure_wait(&format!("cache {hash}: inventory read"), || {
        std::fs::read_to_string(store.join(hash).join("files"))
            .with_context(|| {
                format!("store entry {hash} has no complete inventory; run grove up --no-cache")
            })?
            .trim()
            .parse::<u64>()
            .context("invalid store inventory")
    })?;
    let progress = AtomicU64::new(0);
    #[cfg(target_os = "macos")]
    let cloning = AtomicBool::new(macos_clone_enabled());
    #[cfg(not(target_os = "macos"))]
    let cloning = AtomicBool::new(false);
    let mut linked = crate::timing::measure(&format!("cache {hash}: tree creation"), || {
        // 5s typical on the Mac, 197s once at load 53 with no output in between.
        crate::timing::with_heartbeat(
            crate::timing::heartbeat_interval(),
            |elapsed| {
                if cloning.load(Ordering::Relaxed) {
                    eprintln!(
                        "cache {hash}: still cloning {count} files ({}s)",
                        elapsed.as_secs()
                    );
                } else {
                    eprintln!(
                        "cache {hash}: still linking, {} of {count} files ({}s)",
                        progress.load(Ordering::Relaxed),
                        elapsed.as_secs()
                    );
                }
            },
            || {
                #[cfg(target_os = "macos")]
                if cloning.load(Ordering::Relaxed) {
                    let from = entry(store, hash);
                    if !from.symlink_metadata()?.is_dir() {
                        bail!("cache tree {} is not a directory", from.display());
                    }
                    match clone_tree(&from, to) {
                        Ok(linked) => return Ok(linked),
                        Err(err) => {
                            match to.symlink_metadata() {
                                Err(e) if e.kind() == ErrorKind::NotFound => {}
                                _ => return Err(err),
                            }
                            eprintln!(
                                "cache {hash}: directory clone failed ({err:#}); falling back to per-file hardlinks"
                            );
                            cloning.store(false, Ordering::Relaxed);
                        }
                    }
                }
                link_tree_counting(&entry(store, hash), to, &progress)
            },
        )
    })?;
    crate::timing::measure_wait(&format!("cache {hash}: inventory validation"), || {
        if linked.method == LinkMethod::Clone {
            linked.files = count_files(to)?;
        }
        if linked.files != count {
            bail!(
                "store entry {hash} is incomplete: expected {count} files, found {}; run grove up --no-cache",
                linked.files
            );
        }
        Ok(())
    })?;
    Ok(linked)
}

#[cfg(target_os = "macos")]
fn macos_clone_enabled() -> bool {
    std::env::var_os("GROVE_MACOS_CLONE").as_deref() != Some(std::ffi::OsStr::new("0"))
}

#[cfg(target_os = "macos")]
fn clone_tree(from: &Path, to: &Path) -> Result<Linked> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(from.as_os_str().as_bytes())?;
    let destination = CString::new(to.as_os_str().as_bytes())?;
    const CLONE_NOFOLLOW: u32 = 1;
    // Both strings remain valid and NUL-terminated throughout the syscall.
    let result = unsafe { libc::clonefile(source.as_ptr(), destination.as_ptr(), CLONE_NOFOLLOW) };
    if result != 0 {
        return Err(contextualize_store_link_error(
            std::io::Error::last_os_error(),
            format!("cloning {} to {}", from.display(), to.display()),
            to,
        ));
    }
    Ok(Linked {
        method: LinkMethod::Clone,
        files: 0,
    })
}

fn contextualize_store_link_error(
    error: io::Error,
    context: String,
    destination: &Path,
) -> anyhow::Error {
    let storage_full = error.kind() == ErrorKind::StorageFull || error.raw_os_error() == Some(28);
    let error = anyhow::Error::new(error).context(context);
    if storage_full {
        error.context(format!(
            "the volume is full; free disk space, then retry (filesystem holding destination path: {})",
            destination.display()
        ))
    } else {
        error
    }
}

fn count_files(path: &Path) -> Result<u64> {
    let mut files = 0;
    for item in std::fs::read_dir(path)? {
        let item = item?;
        if item.file_type()?.is_dir() {
            files += count_files(&item.path())?;
        } else {
            files += 1;
        }
    }
    Ok(files)
}

/// Measure an entry published before entries recorded their size, once. The caller
/// holds a lease on it, and an entry never changes after publication, so the figure
/// stays true; two ups racing here write the same number.
pub fn record_bytes(store: &Path, hash: &str) -> Result<()> {
    let path = store.join(hash).join("bytes");
    if path.exists() {
        return Ok(());
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(
        &temporary,
        crate::footprint::tree_size(&entry(store, hash)).to_string(),
    )?;
    std::fs::rename(&temporary, &path).context("recording store entry size")
}

pub struct Size {
    pub bytes: u64,
    pub entries: usize,
    /// Entries published before entries recorded their size and not linked since.
    pub unmeasured: usize,
}

/// The store's size from what each entry recorded when it was published, without
/// walking a tree: `grove ls` reads this, and walking a 10G store under load took 89s.
/// Blocks two entries share through a package manager's own cache count in each.
pub fn size(store: &Path) -> Result<Size> {
    let mut size = Size {
        bytes: 0,
        entries: 0,
        unmeasured: 0,
    };
    let items = match std::fs::read_dir(store) {
        Ok(items) => items,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(size),
        Err(e) => return Err(e).with_context(|| format!("reading {}", store.display())),
    };
    for item in items {
        let name = item?.file_name().to_string_lossy().into_owned();
        if name.len() != 16 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        size.entries += 1;
        match std::fs::read_to_string(store.join(&name).join("bytes"))
            .ok()
            .and_then(|bytes| bytes.trim().parse::<u64>().ok())
        {
            Some(bytes) => size.bytes += bytes,
            None => size.unmeasured += 1,
        }
    }
    Ok(size)
}

/// Forget one entry. Worktrees that linked from it keep their blocks; only the store's
/// names go.
pub fn evict(store: &Path, hash: &str) -> Result<()> {
    let path = store.join(hash);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading store entry {hash}")),
    };
    let removed = if metadata.is_dir() {
        std::fs::remove_dir_all(&path)
    } else {
        std::fs::remove_file(&path)
    };
    match removed {
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
        (name.len() == 16
            && name.bytes().all(|b| b.is_ascii_hexdigit())
            && !referenced.contains(name))
            || stale_temp(store, name)
    });
    doomed.sort();
    Ok(doomed)
}

const STALE_TEMP_AGE: Duration = Duration::from_secs(60 * 60);

fn stale_temp(store: &Path, name: &str) -> bool {
    let Some((_, pid)) = name.rsplit_once(".tmp-") else {
        return false;
    };
    if pid.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let Ok(pid) = pid.parse::<u32>() else {
        return false;
    };
    if pid == 0 || is_grove_process(pid) {
        return false;
    }
    let path = store.join(name);
    let Ok(metadata) = std::fs::symlink_metadata(&path) else {
        return false;
    };
    if !metadata.is_dir() && !metadata.is_file() {
        return false;
    }
    metadata
        .modified()
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age > STALE_TEMP_AGE)
}

/// Keep entries owned by a live Grove process even when its PID is old enough to have
/// been reused. If `ps` cannot establish what a still-live PID runs, preserve the temp.
fn is_grove_process(pid: u32) -> bool {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output();
    if let Ok(output) = output
        && output.status.success()
        && let Some(name) = String::from_utf8_lossy(&output.stdout).lines().next()
    {
        let name = Path::new(name.trim())
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(name.trim());
        return name == "grove";
    }
    // `kill(pid, 0)` distinguishes a dead PID from a process that `ps` could not inspect.
    // SAFETY: signal zero only checks whether the process exists and sends no signal.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Bytes held by stale staging paths in a GC candidate list.
pub fn stale_temp_bytes(store: &Path, names: &[String]) -> Result<u64> {
    names.iter().try_fold(0_u64, |total, name| {
        if name.rsplit_once(".tmp-").is_none() {
            return Ok(total);
        }
        Ok(total + path_bytes(&store.join(name))?)
    })
}

fn path_bytes(path: &Path) -> Result<u64> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        std::fs::read_dir(path)?.try_fold(0_u64, |total, entry| {
            Ok(total + path_bytes(&entry?.path())?)
        })
    } else {
        Ok(metadata.len())
    }
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

#[cfg(test)]
mod tests {
    use super::contextualize_store_link_error;
    use std::io;
    use std::path::Path;

    #[test]
    fn storage_full_link_error_names_destination_and_recovery() {
        let error = contextualize_store_link_error(
            io::Error::from_raw_os_error(28),
            "linking /volume/worktree/node_modules/package/file.js".to_owned(),
            Path::new("/volume/worktree/node_modules/package/file.js"),
        );

        let chain = format!("{error:#}");
        assert!(chain.contains("linking /volume/worktree/node_modules/package/file.js"));
        assert!(chain.contains("volume is full"));
        assert!(chain.contains("/volume/worktree/node_modules/package/file.js"));
        assert!(chain.contains("free disk space, then retry"));
    }

    #[test]
    fn other_link_errors_do_not_get_storage_full_hint() {
        let error = contextualize_store_link_error(
            io::Error::from(io::ErrorKind::PermissionDenied),
            "linking /worktree/node_modules/package/file.js".to_owned(),
            Path::new("/worktree/node_modules/package/file.js"),
        );

        let chain = format!("{error:#}");
        assert!(chain.contains("linking /worktree/node_modules/package/file.js"));
        assert!(!chain.contains("volume is full"));
        assert!(!chain.contains("free disk space"));
    }
}
