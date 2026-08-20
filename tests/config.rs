use grove::{config, llm};
use tempfile::TempDir;

fn worktree_with(toml: &str) -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    std::fs::write(dir.path().join(".grove.toml"), toml).expect("write .grove.toml");
    dir
}

#[test]
fn loads_services_from_the_worktree_root() {
    let wt = worktree_with(
        r#"
version = 1

[ports]
names = ["backend"]

[[service]]
name = "backend"
cwd = "backend"
command = "uv run uvicorn src.main:app --reload --port {{ port.backend }}"
"#,
    );

    let c = config::load(wt.path()).expect("load");

    assert_eq!(c.services.len(), 1);
    assert_eq!(c.services[0].name, "backend");
}

#[test]
fn missing_config_routes_the_agent_to_the_schema() {
    let dir = TempDir::new().expect("tempdir");

    let err = config::load(dir.path()).expect_err("must fail with no .grove.toml");

    let msg = format!("{err:#}");
    assert!(msg.contains(".grove.toml"), "should name the file: {msg}");
    assert!(
        msg.contains("grove --llm"),
        "should point at the schema so an agent can author one: {msg}"
    );
}

/// `grove --llm` hands this example to agents to copy. If it stops parsing, every agent
/// that follows the docs writes a config grove rejects.
#[test]
fn the_documented_example_parses() {
    let c = config::parse(llm::EXAMPLE).expect("the --llm example must parse");

    let names: Vec<_> = c.services.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["backend", "frontend"]);

    assert_eq!(c.ports.names, ["frontend", "backend"]);
    assert_eq!(c.secrets.len(), 2, "backend and frontend .env.local");
    assert_eq!(c.resources.len(), 1, "the shared mongo");
}

#[test]
fn llm_reference_explains_the_container_limit_and_existing_container_boundary() {
    let reference = llm::reference();

    assert!(reference.contains("nofile=64000:64000"), "{reference}");
    assert!(reference.contains("after the image"), "{reference}");
    assert!(reference.contains("preserve"), "{reference}");
    assert!(reference.contains("recreate"), "{reference}");
    assert!(reference.contains("dotenv"), "{reference}");
    assert!(reference.contains("disable dotenv loading"), "{reference}");
    assert!(reference.contains("process variables"), "{reference}");
    assert!(reference.contains("container incarnation"), "{reference}");
}

#[test]
fn llm_reference_routes_test_switches_and_browser_failures_to_their_owners() {
    let reference = llm::reference();

    assert!(
        reference.contains("repository-specific test switch"),
        "{reference}"
    );
    assert!(reference.contains("[secrets.set]"), "{reference}");
    assert!(reference.contains("grove run"), "{reference}");
    assert!(reference.contains("grove status"), "{reference}");
    assert!(reference.contains("CDP"), "{reference}");
    assert!(
        reference.contains("agent-browser doctor --fix"),
        "{reference}"
    );
}

#[test]
fn llm_reference_explains_opt_in_network_exposure() {
    let reference = llm::reference();

    for required in [
        "grove up --expose",
        "grove up --expose-host",
        "{{ host.public }}",
        "{{ host.bind }}",
        "plain `grove up`",
        "CORS",
        "all interfaces",
        "authentication bypass",
    ] {
        assert!(
            reference.contains(required),
            "the agent reference must explain {required:?}"
        );
    }

    assert!(
        llm::EXAMPLE.contains("http://{{ host.public }}:{{ port.backend }}"),
        "the browser-facing API URL must opt in to the public host"
    );
    assert!(
        llm::EXAMPLE.contains("--host {{ host.bind }}"),
        "the service must opt in to binding all interfaces"
    );
    assert!(
        llm::EXAMPLE.contains("CORS_ORIGINS = \"http://{{ host.public }}:{{ port.frontend }}\""),
        "the allowlist must opt in to the browser-visible origin"
    );
}

#[test]
fn secrets_carry_the_per_instance_overrides() {
    let c = config::parse(llm::EXAMPLE).expect("parse");
    let backend = c
        .secrets
        .iter()
        .find(|s| s.from == "backend/.env.local")
        .expect("backend secrets block");

    // Both frontend->backend pointers and the CORS origin are the rewrites that make
    // instances independent; losing any one silently crosses two instances.
    assert!(backend.set.contains_key("CORS_ORIGINS"));
    assert!(backend.set.contains_key("MONGODB_DATABASE"));

    let frontend = c
        .secrets
        .iter()
        .find(|s| s.from == "frontend/.env.local")
        .expect("frontend secrets block");
    assert!(frontend.set.contains_key("VITE_API_URL"));
    assert!(frontend.set.contains_key("VITE_PROXY_TARGET"));
}

/// Port names reach two places that constrain them: `{{ port.<name> }}` in a template,
/// where minijinja would read a hyphen as subtraction, and `GROVE_PORT_<NAME>`, which
/// must be a legal environment variable. Both are silent corruptions, so refuse early.
#[test]
fn a_port_name_that_would_break_templates_is_refused() {
    let err = config::parse(
        r#"
version = 1
[ports]
names = ["my-port"]
"#,
    )
    .expect_err("must refuse a hyphenated port name");

    let msg = format!("{err:#}");
    assert!(msg.contains("my-port"), "should name the offender: {msg}");
}

#[test]
fn seeds_declare_a_command_and_an_optional_guard() {
    let c = config::parse(
        r#"
version = 1

[[seed]]
name = "org"
cwd = "backend"
command = "uv run python -m tests.e2e_harness.seed"

[[seed]]
name = "propositions"
cwd = "backend"
if_exists = "fixtures/propositions.archive"
command = "mongorestore --db {{ db.name }} fixtures/propositions.archive"
"#,
    )
    .expect("parse");

    assert_eq!(c.seeds.len(), 2);
    assert_eq!(c.seeds[0].name, "org");
    assert!(c.seeds[0].if_exists.is_none());
    assert_eq!(
        c.seeds[1].if_exists.as_deref(),
        Some("fixtures/propositions.archive")
    );
}
