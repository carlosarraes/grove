mod common;

use assert_cmd::Command;
use common::Fixture;
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
}

/// Stop whatever this test started, however the test ended. A `down` at the end of the
/// body is skipped by a panic, and the leaked server goes on holding its port — so a
/// single red test poisons every run after it.
impl Drop for Cli {
    fn drop(&mut self) {
        for worktree in self.started.borrow().iter() {
            let _ = Command::cargo_bin("treeish")
                .expect("binary")
                .current_dir(worktree)
                .env("TREEISH_STATE_DIR", self.state.path())
                .arg("down")
                .output();
        }
    }
}

impl Cli {
    fn new() -> Self {
        let fx = Fixture::new();
        std::fs::create_dir_all(fx.main.join("backend")).expect("mkdir");
        std::fs::write(
            fx.main.join("backend/.env.local"),
            "WORKOS__API_KEY=sk_live\nAPI_URL=http://localhost:8000\n",
        )
        .expect("main env");
        std::fs::write(fx.main.join(".treeish.toml"), CONFIG).expect("config");
        // Gitignored, exactly as in a real repo — which is the whole reason a worktree
        // arrives without it. Committing it here would make every test a no-op.
        std::fs::write(fx.main.join(".gitignore"), ".env.local\n").expect("gitignore");
        common::git(&fx.main, &["add", "."]);
        common::git(&fx.main, &["commit", "-m", "add treeish config"]);

        Cli {
            state: TempDir::new().expect("tempdir"),
            fx,
            started: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// A worktree whose services this harness is responsible for stopping.
    fn worktree(&self, slug: &str) -> std::path::PathBuf {
        let path = self.fx.add_worktree(slug);
        self.started.borrow_mut().push(path.clone());
        path
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
        Command::cargo_bin("treeish")
            .expect("binary")
            .current_dir(cwd)
            .env("TREEISH_STATE_DIR", self.state.path())
            .args(args)
            .assert()
    }
}

/// The headline: a worktree with no env file, no ports, and no setup becomes a running
/// instance in one command.
#[test]
fn up_starts_an_instance_in_a_worktree_that_had_nothing() {
    let cli = Cli::new();
    let wt = cli.worktree("mon_2695");
    assert!(!wt.join("backend/.env.local").exists(), "precondition");

    let out = cli.run(&wt, &["up"]).success().get_output().stdout.clone();
    let stdout = String::from_utf8_lossy(&out).into_owned();

    let env = std::fs::read_to_string(wt.join("backend/.env.local")).expect("env written");
    assert!(env.contains("WORKOS__API_KEY=sk_live"), "{env}");
    assert!(env.contains("INSTANCE=mon_2695"), "{env}");

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
    let a = cli.worktree("mon_2694");
    let b = cli.worktree("mon_2695");

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
    let wt = cli.worktree("mon_2695");
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
    let wt = cli.worktree("mon_2695");
    cli.run(&wt, &["up"]).success();

    let out = cli
        .run(&wt, &["status", "--json"])
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: serde_json::Value =
        serde_json::from_slice(&out).expect("status --json must emit valid json");

    assert_eq!(parsed["slug"], "mon_2695");
    assert!(parsed["ports"]["web"].as_u64().is_some(), "{parsed}");
    assert_eq!(parsed["services"]["web"]["running"], true, "{parsed}");

    cli.run(&wt, &["down"]).success();
}

#[test]
fn logs_show_what_the_service_printed() {
    let cli = Cli::new();
    let wt = cli.worktree("mon_2695");
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
    let wt = cli.worktree("mon_2695");
    std::fs::remove_file(wt.join(".treeish.toml")).expect("remove config");

    let assert = cli.run(&wt, &["up"]).failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(stderr.contains("treeish --llm"), "{stderr}");
}

#[test]
fn doctor_passes_in_a_worktree_that_is_ready_to_start() {
    let cli = Cli::new();
    let wt = cli.worktree("mon_2695");

    let out = cli
        .run(&wt, &["doctor"])
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out).into_owned();

    assert!(stdout.contains("backend/.env.local"), "{stdout}");
    assert!(stdout.contains("mon_2695"), "{stdout}");
}

/// The failure treeish exists to prevent, reported before anything is started.
#[test]
fn doctor_names_the_env_file_missing_from_the_main_checkout() {
    let cli = Cli::new();
    std::fs::remove_file(cli.fx.main.join("backend/.env.local")).expect("remove");
    let wt = cli.worktree("mon_2695");

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
    let wt = cli.worktree("mon_2695");
    std::fs::remove_file(wt.join(".treeish.toml")).expect("remove config");

    let assert = cli.run(&wt, &["doctor"]).failure();

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&assert.get_output().stdout),
        String::from_utf8_lossy(&assert.get_output().stderr)
    );
    assert!(combined.contains("treeish --llm"), "{combined}");
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
fn ls_lists_every_running_instance() {
    let cli = Cli::new();
    let a = cli.worktree("mon_2694");
    let b = cli.worktree("mon_2695");
    cli.run(&a, &["up"]).success();
    cli.run(&b, &["up"]).success();

    let out = cli.run(&a, &["ls"]).success().get_output().stdout.clone();
    let stdout = String::from_utf8_lossy(&out).into_owned();

    assert!(stdout.contains("mon_2694"), "{stdout}");
    assert!(stdout.contains("mon_2695"), "{stdout}");

    cli.run(&a, &["down"]).success();
    cli.run(&b, &["down"]).success();
}

#[test]
fn run_executes_a_command_with_the_instance_environment() {
    let cli = Cli::new();
    let wt = cli.worktree("mon_2695");
    cli.run(&wt, &["up"]).success();

    let out = cli
        .run(&wt, &["run", "--", "sh", "-c", "echo $TREEISH_PORT_WEB"])
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

    Command::cargo_bin("treeish")
        .expect("binary")
        .current_dir(&cli.fx.main)
        .env("HOME", home.path())
        .args(["skill", "install"])
        .assert()
        .success();

    let body = std::fs::read_to_string(home.path().join(".claude/skills/treeish/SKILL.md"))
        .expect("skill installed to the global skills directory");
    assert!(body.starts_with("---\nname: treeish\n"), "{body}");
    assert!(
        body.contains("description:"),
        "a model-invoked skill needs a description to be discoverable"
    );
    // The schema lives in the binary and is reached by pointer, so it cannot drift.
    assert!(body.contains("treeish --llm"), "{body}");
    assert!(
        !body.contains("[[secrets]]"),
        "the skill must point at the schema rather than restate it"
    );
}
