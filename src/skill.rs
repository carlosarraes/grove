//! Teaching agents that treeish exists.
//!
//! The skill is embedded in the binary so `treeish skill install` on a new machine needs
//! nothing but the binary, and so upgrading treeish upgrades the skill with it.

use anyhow::{Context, Result};
use std::path::PathBuf;

pub const SKILL: &str = include_str!("../skills/treeish/SKILL.md");

/// Install to the user's global skills directory. Global rather than per-repo because
/// treeish is repo-agnostic: an agent in any checkout should be able to reach for it.
pub fn install() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let dir = PathBuf::from(home).join(".claude/skills/treeish");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let path = dir.join("SKILL.md");
    std::fs::write(&path, SKILL).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}
