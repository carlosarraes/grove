//! Lockfile projects in known config directories whose dependencies have no setup owner.
use crate::config::Config;
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

pub struct Uncovered {
    cwd: PathBuf,
    manifest: &'static str,
    lock: &'static str,
    command: &'static str,
    cache: &'static str,
    workspace: bool,
    packages: Vec<String>,
}

fn relative(cwd: &str) -> Option<PathBuf> {
    let mut path = PathBuf::new();
    for component in Path::new(cwd).components() {
        match component {
            Component::Normal(part) => path.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !path.pop() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(path)
}

pub fn detect(root: &Path, config: &Config) -> Vec<Uncovered> {
    let mut known = BTreeSet::from([PathBuf::new()]);
    let mut covered = BTreeSet::new();
    for service in &config.services {
        let Some(cwd) = relative(service.cwd.as_deref().unwrap_or(".")) else {
            continue;
        };
        if service.setup.is_some() {
            covered.insert(cwd.clone());
        }
        for parent in cwd.ancestors() {
            known.insert(parent.to_path_buf());
        }
    }
    let kinds = [
        (
            "package.json",
            "pnpm-lock.yaml",
            "corepack pnpm install --frozen-lockfile",
            "node_modules",
        ),
        (
            "package.json",
            "package-lock.json",
            "npm ci",
            "node_modules",
        ),
        (
            "package.json",
            "yarn.lock",
            "corepack yarn install --frozen-lockfile",
            "node_modules",
        ),
        (
            "package.json",
            "bun.lockb",
            "bun install --frozen-lockfile",
            "node_modules",
        ),
        (
            "package.json",
            "bun.lock",
            "bun install --frozen-lockfile",
            "node_modules",
        ),
        ("pyproject.toml", "uv.lock", "uv sync --locked", ".venv"),
    ];
    let mut found = Vec::new();
    for cwd in known.difference(&covered) {
        let dir = root.join(cwd);
        for (manifest, lock, command, cache) in kinds {
            if !dir.join(manifest).is_file() || !dir.join(lock).is_file() {
                continue;
            }
            let mut packages = BTreeSet::new();
            if manifest == "package.json"
                && let Ok(text) = std::fs::read_to_string(dir.join(manifest))
                && let Ok(json) = serde_json::from_str::<serde_json::Value>(&text)
            {
                for field in ["dependencies", "devDependencies"] {
                    if let Some(deps) = json[field].as_object() {
                        packages.extend(deps.keys().cloned());
                    }
                }
            }
            found.push(Uncovered {
                cwd: cwd.clone(),
                manifest,
                lock,
                command,
                cache,
                workspace: lock == "pnpm-lock.yaml" && dir.join("pnpm-workspace.yaml").is_file(),
                packages: packages.into_iter().collect(),
            });
        }
    }
    found
}

impl Uncovered {
    fn cwd(&self) -> String {
        if self.cwd.as_os_str().is_empty() {
            ".".into()
        } else {
            format!("./{}", self.cwd.display())
        }
    }

    pub fn warning(&self) -> String {
        let tools = if self.packages.is_empty() {
            String::new()
        } else {
            format!(" (packages: {})", self.packages.join(", "))
        };
        format!(
            "{}/{} + {} is not installed by any service; {} tools there{} will be missing.",
            self.cwd(),
            self.manifest,
            self.lock,
            if self.cache == ".venv" {
                ".venv/bin"
            } else {
                "node_modules/.bin"
            },
            tools
        )
    }

    pub fn snippet(&self, config: &Config) -> String {
        let base = if self.cwd.as_os_str().is_empty() {
            "root"
        } else {
            "dependencies"
        };
        let mut name = base.to_string();
        let mut suffix = 2;
        while config.services.iter().any(|s| s.name == name) {
            name = format!("{base}-{suffix}");
            suffix += 1;
        }
        let inputs = if self.workspace {
            format!("{:?}, \"pnpm-workspace.yaml\"", self.lock)
        } else {
            format!("{:?}", self.lock)
        };
        let install = if self.cache == ".venv" {
            "setup_inputs = [\"uv.lock\", \"pyproject.toml\"]".to_string()
        } else {
            format!("cache = {{ path = {:?}, key = [{inputs}] }}", self.cache)
        };
        format!(
            "[[service]]\nname = {name:?}\ncwd = {:?}\nsetup = {:?}\n{install}",
            self.cwd(),
            self.command
        )
    }
}
