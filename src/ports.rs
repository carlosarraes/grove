//! Port allocation: every instance gets a contiguous block nobody else holds.

use anyhow::{Result, bail};
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::net::TcpListener;
use std::ops::Range;

use crate::registry::Entry;

/// Deliberately far from 3000/5173/8000/8080. Every dev config carries a hardcoded
/// fallback to one of those, so an instance whose rewrite failed lands on a dead port and
/// says so, instead of quietly answering with a neighbour's data.
pub const RANGE: Range<u16> = 20000..30000;

/// Ports per instance are contiguous, so a block is easy to read and easy to reserve.
const STRIDE: u16 = 10;

/// The range in force: `GROVE_PORT_RANGE=low-high` when set, `RANGE` otherwise. One
/// machine running several registries — a test suite, or two state dirs — shares one port
/// space, and the bind test below only sees what is already listening; a range each is
/// what keeps their instances from landing on the same block in the same moment.
pub fn range() -> Range<u16> {
    std::env::var("GROVE_PORT_RANGE")
        .ok()
        .and_then(|s| parse_range(&s))
        .unwrap_or(RANGE)
}

pub fn parse_range(text: &str) -> Option<Range<u16>> {
    let (low, high) = text.split_once('-')?;
    let (low, high): (u16, u16) = (low.trim().parse().ok()?, high.trim().parse().ok()?);
    (low < high).then_some(low..high)
}

pub fn allocate(repo_key: &str, slug: &str, names: &[String]) -> Result<BTreeMap<String, u16>> {
    allocate_avoiding(repo_key, slug, names, &HashSet::new())
}

/// `taken` carries ports the registry has promised to other instances. They may not be
/// listening yet, so a bind test alone would call them free and hand out a duplicate.
pub fn allocate_avoiding(
    repo_key: &str,
    slug: &str,
    names: &[String],
    taken: &HashSet<u16>,
) -> Result<BTreeMap<String, u16>> {
    allocate_within(range(), repo_key, slug, names, taken)
}

pub fn allocate_within(
    range: Range<u16>,
    repo_key: &str,
    slug: &str,
    names: &[String],
    taken: &HashSet<u16>,
) -> Result<BTreeMap<String, u16>> {
    if names.is_empty() {
        return Ok(BTreeMap::new());
    }
    if names.len() > STRIDE as usize {
        bail!(
            "{} needs {} ports but a block holds {STRIDE}",
            slug,
            names.len()
        );
    }

    let blocks = blocks(&range);
    let first = fnv1a(&format!("{repo_key}/{slug}")) % u32::from(blocks);

    // Start at the block this instance hashes to, then walk. Hashing gives stability
    // across restarts; walking gives correctness when the machine is busy.
    for step in 0..blocks {
        let block = (first + u32::from(step)) % u32::from(blocks);
        let base = range.start + (block as u16) * STRIDE;
        let candidate: BTreeMap<String, u16> = names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.clone(), base + i as u16))
            .collect();

        if candidate
            .values()
            .all(|p| !taken.contains(p) && is_free(*p))
        {
            return Ok(candidate);
        }
    }

    Err(Exhausted { range }.into())
}

/// The block `held` sits in, offered to `names` if every port in it is unpromised and
/// unbound. How an exhausted range hands a stopped instance's block to someone who needs it.
pub fn take_block(
    range: &Range<u16>,
    held: &BTreeMap<String, u16>,
    names: &[String],
    taken: &HashSet<u16>,
) -> Option<BTreeMap<String, u16>> {
    if names.len() > STRIDE as usize {
        return None;
    }
    let port = held.values().find(|p| range.contains(p))?;
    let base = range.start + (port - range.start) / STRIDE * STRIDE;
    if base + STRIDE > range.end {
        return None;
    }
    let candidate: BTreeMap<String, u16> = names
        .iter()
        .enumerate()
        .map(|(i, n)| (n.clone(), base + i as u16))
        .collect();
    candidate
        .values()
        .all(|p| !taken.contains(p) && is_free(*p))
        .then_some(candidate)
}

pub fn blocks(range: &Range<u16>) -> u16 {
    ((range.end - range.start) / STRIDE).max(1)
}

/// Every block in the range is reserved or bound. Typed so the registry, which knows who
/// holds the blocks, can say so; allocation alone cannot.
#[derive(Debug)]
pub struct Exhausted {
    pub range: Range<u16>,
}

impl fmt::Display for Exhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no free port block in {:?}", self.range)
    }
}

impl std::error::Error for Exhausted {}

/// What releases a block. Stopping an instance does not: its block stays reserved so it
/// comes back on the same ports, until `grove release` or an exhausted `up` takes it.
pub const CURE: &str = "`grove release --stopped` (dry run, then `--yes`) frees blocks of stopped instances, keeping their worktrees and data; `grove prune` frees blocks of orphaned instances (worktree gone); `grove clean` (dry run, then `--yes`) removes idle worktrees with pushed commits or merged/closed PRs and releases theirs; GROVE_PORT_RANGE=low-high widens the range";

/// Blocks in a range the registry has promised away, by the state of their holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pressure {
    pub blocks: u16,
    pub running: usize,
    pub stopped: usize,
    pub orphaned: usize,
}

impl Pressure {
    pub fn of<'a>(range: &Range<u16>, entries: impl IntoIterator<Item = &'a Entry>) -> Self {
        let mut pressure = Pressure {
            blocks: blocks(range),
            running: 0,
            stopped: 0,
            orphaned: 0,
        };
        let mut seen = HashSet::new();
        for entry in entries {
            let Some(port) = entry.ports.values().find(|p| range.contains(p)) else {
                continue;
            };
            if !seen.insert((port - range.start) / STRIDE) {
                continue;
            }
            if entry.is_orphaned() {
                pressure.orphaned += 1;
            } else if entry.is_running() {
                pressure.running += 1;
            } else {
                pressure.stopped += 1;
            }
        }
        pressure
    }

    pub fn held(&self) -> usize {
        self.running + self.stopped + self.orphaned
    }

    pub fn percent(&self) -> usize {
        self.held() * 100 / usize::from(self.blocks)
    }

    pub fn holders(&self) -> String {
        format!(
            "{} running, {} stopped, {} orphaned",
            self.running, self.stopped, self.orphaned
        )
    }

    /// The exhaustion error `grove up` prints: who holds the blocks, why stopping does
    /// not help, and what does.
    pub fn exhausted_message(&self, range: &Range<u16>) -> String {
        let mut headline = format!(
            "no free port block in {range:?}: {} blocks, {} held by grove ({})",
            self.blocks,
            self.held(),
            self.holders()
        );
        if self.held() < usize::from(self.blocks) {
            headline.push_str("; the rest are bound by other processes");
        }
        format!(
            "{headline}\nno stopped instance has been idle long enough for `up` to take its block\nto free blocks: {CURE}"
        )
    }
}

/// Bound on all interfaces rather than loopback alone: a dev server listening on 0.0.0.0
/// would otherwise look free here and then fail to bind.
pub fn is_free(port: u16) -> bool {
    TcpListener::bind(("0.0.0.0", port)).is_ok() && TcpListener::bind(("127.0.0.1", port)).is_ok()
}

fn fnv1a(s: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in s.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}
