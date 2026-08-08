//! Teaching agents that grove exists.
//!
//! The skill is embedded in the binary so `grove skill install` on a new machine needs
//! nothing but the binary, and so upgrading grove upgrades the skill with it.

use anyhow::{Context, Result};
use std::path::PathBuf;

pub const SKILL: &str = include_str!("../skills/grove/SKILL.md");

/// The project's name before 0.1.2. Its skill describes a binary that no longer exists,
/// and two skills for one tool is worse than none.
const FORMER_NAME: &str = "treeish";

pub struct Installed {
    pub path: PathBuf,
    pub removed: Option<PathBuf>,
}

/// Install to the user's global skills directory. Global rather than per-repo because
/// grove is repo-agnostic: an agent in any checkout should be able to reach for it.
pub fn install() -> Result<Installed> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let skills = home.join(".claude/skills");

    let dir = skills.join("grove");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("SKILL.md");
    std::fs::write(&path, SKILL).with_context(|| format!("writing {}", path.display()))?;

    let former = skills.join(FORMER_NAME);
    let removed = if former.is_dir() {
        std::fs::remove_dir_all(&former)
            .with_context(|| format!("removing {}", former.display()))?;
        Some(former)
    } else {
        None
    };

    Ok(Installed { path, removed })
}
