mod common;

use assert_cmd::Command;
use common::Fixture;
use std::net::TcpListener;
use std::path::Path;
use tempfile::TempDir;

/// A service that really binds its assigned port, so the test exercises readiness
/// polling rather than a stub that returns instantly.
const CONFIG: &str = r#"
version = 1

[ports]
names = ["web"]

[[secrets]]
from = "backend/.env.local"
into = "backend/.env.local"

[secrets.set]
API_URL = "http://localhost:{{ port.web }}"
INSTANCE = "{{ slug }}"

[[service]]
name = "web"
command = "python3 -u -m http.server {{ port.web }}"
ready = { http = "http://127.0.0.1:{{ port.web }}/", timeout = "30s" }
"#;

struct Cli {
    state: TempDir,
    fx: Fixture,
    started: std::cell::RefCell<Vec<std::path::PathBuf>>,
    docker: Option<FakeDocker>,
}

struct FakeDocker {
    program: std::path::PathBuf,
    inspect: std::path::PathBuf,
}

impl FakeDocker {
    fn at(root: &Path, id: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;

        let program = root.join("fake-docker");
        let inspect = root.join("docker-inspect.json");
        std::fs::write(
            &program,
            "#!/bin/sh\ncase \"$1\" in\n  inspect) cat \"$GROVE_DOCKER_INSPECT\" ;;\n  logs) cat \"$GROVE_DOCKER_LOGS\" ;;\n  *) exit 0 ;;\nesac\n",
        )
        .expect("write fake docker");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake docker");
        let fake = FakeDocker { program, inspect };
        fake.set_container(id, true, 0, Some((64_000, 64_000)));
        fake
    }

    fn set_container(&self, id: &str, running: bool, exit_code: i64, nofile: Option<(u64, u64)>) {
        let ulimits = nofile.map_or_else(
            || "[]".to_string(),
            |(soft, hard)| format!(r#"[{{"Name":"nofile","Soft":{soft},"Hard":{hard}}}]"#),
        );
        std::fs::write(
            &self.inspect,
            format!(
                r#"[{{"Id":"{id}","State":{{"Running":{running},"ExitCode":{exit_code}}},"HostConfig":{{"Ulimits":{ulimits}}}}}]"#
            ),
        )
        .expect("write docker inspect fixture");
    }

    fn set_unreadable_inspect(&self) {
        std::fs::write(&self.inspect, "docker daemon response was unavailable")
            .expect("write unreadable inspect fixture");
    }

    fn set_logs(&self, root: &Path, body: &str) {
        std::fs::write(root.join("docker.log"), body).expect("write docker logs fixture");
    }
}

/// Stop whatever this test started, however the test ended. A `down` at the end of the
/// body is skipped by a panic, and the leaked server goes on holding its port — so a
/// single red test poisons every run after it.
impl Drop for Cli {
    fn drop(&mut self) {
        for worktree in self.started.borrow().iter() {
            let _ = Command::cargo_bin("grove")
                .expect("binary")
                .current_dir(worktree)
                .env("GROVE_STATE_DIR", self.state.path())
                .arg("down")
                .output();
        }
    }
}

impl Cli {
    fn new() -> Self {
        Cli::with_config(CONFIG)
    }

    fn with_config(config: &str) -> Self {
        let fx = Fixture::new();
        std::fs::create_dir_all(fx.main.join("backend")).expect("mkdir");
        std::fs::write(
            fx.main.join("backend/.env.local"),
            "AUTH__API_KEY=sk_live\nAPI_URL=http://localhost:8000\n",
        )
        .expect("main env");
        std::fs::write(fx.main.join(".grove.toml"), config).expect("config");
        // Gitignored, exactly as in a real repo — which is the whole reason a worktree
        // arrives without it. Committing it here would make every test a no-op.
        std::fs::write(fx.main.join(".gitignore"), ".env.local\n").expect("gitignore");
        common::git(&fx.main, &["add", "."]);
        common::git(&fx.main, &["commit", "-m", "add grove config"]);

        Cli {
            state: TempDir::new().expect("tempdir"),
            fx,
            started: std::cell::RefCell::new(Vec::new()),
            docker: None,
        }
    }

    fn with_fake_docker(config: &str, id: &str) -> Self {
        let mut cli = Cli::with_config(config);
        cli.docker = Some(FakeDocker::at(cli.state.path(), id));
        cli
    }

    /// A worktree whose services this harness is responsible for stopping.
    fn worktree(&self, slug: &str) -> std::path::PathBuf {
        let path = self.fx.add_worktree(slug);
        self.started.borrow_mut().push(path.clone());
        path
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
        let mut command = Command::cargo_bin("grove").expect("binary");
        command
            .current_dir(cwd)
            .env("GROVE_STATE_DIR", self.state.path());
        if let Some(docker) = &self.docker {
            command
                .env("GROVE_DOCKER", &docker.program)
                .env("GROVE_DOCKER_INSPECT", &docker.inspect)
                .env("GROVE_DOCKER_LOGS", self.state.path().join("docker.log"));
        }
        command.args(args).assert()
    }
}

#[test]
fn help_names_the_shared_container_open_file_limit() {
    let out = Command::cargo_bin("grove")
        .expect("binary")
        .arg("--help")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out);

    assert!(stdout.contains("nofile=64000"), "{stdout}");
}

/// The headline: a worktree with no env file, no ports, and no setup becomes a running
/// instance in one command.
#[test]
fn up_starts_an_instance_in_a_worktree_that_had_nothing() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    assert!(!wt.join("backend/.env.local").exists(), "precondition");

    let out = cli.run(&wt, &["up"]).success().get_output().stdout.clone();
    let stdout = String::from_utf8_lossy(&out).into_owned();

    let env = std::fs::read_to_string(wt.join("backend/.env.local")).expect("env written");
    assert!(env.contains("AUTH__API_KEY=sk_live"), "{env}");
    assert!(env.contains("INSTANCE=feat_search"), "{env}");

    let port: u16 = env
        .lines()
        .find_map(|l| l.strip_prefix("API_URL=http://localhost:"))
        .expect("API_URL rewritten to this instance's port")
        .parse()
        .expect("port");
    assert!(
        stdout.contains(&port.to_string()),
        "up must print the port an agent needs: {stdout}"
    );
    assert!(
        ureq::get(format!("http://127.0.0.1:{port}/"))
            .call()
            .is_ok(),
        "the service must actually be listening on {port}"
    );

    cli.run(&wt, &["down"]).success();
}

#[test]
fn two_worktrees_run_at_once_without_touching_each_other() {
    let cli = Cli::new();
    let a = cli.worktree("fix_login");
    let b = cli.worktree("feat_search");

    cli.run(&a, &["up"]).success();
    cli.run(&b, &["up"]).success();

    let port_of = |wt: &Path| -> u16 {
        std::fs::read_to_string(wt.join("backend/.env.local"))
            .expect("env")
            .lines()
            .find_map(|l| l.strip_prefix("API_URL=http://localhost:"))
            .expect("API_URL")
            .parse()
            .expect("port")
    };
    let (pa, pb) = (port_of(&a), port_of(&b));
    assert_ne!(pa, pb);
    assert!(ureq::get(format!("http://127.0.0.1:{pa}/")).call().is_ok());
    assert!(ureq::get(format!("http://127.0.0.1:{pb}/")).call().is_ok());

    // Stopping one must leave the other serving.
    cli.run(&a, &["down"]).success();
    assert!(
        ureq::get(format!("http://127.0.0.1:{pb}/")).call().is_ok(),
        "the second instance died with the first"
    );

    cli.run(&b, &["down"]).success();
}

/// `down` runs in a different process than `up`, so the pids have to survive in the
/// registry rather than in memory.
#[test]
fn down_stops_a_service_started_by_an_earlier_invocation() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();
    let port: u16 = std::fs::read_to_string(wt.join("backend/.env.local"))
        .expect("env")
        .lines()
        .find_map(|l| l.strip_prefix("API_URL=http://localhost:"))
        .expect("API_URL")
        .parse()
        .expect("port");

    cli.run(&wt, &["down"]).success();

    std::thread::sleep(std::time::Duration::from_millis(400));
    assert!(
        ureq::get(format!("http://127.0.0.1:{port}/"))
            .call()
            .is_err(),
        "the service is still listening on {port} after down"
    );
}

#[test]
fn status_reports_the_ports_as_json_for_agents() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();

    let out = cli
        .run(&wt, &["status", "--json"])
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: serde_json::Value =
        serde_json::from_slice(&out).expect("status --json must emit valid json");

    assert_eq!(parsed["slug"], "feat_search");
    assert!(parsed["ports"]["web"].as_u64().is_some(), "{parsed}");
    assert_eq!(parsed["services"]["web"]["running"], true, "{parsed}");

    cli.run(&wt, &["down"]).success();
}

#[test]
fn logs_show_what_the_service_printed() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();

    let out = cli
        .run(&wt, &["logs", "web"])
        .success()
        .get_output()
        .stdout
        .clone();

    assert!(
        String::from_utf8_lossy(&out).contains("Serving HTTP"),
        "logs must surface the service's own output"
    );

    cli.run(&wt, &["down"]).success();
}

/// The guard. Rendering here would overwrite the real `.env.local` that every instance
/// reads from.
#[test]
fn up_refuses_to_run_in_the_main_worktree() {
    let cli = Cli::new();
    let before = std::fs::read_to_string(cli.fx.main.join("backend/.env.local")).expect("read");

    let assert = cli.run(&cli.fx.main, &["up"]).failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();

    assert!(
        stderr.contains("main worktree") || stderr.contains("main checkout"),
        "must say why it refused: {stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(cli.fx.main.join("backend/.env.local")).expect("read"),
        before,
        "the main checkout's env was modified"
    );
}

#[test]
fn an_unconfigured_repo_points_the_agent_at_the_schema() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    std::fs::remove_file(wt.join(".grove.toml")).expect("remove config");

    let assert = cli.run(&wt, &["up"]).failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(stderr.contains("grove --llm"), "{stderr}");
}

#[test]
fn doctor_passes_in_a_worktree_that_is_ready_to_start() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");

    let out = cli
        .run(&wt, &["doctor"])
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out).into_owned();

    assert!(stdout.contains("backend/.env.local"), "{stdout}");
    assert!(stdout.contains("feat_search"), "{stdout}");
}

/// The failure grove exists to prevent, reported before anything is started.
#[test]
fn doctor_names_the_env_file_missing_from_the_main_checkout() {
    let cli = Cli::new();
    std::fs::remove_file(cli.fx.main.join("backend/.env.local")).expect("remove");
    let wt = cli.worktree("feat_search");

    let assert = cli.run(&wt, &["doctor"]).failure();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();

    assert!(stdout.contains("backend/.env.local"), "{stdout}");
    assert!(
        stdout.contains(&cli.fx.main.display().to_string()),
        "must point at the main checkout, where the fix belongs: {stdout}"
    );
}

#[test]
fn doctor_in_an_unconfigured_repo_points_at_the_schema() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    std::fs::remove_file(wt.join(".grove.toml")).expect("remove config");

    let assert = cli.run(&wt, &["doctor"]).failure();

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&assert.get_output().stdout),
        String::from_utf8_lossy(&assert.get_output().stderr)
    );
    assert!(combined.contains("grove --llm"), "{combined}");
}

#[test]
fn doctor_warns_that_the_main_worktree_cannot_be_started() {
    let cli = Cli::new();

    let assert = cli.run(&cli.fx.main, &["doctor"]).failure();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();

    assert!(
        stdout.contains("main worktree"),
        "must explain why this directory cannot host an instance: {stdout}"
    );
}

#[test]
fn doctor_reports_a_healthy_managed_resource_identity_and_limit() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listen");
    let port = listener.local_addr().expect("address").port();
    let cli = Cli::with_fake_docker(
        &resource_seed_config(port, "true"),
        "healthy-container-abcdef",
    );
    let wt = cli.worktree("feat_search");

    let output = cli.run(&wt, &["doctor"]).success();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout);

    assert!(stdout.contains("healthy-cont"), "{stdout}");
    assert!(stdout.contains("nofile=64000:64000"), "{stdout}");
}

#[test]
fn doctor_warns_when_a_managed_resource_has_an_old_limit() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listen");
    let port = listener.local_addr().expect("address").port();
    let cli = Cli::with_fake_docker(&resource_seed_config(port, "true"), "old-limit-container");
    cli.docker.as_ref().expect("fake docker").set_container(
        "old-limit-container",
        true,
        0,
        Some((1024, 1024)),
    );
    let wt = cli.worktree("feat_search");

    let output = cli.run(&wt, &["doctor"]).success();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout);

    assert!(stdout.contains("warn"), "{stdout}");
    assert!(stdout.contains("expected 64000:64000"), "{stdout}");
    assert!(stdout.contains("observed 1024:1024"), "{stdout}");
}

#[test]
fn doctor_explains_a_stopped_managed_resource() {
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("find port")
        .local_addr()
        .expect("address")
        .port();
    let cli = Cli::with_fake_docker(
        &resource_seed_config(port, "true"),
        "stopped-container-abcdef",
    );
    let docker = cli.docker.as_ref().expect("fake docker");
    docker.set_container("stopped-container-abcdef", false, 133, None);
    docker.set_logs(cli.state.path(), "MongoDB abort: Too many open files\n");
    let wt = cli.worktree("feat_search");

    let output = cli.run(&wt, &["doctor"]).failure();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout);

    assert!(stdout.contains("exit 133"), "{stdout}");
    assert!(stdout.contains("Too many open files"), "{stdout}");
    assert!(stdout.contains("preserve"), "{stdout}");
    assert!(stdout.contains("recreate"), "{stdout}");
}

#[test]
fn doctor_keeps_an_answering_external_resource_valid_when_docker_is_unavailable() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listen");
    let port = listener.local_addr().expect("address").port();
    let cli = Cli::with_fake_docker(&resource_seed_config(port, "true"), "unused");
    cli.docker
        .as_ref()
        .expect("fake docker")
        .set_unreadable_inspect();
    let wt = cli.worktree("feat_search");

    let output = cli.run(&wt, &["doctor"]).success();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout);

    assert!(stdout.contains("external or unobserved"), "{stdout}");
}

#[test]
fn doctor_fails_when_an_absent_resource_needs_unavailable_docker() {
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("find port")
        .local_addr()
        .expect("address")
        .port();
    let cli = Cli::with_fake_docker(&resource_seed_config(port, "true"), "unused");
    cli.docker
        .as_ref()
        .expect("fake docker")
        .set_unreadable_inspect();
    let wt = cli.worktree("feat_search");

    let output = cli.run(&wt, &["doctor"]).failure();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout);

    assert!(
        stdout.contains("Docker cannot inspect or start"),
        "{stdout}"
    );
}

#[test]
fn ls_lists_every_running_instance() {
    let cli = Cli::new();
    let a = cli.worktree("fix_login");
    let b = cli.worktree("feat_search");
    cli.run(&a, &["up"]).success();
    cli.run(&b, &["up"]).success();

    let out = cli.run(&a, &["ls"]).success().get_output().stdout.clone();
    let stdout = String::from_utf8_lossy(&out).into_owned();

    assert!(stdout.contains("fix_login"), "{stdout}");
    assert!(stdout.contains("feat_search"), "{stdout}");

    cli.run(&a, &["down"]).success();
    cli.run(&b, &["down"]).success();
}

#[test]
fn run_executes_a_command_with_the_instance_environment() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();

    let out = cli
        .run(&wt, &["run", "--", "sh", "-c", "echo $GROVE_PORT_WEB"])
        .success()
        .get_output()
        .stdout
        .clone();

    let printed = String::from_utf8_lossy(&out).trim().to_string();
    assert!(
        printed.parse::<u16>().is_ok(),
        "run must export this instance's ports, got {printed:?}"
    );

    cli.run(&wt, &["down"]).success();
}

#[test]
fn skill_install_writes_a_skill_agents_can_load() {
    let cli = Cli::new();
    let home = TempDir::new().expect("tempdir");

    Command::cargo_bin("grove")
        .expect("binary")
        .current_dir(&cli.fx.main)
        .env("HOME", home.path())
        .args(["skill", "install"])
        .assert()
        .success();

    let body = std::fs::read_to_string(home.path().join(".claude/skills/grove/SKILL.md"))
        .expect("skill installed to the global skills directory");
    assert!(body.starts_with("---\nname: grove\n"), "{body}");
    assert!(
        body.contains("description:"),
        "a model-invoked skill needs a description to be discoverable"
    );
    // The schema lives in the binary and is reached by pointer, so it cannot drift.
    assert!(body.contains("grove --llm"), "{body}");
    assert!(
        !body.contains("[[secrets]]"),
        "the skill must point at the schema rather than restate it"
    );
}

/// Reported from real use: after `down`, `ls` still described the instance as up, which
/// reads as a phantom port conflict when you are deciding whether a port is free.
#[test]
fn ls_reports_a_stopped_instance_as_stopped() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();

    // The instance's own row, not the whole page: the footer counts what is running on
    // the machine, and matching that would let this test pass on the wrong evidence.
    let row = |cli: &Cli| -> String {
        String::from_utf8_lossy(&cli.run(&wt, &["ls"]).success().get_output().stdout.clone())
            .lines()
            .find(|l| l.contains("feat_search"))
            .expect("the instance must be listed")
            .to_string()
    };

    assert!(row(&cli).contains("running"), "while up: {}", row(&cli));

    cli.run(&wt, &["down"]).success();

    let stopped = row(&cli);
    assert!(
        !stopped.contains("running"),
        "after down, nothing is listening — ls must not claim otherwise: {stopped}"
    );
    assert!(
        stopped.contains("stopped"),
        "the instance should still be listed, but as stopped: {stopped}"
    );
}

/// Two agents independently guessed `grove list` and `grove instances` and got an
/// error before recovering. The names cost nothing to accept.
#[test]
fn ls_answers_to_the_names_agents_actually_guess() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();

    for name in ["ls", "list", "instances"] {
        cli.run(&wt, &[name])
            .success()
            .stdout(predicates::str::contains("feat_search"));
    }

    cli.run(&wt, &["down"]).success();
}

/// Reported from parallel QA: agents shared agent-browser's default session, so one
/// agent's navigation stole another's tab — and the resulting auth error page reads as
/// an app bug in the wrong instance.
#[test]
fn run_isolates_the_browser_session_per_instance() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();

    let out = cli
        .run(
            &wt,
            &["run", "--", "sh", "-c", "echo $AGENT_BROWSER_SESSION"],
        )
        .success()
        .get_output()
        .stdout
        .clone();

    assert_eq!(String::from_utf8_lossy(&out).trim(), "feat_search");
    cli.run(&wt, &["down"]).success();
}

/// Code that reads `os.environ` before its settings library loads the .env file sees the
/// wrong value otherwise — a backend logged "ENVIRONMENT not set, defaulting to production"
/// while ENVIRONMENT=test sat on line 8 of the file grove had just written.
#[test]
fn the_instance_overrides_are_in_the_environment_not_only_the_file() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();

    let out = cli
        .run(&wt, &["run", "--", "sh", "-c", "echo $INSTANCE:$API_URL"])
        .success()
        .get_output()
        .stdout
        .clone();

    let printed = String::from_utf8_lossy(&out).trim().to_string();
    assert!(
        printed.starts_with("feat_search:http://localhost:"),
        "{printed}"
    );
    cli.run(&wt, &["down"]).success();
}

/// The project was called treeish until 0.1.1. Leaving its skill installed means agents
/// see two skills describing the same tool, one of them naming a binary that is gone.
#[test]
fn skill_install_removes_the_skill_from_the_old_name() {
    let cli = Cli::new();
    let home = TempDir::new().expect("tempdir");
    let stale = home.path().join(".claude/skills/treeish");
    std::fs::create_dir_all(&stale).expect("mkdir");
    std::fs::write(stale.join("SKILL.md"), "---\nname: treeish\n---\n").expect("stale skill");

    let out = Command::cargo_bin("grove")
        .expect("binary")
        .current_dir(&cli.fx.main)
        .env("HOME", home.path())
        .args(["skill", "install"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert!(!stale.exists(), "the treeish skill directory must be gone");
    assert!(
        home.path().join(".claude/skills/grove/SKILL.md").exists(),
        "the grove skill must be installed"
    );
    assert!(
        String::from_utf8_lossy(&out).contains("treeish"),
        "say what was removed rather than deleting silently"
    );
}

/// Every agent in a parallel batch hit `403 organization_not_found` and wrote its own
/// seed script, because a fresh per-instance database has no organisation row. Seeding
/// belongs to the instance, once, declared in the config.
const SEEDED_CONFIG: &str = r#"
version = 1

[ports]
names = ["web"]

[[secrets]]
from = "backend/.env.local"
into = "backend/.env.local"

[secrets.set]
API_URL = "http://localhost:{{ port.web }}"

[[seed]]
name = "org"
command = "echo $GROVE_SLUG >> seeded.log"

[[seed]]
name = "fixture"
if_exists = "fixtures/absent.archive"
command = "echo ran >> fixture.log"

[[service]]
name = "web"
command = "python3 -u -m http.server {{ port.web }}"
ready = { http = "http://127.0.0.1:{{ port.web }}/", timeout = "30s" }
"#;

#[test]
fn seeds_run_once_per_instance_and_skip_when_their_fixture_is_absent() {
    let cli = Cli::with_config(SEEDED_CONFIG);
    let wt = cli.worktree("feat_search");

    cli.run(&wt, &["up"]).success();
    let seeded = wt.join("seeded.log");
    assert_eq!(
        std::fs::read_to_string(&seeded).expect("seed must have run"),
        "feat_search\n",
        "the seed runs with the instance environment"
    );
    assert!(
        !wt.join("fixture.log").exists(),
        "a seed guarded by a missing path must be skipped, not run"
    );

    // A second `up` must not re-seed; that is the difference between provisioning and
    // a command you run every time.
    cli.run(&wt, &["up"]).success();
    assert_eq!(
        std::fs::read_to_string(&seeded).expect("read"),
        "feat_search\n"
    );

    cli.run(&wt, &["down"]).success();
}

#[test]
fn seed_force_reruns_without_reinstalling_anything() {
    let cli = Cli::with_config(SEEDED_CONFIG);
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();

    let out = cli
        .run(&wt, &["seed", "--force"])
        .success()
        .get_output()
        .stdout
        .clone();

    assert_eq!(
        std::fs::read_to_string(wt.join("seeded.log")).expect("read"),
        "feat_search\nfeat_search\n"
    );
    let stdout = String::from_utf8_lossy(&out).into_owned();
    assert!(stdout.contains("org"), "{stdout}");
    assert!(
        stdout.contains("skipped"),
        "the guarded seed should say why it did nothing: {stdout}"
    );

    cli.run(&wt, &["down"]).success();
}

fn resource_seed_config(port: u16, command: &str) -> String {
    format!(
        r#"
version = 1

[[resource]]
name = "mongo"
kind = "docker-shared"
image = "mongo:8"
port = {port}

[[seed]]
name = "org"
command = "{command}"
"#
    )
}

fn seed_marker(root: &Path) -> std::path::PathBuf {
    fn find(dir: &Path) -> Option<std::path::PathBuf> {
        for entry in std::fs::read_dir(dir).ok()? {
            let path = entry.ok()?.path();
            if path.file_name().and_then(|name| name.to_str()) == Some(".seed-org") {
                return Some(path);
            }
            if path.is_dir()
                && let Some(found) = find(&path)
            {
                return Some(found);
            }
        }
        None
    }
    find(root).expect("seed marker")
}

#[test]
fn seed_marker_is_invalidated_when_a_managed_resource_is_recreated() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listen");
    let port = listener.local_addr().expect("address").port();
    let cli = Cli::with_fake_docker(
        &resource_seed_config(port, "echo $GROVE_SLUG >> seeded.log"),
        "mongo-generation-a",
    );
    let wt = cli.worktree("feat_search");

    cli.run(&wt, &["up"]).success();
    cli.run(&wt, &["up"]).success();
    assert_eq!(
        std::fs::read_to_string(wt.join("seeded.log")).expect("seed log"),
        "feat_search\n"
    );

    cli.docker.as_ref().expect("fake docker").set_container(
        "mongo-generation-b",
        true,
        0,
        Some((64_000, 64_000)),
    );
    let output = cli.run(&wt, &["up"]).success();
    assert_eq!(
        std::fs::read_to_string(wt.join("seeded.log")).expect("seed log"),
        "feat_search\nfeat_search\n"
    );
    let stderr = String::from_utf8_lossy(&output.get_output().stderr);
    assert!(stderr.contains("resource mongo was recreated"), "{stderr}");
}

#[test]
fn a_matching_legacy_seed_marker_migrates_without_rerunning() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listen");
    let port = listener.local_addr().expect("address").port();
    let command = "echo $GROVE_SLUG >> seeded.log";
    let cli = Cli::with_fake_docker(&resource_seed_config(port, command), "mongo-generation-a");
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();
    let marker = seed_marker(cli.state.path());
    std::fs::write(&marker, command).expect("write legacy marker");

    cli.run(&wt, &["up"]).success();

    assert_eq!(
        std::fs::read_to_string(wt.join("seeded.log")).expect("seed log"),
        "feat_search\n"
    );
    let migrated: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(marker).expect("read migrated marker"))
            .expect("marker should migrate to json");
    assert_eq!(migrated["resources"]["mongo"], "mongo-generation-a");
}

#[test]
fn changing_a_seed_command_invalidates_its_marker() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listen");
    let port = listener.local_addr().expect("address").port();
    let cli = Cli::with_fake_docker(
        &resource_seed_config(port, "echo first >> seeded.log"),
        "mongo-generation-a",
    );
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();
    std::fs::write(
        wt.join(".grove.toml"),
        resource_seed_config(port, "echo second >> seeded.log"),
    )
    .expect("change seed command");

    let output = cli.run(&wt, &["up"]).success();

    assert_eq!(
        std::fs::read_to_string(wt.join("seeded.log")).expect("seed log"),
        "first\nsecond\n"
    );
    let stderr = String::from_utf8_lossy(&output.get_output().stderr);
    assert!(stderr.contains("command changed"), "{stderr}");
}

#[test]
fn a_malformed_structured_seed_marker_is_not_trusted() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listen");
    let port = listener.local_addr().expect("address").port();
    let cli = Cli::with_fake_docker(
        &resource_seed_config(port, "echo $GROVE_SLUG >> seeded.log"),
        "mongo-generation-a",
    );
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();
    std::fs::write(seed_marker(cli.state.path()), "{not valid json").expect("corrupt seed marker");

    let output = cli.run(&wt, &["up"]).success();

    assert_eq!(
        std::fs::read_to_string(wt.join("seeded.log")).expect("seed log"),
        "feat_search\nfeat_search\n"
    );
    let stderr = String::from_utf8_lossy(&output.get_output().stderr);
    assert!(stderr.contains("seed marker was invalid"), "{stderr}");
}

#[test]
fn an_unobservable_resource_does_not_invalidate_a_seed_marker() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listen");
    let port = listener.local_addr().expect("address").port();
    let cli = Cli::with_fake_docker(
        &resource_seed_config(port, "echo $GROVE_SLUG >> seeded.log"),
        "mongo-generation-a",
    );
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();
    cli.docker
        .as_ref()
        .expect("fake docker")
        .set_unreadable_inspect();

    cli.run(&wt, &["up"]).success();

    assert_eq!(
        std::fs::read_to_string(wt.join("seeded.log")).expect("seed log"),
        "feat_search\n",
        "a Docker observation failure is not evidence that the datastore was recreated"
    );
}

/// A deleted worktree cannot be `cd`-ed into, so `down` can never reach it — its services
/// keep running and its ports stay reserved with nothing left to release them.
#[test]
fn prune_reclaims_instances_whose_worktree_is_gone() {
    let cli = Cli::new();
    let doomed = cli.worktree("fix_login");
    let alive = cli.worktree("feat_search");
    cli.run(&doomed, &["up"]).success();
    cli.run(&alive, &["up"]).success();

    let port_of = |wt: &Path| -> u16 {
        std::fs::read_to_string(wt.join("backend/.env.local"))
            .expect("env")
            .lines()
            .find_map(|l| l.strip_prefix("API_URL=http://localhost:"))
            .expect("API_URL")
            .parse()
            .expect("port")
    };
    let (gone_port, kept_port) = (port_of(&doomed), port_of(&alive));

    std::fs::remove_dir_all(&doomed).expect("delete the worktree");
    let out = cli
        .run(&alive, &["prune"])
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out).into_owned();

    assert!(
        stdout.contains("fix_login"),
        "must name what it reclaimed: {stdout}"
    );
    std::thread::sleep(std::time::Duration::from_millis(400));
    assert!(
        ureq::get(format!("http://127.0.0.1:{gone_port}/"))
            .call()
            .is_err(),
        "the orphan's service is still listening on {gone_port}"
    );
    assert!(
        ureq::get(format!("http://127.0.0.1:{kept_port}/"))
            .call()
            .is_ok(),
        "prune stopped a live instance"
    );

    let listed = String::from_utf8_lossy(
        &cli.run(&alive, &["ls"])
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .into_owned();
    assert!(!listed.contains("fix_login"), "{listed}");
    assert!(listed.contains("feat_search"), "{listed}");

    cli.run(&alive, &["down"]).success();
}

#[test]
fn prune_says_so_when_there_is_nothing_to_reclaim() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");

    cli.run(&wt, &["prune"])
        .success()
        .stdout(predicates::str::contains("nothing"));
}

/// `ls` used to reap, which discarded the pids of a deleted worktree's services without
/// stopping them — leaving processes on the machine that grove could never reach again.
/// Listing is a read; reclaiming belongs to `prune`.
#[test]
fn ls_reports_an_orphan_rather_than_quietly_forgetting_it() {
    let cli = Cli::new();
    let doomed = cli.worktree("fix_login");
    cli.run(&doomed, &["up"]).success();
    std::fs::remove_dir_all(&doomed).expect("delete the worktree");

    let listed = String::from_utf8_lossy(
        &cli.run(&cli.fx.main, &["ls"])
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .into_owned();

    assert!(
        listed.contains("fix_login"),
        "the instance must stay listed until something stops it: {listed}"
    );
    assert!(
        listed.contains("orphan"),
        "and be marked so the state is obvious: {listed}"
    );

    // Still reclaimable, which is the whole point of not having forgotten it.
    cli.run(&cli.fx.main, &["prune"])
        .success()
        .stdout(predicates::str::contains("fix_login"));
}

/// The natural reach after editing a service that does not hot-reload. Today that means
/// `down && up`, which also stops everything else in the instance.
#[test]
fn restart_replaces_one_service_and_leaves_the_rest_alone() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();

    let pid_of = |name: &str| -> u64 {
        let out = cli
            .run(&wt, &["status", "--json"])
            .success()
            .get_output()
            .stdout
            .clone();
        let v: serde_json::Value = serde_json::from_slice(&out).expect("json");
        v["services"][name]["pid"]
            .as_u64()
            .expect("a pid in status")
    };
    let before = pid_of("web");

    cli.run(&wt, &["restart", "web"]).success();

    let after = pid_of("web");
    assert_ne!(before, after, "restart must actually replace the process");

    // And it is serving again, not merely respawned.
    let port: u16 = std::fs::read_to_string(wt.join("backend/.env.local"))
        .expect("env")
        .lines()
        .find_map(|l| l.strip_prefix("API_URL=http://localhost:"))
        .expect("API_URL")
        .parse()
        .expect("port");
    assert!(
        ureq::get(format!("http://127.0.0.1:{port}/"))
            .call()
            .is_ok()
    );

    cli.run(&wt, &["down"]).success();
}

/// The failure that nearly produced a false bug report: a service started before your
/// last edit keeps serving the old code, and nothing says so.
#[test]
fn status_warns_when_a_service_predates_the_newest_source_change() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();

    let clean = String::from_utf8_lossy(
        &cli.run(&wt, &["status"])
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .into_owned();
    assert!(!clean.contains("stale"), "nothing edited yet: {clean}");

    // Edit a tracked source file, the way you would mid-ticket.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(wt.join("README.md"), "changed\n").expect("edit");

    let warned = String::from_utf8_lossy(
        &cli.run(&wt, &["status"])
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .into_owned();
    assert!(
        warned.contains("stale"),
        "a service older than the newest edit must be flagged: {warned}"
    );
    assert!(
        warned.contains("grove restart"),
        "and say how to fix it: {warned}"
    );

    cli.run(&wt, &["down"]).success();
}

/// `logs` replayed the whole build first, so its head showed dependency resolution rather
/// than the service booting.
#[test]
fn logs_can_show_only_this_run_and_only_the_tail() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();
    cli.run(&wt, &["restart", "web"]).success();

    let all = String::from_utf8_lossy(
        &cli.run(&wt, &["logs", "web"])
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .into_owned();
    let since = String::from_utf8_lossy(
        &cli.run(&wt, &["logs", "web", "--since-restart"])
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .into_owned();

    // Assert the relationship, not an exact line count. Every test harness here owns a
    // separate registry, so two running concurrently can be handed the same port and one
    // readiness probe can be answered by the other's server — which perturbs how many
    // startup lines land in a given log without saying anything about this behaviour.
    assert!(
        all.len() > since.len(),
        "--since-restart must drop earlier output"
    );
    assert!(
        all.ends_with(&since),
        "--since-restart must be a suffix of the whole log"
    );
    assert!(
        since.contains("Serving HTTP"),
        "and must still reach back to this run's startup: {since}"
    );

    let tail = String::from_utf8_lossy(
        &cli.run(&wt, &["logs", "web", "-n", "1"])
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .into_owned();
    assert_eq!(tail.lines().count(), 1, "{tail}");

    cli.run(&wt, &["down"]).success();
}

fn registry_of(cli: &Cli) -> grove::registry::Registry {
    grove::registry::Registry::at(cli.state.path().join("registry.json"))
}

/// Age an instance so a later command's effect on the idle clock is visible without the
/// test sleeping through it.
///
/// Both halves of the signal have to move. `up` writes a service log on the way past, so
/// an instance that only had its clock rolled back is still "busy" — correctly, and that
/// is the browser-QA protection working, but it means a test wanting a stale instance has
/// to backdate the logs too.
fn backdate(cli: &Cli, worktree: &Path, seconds: u64) {
    let entry = registry_of(cli).get(worktree).expect("get").expect("entry");

    if let Some(dir) = &entry.instance_dir {
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(seconds);
        for log in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let _ = std::fs::File::options()
                .write(true)
                .open(log.path())
                .map(|f| f.set_modified(when));
        }
    }

    // Edited in the file rather than through `record`, which will not move the clock
    // backwards — that invariant is what stops a command writing back a stale entry and
    // undoing its own touch, and a test wanting an aged instance has to go around it.
    let path = cli.state.path().join("registry.json");
    let mut state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("registry")).expect("json");
    state["instances"][worktree.to_str().expect("utf-8 path")]["last_used"] =
        (grove::registry::now() - seconds).into();
    std::fs::write(&path, serde_json::to_string_pretty(&state).expect("json")).expect("write");
}

fn last_used(cli: &Cli, worktree: &Path) -> Option<u64> {
    registry_of(cli)
        .get(worktree)
        .expect("get")
        .expect("entry")
        .last_used
}

/// What `--idle` sweeps is decided by this clock, so which commands advance it *is* the
/// feature. Working in an instance keeps it; looking at one must not, because agents poll
/// `status` in a loop and `ls` is what you read while deciding what to stop — either one
/// refreshing the thing it reports on would make the sweep list permanently empty.
#[test]
fn work_advances_the_idle_clock_and_inspection_leaves_it_alone() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();

    assert!(
        last_used(&cli, &wt).is_some(),
        "up must mark the instance used"
    );

    for inspection in [vec!["status"], vec!["ls"]] {
        backdate(&cli, &wt, 7200);
        let before = last_used(&cli, &wt);
        cli.run(&wt, &inspection).success();
        assert_eq!(
            last_used(&cli, &wt),
            before,
            "`grove {}` is a read and must not look like work",
            inspection.join(" ")
        );
    }

    for work in [vec!["run", "--", "true"], vec!["seed"], vec!["logs", "web"]] {
        backdate(&cli, &wt, 7200);
        cli.run(&wt, &work).success();
        let idle = grove::registry::now() - last_used(&cli, &wt).expect("used");
        assert!(
            idle < 60,
            "`grove {}` means someone is working here, but it read as {idle}s idle",
            work.join(" ")
        );
    }

    cli.run(&wt, &["down"]).success();
}

impl Cli {
    /// Real machine load is not something a test can arrange, so grove lets both halves
    /// be forced. Set on the child, never on this process — `set_var` is unsafe in this
    /// edition and would race the other tests sharing this binary.
    fn run_on_machine(
        &self,
        cwd: &Path,
        args: &[&str],
        load: &str,
        cores: &str,
    ) -> assert_cmd::assert::Assert {
        Command::cargo_bin("grove")
            .expect("binary")
            .current_dir(cwd)
            .env("GROVE_STATE_DIR", self.state.path())
            .env("GROVE_LOAD", load)
            .env("GROVE_CORES", cores)
            .args(args)
            .assert()
    }
}

/// The failure mode of this whole feature is crying wolf. An agent that learns grove
/// warns on a quiet machine learns to skip grove's warnings, including the one that
/// would have saved it an hour — so the quiet case is the case worth guarding.
#[test]
fn a_calm_machine_is_never_warned_about() {
    let cli = Cli::new();
    let a = cli.worktree("fix_login");
    let b = cli.worktree("feat_search");
    cli.run(&a, &["up"]).success();

    let out = cli.run_on_machine(&b, &["up"], "0.4", "16");
    let stderr = String::from_utf8_lossy(&out.success().get_output().stderr).into_owned();

    assert!(
        !stderr.contains("warning"),
        "two instances on an idle machine is not a pile-up: {stderr}"
    );

    cli.run(&a, &["down"]).success();
    cli.run(&b, &["down"]).success();
}

/// The other false positive: one big type-check crosses load 16 on a sixteen-core box
/// with only a couple of instances up. Nothing there is reclaimable, so the warning would
/// offer to stop an idle box that is not the problem.
#[test]
fn a_loaded_machine_with_few_instances_is_never_warned_about() {
    let cli = Cli::new();
    let a = cli.worktree("fix_login");
    let b = cli.worktree("feat_search");
    cli.run(&a, &["up"]).success();

    let out = cli.run_on_machine(&b, &["up"], "40", "8");
    let stderr = String::from_utf8_lossy(&out.success().get_output().stderr).into_owned();

    assert!(
        !stderr.contains("warning"),
        "a busy machine with nothing to reclaim is not grove's news to break: {stderr}"
    );

    cli.run(&a, &["down"]).success();
    cli.run(&b, &["down"]).success();
}

/// The pile-up forms one agent at a time, and each one is currently told nothing about
/// what it is joining. The warning has to name the boxes — "would stop 7" answers how
/// many when the question is which.
#[test]
fn joining_a_crowded_machine_warns_and_names_what_can_be_reclaimed() {
    let cli = Cli::new();
    let crowd: Vec<_> = ["one", "two", "three", "four"]
        .iter()
        .map(|s| cli.worktree(s))
        .collect();
    for wt in &crowd {
        cli.run(wt, &["up"]).success();
    }
    backdate(&cli, &crowd[0], 6 * 3600);

    let joining = cli.worktree("five");
    let out = cli.run_on_machine(&joining, &["up"], "30", "8");
    let stderr = String::from_utf8_lossy(&out.success().get_output().stderr).into_owned();

    assert!(stderr.contains("warning"), "{stderr}");
    assert!(stderr.contains("4 instances"), "{stderr}");
    assert!(stderr.contains("load 30"), "{stderr}");
    assert!(
        stderr.contains("one (6h)"),
        "the warning must name the stale box and its age: {stderr}"
    );
    assert!(
        stderr.contains("grove down --idle"),
        "a warning without the command that fixes it is a complaint: {stderr}"
    );

    for wt in crowd.iter().chain([&joining]) {
        cli.run(wt, &["down"]).success();
    }
}

/// A prescription that would stop nothing teaches the reader that grove's prescriptions
/// are noise.
#[test]
fn a_crowd_with_nothing_stale_is_warned_about_without_a_prescription() {
    let cli = Cli::new();
    let crowd: Vec<_> = ["one", "two", "three", "four"]
        .iter()
        .map(|s| cli.worktree(s))
        .collect();
    for wt in &crowd {
        cli.run(wt, &["up"]).success();
    }

    let joining = cli.worktree("five");
    let out = cli.run_on_machine(&joining, &["up"], "30", "8");
    let stderr = String::from_utf8_lossy(&out.success().get_output().stderr).into_owned();

    assert!(stderr.contains("warning"), "{stderr}");
    assert!(
        !stderr.contains("grove down --idle"),
        "every box is in use; there is nothing to propose: {stderr}"
    );

    for wt in crowd.iter().chain([&joining]) {
        cli.run(wt, &["down"]).success();
    }
}

/// The hours this feature exists to save went into diagnosing tests that failed because
/// the machine was buried, not because the branch was wrong. grove is in the loop for
/// `grove run`, and it knows the exit code — so failure is the one moment worth speaking
/// up, and success is the moment worth staying quiet.
#[test]
fn a_failing_run_on_a_buried_machine_says_so_and_a_passing_one_does_not() {
    let cli = Cli::new();
    let crowd: Vec<_> = ["one", "two", "three", "four"]
        .iter()
        .map(|s| cli.worktree(s))
        .collect();
    for wt in &crowd {
        cli.run(wt, &["up"]).success();
    }

    let failed = cli
        .run_on_machine(&crowd[0], &["run", "--", "false"], "30", "8")
        .failure();
    let stderr = String::from_utf8_lossy(&failed.get_output().stderr).into_owned();
    assert!(stderr.contains("load 30"), "{stderr}");
    assert!(
        stderr.contains("may be the machine"),
        "the note has to name the alternative explanation, or it is just trivia: {stderr}"
    );
    assert_eq!(
        failed.get_output().status.code(),
        Some(1),
        "the note must not disturb the exit code a caller is branching on"
    );

    let passed = cli
        .run_on_machine(&crowd[0], &["run", "--", "true"], "30", "8")
        .success();
    assert!(
        String::from_utf8_lossy(&passed.get_output().stderr).is_empty(),
        "a green run on a busy machine is not news"
    );

    let quiet = cli
        .run_on_machine(&crowd[0], &["run", "--", "false"], "0.2", "8")
        .failure();
    assert!(
        String::from_utf8_lossy(&quiet.get_output().stderr).is_empty(),
        "an ordinary failure on an idle machine is the branch's fault and grove should not muddy it"
    );

    for wt in &crowd {
        cli.run(wt, &["down"]).success();
    }
}

/// Eighteen rows and the stale ones scattered through them is how a pile-up goes
/// unnoticed. The instances worth reclaiming belong at the top, and the machine's state
/// belongs where someone deciding what to stop will actually read it.
#[test]
fn ls_puts_the_most_neglected_instance_first_and_reports_the_machine() {
    let cli = Cli::new();
    let fresh = cli.worktree("worked_on_lately");
    let forgotten = cli.worktree("forgotten_since_lunch");
    cli.run(&fresh, &["up"]).success();
    cli.run(&forgotten, &["up"]).success();
    backdate(&cli, &forgotten, 6 * 3600);

    let out = cli
        .run_on_machine(&fresh, &["ls"], "0.3", "16")
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out).into_owned();

    let stale = stdout.find("forgotten_since_lunch").expect("listed");
    let recent = stdout.find("worked_on_lately").expect("listed");
    assert!(
        stale < recent,
        "the reclaim candidate has to surface, not hide in the middle: {stdout}"
    );
    assert!(
        stdout.contains("6h"),
        "an idle age is what makes a row actionable: {stdout}"
    );
    assert!(
        stdout.contains("load 0.3 on 16 cores"),
        "ls is where someone deciding what to stop is already looking: {stdout}"
    );
    assert!(
        !stdout.contains("grove down --idle"),
        "a calm machine needs no prescription: {stdout}"
    );

    cli.run(&fresh, &["down"]).success();
    cli.run(&forgotten, &["down"]).success();
}

/// `pgrep | wc -l` and `uptime` got reached for before `grove ls` did. An agent should be
/// able to decide from grove directly rather than parsing a table or shelling out.
#[test]
fn ls_json_carries_what_an_agent_needs_to_decide() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();
    backdate(&cli, &wt, 3 * 3600);

    let out = cli
        .run_on_machine(&wt, &["ls", "--json"], "26.1", "16")
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: serde_json::Value =
        serde_json::from_slice(&out).expect("ls --json must emit valid json");

    assert_eq!(parsed["load"], 26.1);
    assert_eq!(parsed["cores"], 16);
    assert_eq!(parsed["running"], 1);

    let instance = &parsed["instances"][0];
    assert_eq!(instance["slug"], "feat_search");
    assert_eq!(instance["running"], true);
    assert!(instance["ports"]["web"].as_u64().is_some(), "{parsed}");
    let idle = instance["idle_seconds"].as_u64().expect("idle age");
    assert!((3 * 3600..3 * 3600 + 120).contains(&idle), "{parsed}");

    cli.run(&wt, &["down"]).success();
}

/// The point of `--idle` over `prune`: a forgotten box is one you want back tomorrow, and
/// the URL an agent wrote down has to still work when it comes back.
#[test]
fn a_swept_instance_keeps_its_ports_and_the_current_one_is_spared() {
    let cli = Cli::new();
    let here = cli.worktree("where_i_am_working");
    let stale = cli.worktree("forgotten");
    cli.run(&here, &["up"]).success();
    cli.run(&stale, &["up"]).success();

    let port_of = |wt: &Path| -> u16 {
        std::fs::read_to_string(wt.join("backend/.env.local"))
            .expect("env")
            .lines()
            .find_map(|l| l.strip_prefix("API_URL=http://localhost:"))
            .expect("API_URL")
            .parse()
            .expect("port")
    };
    let (mine, theirs) = (port_of(&here), port_of(&stale));

    backdate(&cli, &stale, 3 * 3600);
    backdate(&cli, &here, 3 * 3600);

    let out = cli.run(&here, &["down", "--idle", "2h"]).success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();

    assert!(stdout.contains("forgotten"), "{stdout}");
    assert!(
        stdout.contains("3h"),
        "naming the age is what makes the list checkable before it is acted on: {stdout}"
    );
    assert!(
        !stdout.contains("where_i_am_working"),
        "sweeping the box you are standing in is the one genuinely surprising outcome: {stdout}"
    );

    std::thread::sleep(std::time::Duration::from_millis(400));
    assert!(
        ureq::get(format!("http://127.0.0.1:{theirs}/"))
            .call()
            .is_err(),
        "the stale instance is still serving on {theirs}"
    );
    assert!(
        ureq::get(format!("http://127.0.0.1:{mine}/"))
            .call()
            .is_ok(),
        "the instance being worked in was stopped"
    );

    cli.run(&stale, &["up"]).success();
    assert_eq!(
        port_of(&stale),
        theirs,
        "a swept instance must come back on the ports it had, or every URL written down \
         while it ran is now wrong"
    );

    cli.run(&here, &["down"]).success();
    cli.run(&stale, &["down"]).success();
}

/// The blast radius spans other people's work, so there has to be a way to read the list
/// before committing to it.
#[test]
fn a_dry_run_names_the_casualties_without_creating_any() {
    let cli = Cli::new();
    let here = cli.worktree("where_i_am_working");
    let stale = cli.worktree("forgotten");
    cli.run(&here, &["up"]).success();
    cli.run(&stale, &["up"]).success();
    backdate(&cli, &stale, 3 * 3600);

    let out = cli
        .run(&here, &["down", "--idle", "2h", "--dry-run"])
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();

    assert!(stdout.contains("forgotten"), "{stdout}");
    assert!(stdout.contains("would stop"), "{stdout}");

    let still_up: serde_json::Value = serde_json::from_slice(
        &cli.run(&stale, &["status", "--json"])
            .success()
            .get_output()
            .stdout,
    )
    .expect("json");
    assert_eq!(
        still_up["services"]["web"]["running"], true,
        "a dry run that stopped something is not a dry run"
    );

    cli.run(&here, &["down"]).success();
    cli.run(&stale, &["down"]).success();
}

/// An agent deep in QA on a sibling worktree is exactly who this command is dangerous to,
/// so an instance still serving traffic is not a candidate however long ago its last
/// grove command was.
#[test]
fn an_instance_still_serving_traffic_survives_a_sweep() {
    let cli = Cli::new();
    let here = cli.worktree("where_i_am_working");
    let busy = cli.worktree("mid_browser_qa");
    cli.run(&here, &["up"]).success();
    cli.run(&busy, &["up"]).success();

    // Its last grove command was hours ago, but the service has been logging since.
    backdate(&cli, &busy, 3 * 3600);
    let logs = registry_of(&cli)
        .get(&busy)
        .expect("get")
        .expect("entry")
        .instance_dir
        .expect("instance dir");
    {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(logs.join("web.log"))
            .expect("log")
            .write_all(b"GET /api/quotes 200\n")
            .expect("append");
    }

    let out = cli
        .run(&here, &["down", "--idle", "2h", "--dry-run"])
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();

    assert!(
        !stdout.contains("mid_browser_qa"),
        "an instance whose service is still writing is in use, whoever last ran a grove \
         command there: {stdout}"
    );

    cli.run(&here, &["down"]).success();
    cli.run(&busy, &["down"]).success();
}

/// Dropping every swept instance's database would mean loading each worktree's config to
/// find its datastore. Refusing is better than a flag that silently does nothing.
#[test]
fn purging_a_whole_machine_is_refused_rather_than_half_done() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();

    let out = cli.run(&wt, &["down", "--purge", "--idle", "2h"]).failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();

    assert!(
        stderr.contains("--purge"),
        "the refusal has to name the flag it is refusing: {stderr}"
    );
    assert!(
        stderr.contains("one instance") || stderr.contains("grove down --purge"),
        "and point at the form that does work: {stderr}"
    );

    cli.run(&wt, &["down"]).success();
}

/// The blunt form, for when you know you are done with everything else on the machine.
#[test]
fn all_but_this_stops_the_siblings_and_spares_the_one_you_are_in() {
    let cli = Cli::new();
    let here = cli.worktree("where_i_am_working");
    let other = cli.worktree("someone_elses");
    cli.run(&here, &["up"]).success();
    cli.run(&other, &["up"]).success();

    let out = cli.run(&here, &["down", "--all-but-this"]).success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();

    assert!(stdout.contains("someone_elses"), "{stdout}");
    assert!(!stdout.contains("where_i_am_working"), "{stdout}");

    let mine: serde_json::Value = serde_json::from_slice(
        &cli.run(&here, &["status", "--json"])
            .success()
            .get_output()
            .stdout,
    )
    .expect("json");
    assert_eq!(
        mine["services"]["web"]["running"], true,
        "--all-but-this must mean all but this"
    );

    cli.run(&here, &["down"]).success();
}

/// A sweep that matched nothing must say so rather than printing a blank success, which
/// reads as "it worked" when in fact the window was wrong.
#[test]
fn a_sweep_that_matches_nothing_says_so() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();

    let out = cli.run(&wt, &["down", "--idle", "9h"]).success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();

    assert!(stdout.contains("nothing to stop"), "{stdout}");

    cli.run(&wt, &["down"]).success();
}
