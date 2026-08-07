//! Port allocation: every instance gets a contiguous block nobody else holds.

use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::net::TcpListener;
use std::ops::Range;

/// Deliberately far from 3000/5173/8000/8080. Every dev config carries a hardcoded
/// fallback to one of those, so an instance whose rewrite failed lands on a dead port and
/// says so, instead of quietly answering with a neighbour's data.
pub const RANGE: Range<u16> = 20000..30000;

/// Ports per instance are contiguous, so a block is easy to read and easy to reserve.
const STRIDE: u16 = 10;

pub fn allocate(repo_key: &str, slug: &str, names: &[String]) -> Result<BTreeMap<String, u16>> {
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

    let blocks = (RANGE.end - RANGE.start) / STRIDE;
    let first = fnv1a(&format!("{repo_key}/{slug}")) % u32::from(blocks);

    // Start at the block this instance hashes to, then walk. Hashing gives stability
    // across restarts; walking gives correctness when the machine is busy.
    for step in 0..blocks {
        let block = (first + u32::from(step)) % u32::from(blocks);
        let base = RANGE.start + (block as u16) * STRIDE;
        let candidate: BTreeMap<String, u16> = names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.clone(), base + i as u16))
            .collect();

        if candidate.values().all(|p| is_free(*p)) {
            return Ok(candidate);
        }
    }

    bail!("no free port block in {RANGE:?}; run `treeish ls` and stop instances you are done with")
}

/// Bound on all interfaces rather than loopback alone: a dev server listening on 0.0.0.0
/// would otherwise look free here and then fail to bind.
fn is_free(port: u16) -> bool {
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
