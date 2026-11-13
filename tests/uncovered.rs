use grove::{config, uncovered};
use tempfile::TempDir;

#[test]
fn detection_checks_only_known_directories_and_setup_cwds() {
    let dir = TempDir::new().unwrap();
    for path in ["", "frontend", "apps", "apps/api", "unrelated"] {
        let cwd = dir.path().join(path);
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("package.json"), "{}").unwrap();
        std::fs::write(cwd.join("pnpm-lock.yaml"), "").unwrap();
    }
    let config = config::parse(
        r#"
version = 1
[[service]]
name = "frontend"
cwd = "frontend/."
setup = "pnpm install"
[[service]]
name = "api"
cwd = "apps/api"
command = "serve"
"#,
    )
    .unwrap();
    let found = uncovered::detect(dir.path(), &config);
    assert_eq!(found.len(), 3);
    assert!(
        found
            .iter()
            .any(|p| p.warning().starts_with("./package.json"))
    );
    let covered = config::parse(
        "version = 1\n[[service]]\nname = \"root\"\ncwd = \".\"\nsetup = \"install\"",
    )
    .unwrap();
    assert!(uncovered::detect(dir.path(), &covered).is_empty());
}

#[test]
fn uv_and_node_lockfile_types_have_install_suggestions() {
    for (manifest, lock, command) in [
        (
            "package.json",
            "pnpm-lock.yaml",
            "corepack pnpm install --frozen-lockfile",
        ),
        ("package.json", "package-lock.json", "npm ci"),
        (
            "package.json",
            "yarn.lock",
            "corepack yarn install --frozen-lockfile",
        ),
        ("package.json", "bun.lock", "bun install --frozen-lockfile"),
        ("package.json", "bun.lockb", "bun install --frozen-lockfile"),
        ("pyproject.toml", "uv.lock", "uv sync --locked"),
    ] {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(lock), "").unwrap();
        let config = config::parse("version = 1").unwrap();
        assert!(uncovered::detect(dir.path(), &config).is_empty());
        std::fs::write(dir.path().join(manifest), "{}").unwrap();
        let found = uncovered::detect(dir.path(), &config);
        assert_eq!(found.len(), 1);
        let snippet = found[0].snippet(&config);
        assert!(snippet.contains(command));
        config::parse(&format!("version = 1\n{snippet}")).unwrap();
        let covered = config::parse(&format!(
            "version = 1\n[[service]]\nname = \"covered\"\nsetup = {command:?}"
        ))
        .unwrap();
        assert!(uncovered::detect(dir.path(), &covered).is_empty());
    }
}
