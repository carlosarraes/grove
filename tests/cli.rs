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

/// A `setup` that leaves a measurable, gitignored tree behind, the way `npm ci` does.
/// Appends rather than truncates so a rerun visibly grows it.
const SETUP_CONFIG: &str = r#"
version = 1

[ports]
names = ["web"]

[[secrets]]
from = "backend/.env.local"
into = "backend/.env.local"

[secrets.set]
API_URL = "http://localhost:{{ port.web }}"

[[service]]
name = "web"
setup = "mkdir -p node_modules && head -c 2097152 /dev/zero >> node_modules/blob"
command = "python3 -u -m http.server {{ port.web }}"
ready = { http = "http://127.0.0.1:{{ port.web }}/", timeout = "30s" }
"#;

/// A `setup` whose output is shared through the store. It logs every real run to a file
/// beside the worktrees, so a test can count installs across several of them.
const CACHE_CONFIG: &str = r#"
version = 1

[ports]
names = ["web"]

[[secrets]]
from = "backend/.env.local"
into = "backend/.env.local"

[secrets.set]
API_URL = "http://localhost:{{ port.web }}"

[[service]]
name = "web"
setup = "mkdir -p node_modules && head -c 1048576 /dev/zero > node_modules/blob && echo installed >> ../installs.log"
command = "python3 -u -m http.server {{ port.web }}"
ready = { http = "http://127.0.0.1:{{ port.web }}/", timeout = "30s" }

[service.cache]
path = "node_modules"
key = ["package-lock.json"]
"#;

const EXPOSURE_CONFIG: &str = r#"
version = 1

[ports]
names = ["web"]

[[secrets]]
from = "backend/.env.local"
into = "backend/.env.local"

[secrets.set]
API_URL = "http://{{ host.public }}:{{ port.web }}"
CORS_ORIGINS = "http://{{ host.public }}:{{ port.web }}"
BIND_HOST = "{{ host.bind }}"

[[service]]
name = "web"
command = "printf '%s' '{{ host.bind }}' > bind-host.txt; exec python3 -u -m http.server {{ port.web }} --bind {{ host.bind }}"
ready = { http = "http://127.0.0.1:{{ port.web }}/", timeout = "30s" }
"#;

// Production uses 20000..30000. Sharing that range made health tests report live
// lane servers as stray listeners. Keep fixture ranges separate, skip occupied
// ranges, and hold a lease across setup and teardown for concurrent test processes.
static NEXT_SLICE: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

fn claim_test_ports() -> (std::ops::Range<u16>, std::fs::File) {
    let locks = std::env::temp_dir().join("grove-cli-test-ports");
    std::fs::create_dir_all(&locks).expect("port lease directory");
    for _ in 0..190 {
        let slice = NEXT_SLICE.fetch_add(1, std::sync::atomic::Ordering::SeqCst) % 190;
        let low = 30000 + slice * 100;
        let range = low..low + 100;
        let lease = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(locks.join(low.to_string()))
            .expect("port lease");
        match lease.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => continue,
            Err(error) => panic!("port lease: {error}"),
        }
        let listeners: std::io::Result<Vec<_>> = range
            .clone()
            .map(|port| TcpListener::bind(("0.0.0.0", port)))
            .collect();
        if listeners.is_ok() {
            return (range, lease);
        }
    }
    panic!("no unoccupied CLI test range in 30000..49000");
}

struct Cli {
    state: TempDir,
    ports: std::ops::Range<u16>,
    _port_lease: std::fs::File,
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
        std::fs::write(fx.main.join(".gitignore"), ".env.local\nnode_modules/\n")
            .expect("gitignore");
        common::git(&fx.main, &["add", "."]);
        // A developer may globally ignore `.grove.toml` while keeping repo configs
        // explicitly tracked. This fixture requires the latter regardless of ambient
        // Git configuration.
        common::git(&fx.main, &["add", "--force", ".grove.toml"]);
        common::git(&fx.main, &["commit", "-m", "add grove config"]);

        let (ports, port_lease) = claim_test_ports();
        Cli {
            state: TempDir::new().expect("tempdir"),
            ports,
            _port_lease: port_lease,
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

    fn port_range(&self) -> String {
        format!("{}-{}", self.ports.start, self.ports.end)
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
        let mut command = Command::cargo_bin("grove").expect("binary");
        command
            .current_dir(cwd)
            .env("GROVE_STATE_DIR", self.state.path())
            .env("GROVE_CACHE_DIR", self.state.path().join("store"))
            .env("GROVE_PORT_RANGE", self.port_range())
            .env_remove("GROVE_MACOS_CLONE");
        if let Some(docker) = &self.docker {
            command
                .env("GROVE_DOCKER", &docker.program)
                .env("GROVE_DOCKER_INSPECT", &docker.inspect)
                .env("GROVE_DOCKER_LOGS", self.state.path().join("docker.log"));
        }
        command.args(args).assert()
    }
}

fn service_pid(cli: &Cli, worktree: &Path, service: &str) -> u64 {
    let output = cli
        .run(worktree, &["status", "--json"])
        .success()
        .get_output()
        .stdout
        .clone();
    let status: serde_json::Value = serde_json::from_slice(&output).expect("status JSON");
    status["services"][service]["pid"]
        .as_u64()
        .expect("running service pid")
}

#[test]
fn exposure_changes_restart_and_rerender_once_and_plain_up_returns_to_local() {
    let cli = Cli::with_config(EXPOSURE_CONFIG);
    let wt = cli.worktree("feat_search");

    cli.run(&wt, &["up"]).success();
    let local_pid = service_pid(&cli, &wt, "web");
    let local_env = std::fs::read_to_string(wt.join("backend/.env.local")).expect("local env");
    assert!(
        local_env.contains("API_URL=http://localhost:"),
        "{local_env}"
    );
    assert!(local_env.contains("BIND_HOST=127.0.0.1"), "{local_env}");
    assert_eq!(
        std::fs::read_to_string(wt.join("bind-host.txt")).expect("bind host"),
        "127.0.0.1"
    );

    let exposed = cli
        .run(&wt, &["up", "--expose-host", "dev-mac.local"])
        .success();
    let exposed_stdout = String::from_utf8_lossy(&exposed.get_output().stdout);
    let exposed_stderr = String::from_utf8_lossy(&exposed.get_output().stderr);
    assert!(
        exposed_stdout.contains("http://dev-mac.local:"),
        "{exposed_stdout}"
    );
    assert!(exposed_stderr.contains("warning"), "{exposed_stderr}");
    assert!(exposed_stderr.contains("dev-mac.local"), "{exposed_stderr}");

    let exposed_pid = service_pid(&cli, &wt, "web");
    assert_ne!(
        exposed_pid, local_pid,
        "changing exposure must restart services"
    );
    let exposed_env = std::fs::read_to_string(wt.join("backend/.env.local")).expect("exposed env");
    assert!(
        exposed_env.contains("API_URL=http://dev-mac.local:"),
        "{exposed_env}"
    );
    assert!(exposed_env.contains("BIND_HOST=0.0.0.0"), "{exposed_env}");
    assert_eq!(
        std::fs::read_to_string(wt.join("bind-host.txt")).expect("bind host"),
        "0.0.0.0"
    );

    cli.run(&wt, &["up", "--expose-host", "dev-mac.local"])
        .success();
    assert_eq!(
        service_pid(&cli, &wt, "web"),
        exposed_pid,
        "repeating the same exposure must remain idempotent"
    );

    cli.run(&wt, &["up", "--expose-host", "dev-mac-2.local"])
        .success();
    let changed_host_pid = service_pid(&cli, &wt, "web");
    assert_ne!(
        changed_host_pid, exposed_pid,
        "changing only the public host must restart services"
    );
    let changed_host_env =
        std::fs::read_to_string(wt.join("backend/.env.local")).expect("changed host env");
    assert!(
        changed_host_env.contains("API_URL=http://dev-mac-2.local:"),
        "{changed_host_env}"
    );

    cli.run(&wt, &["up"]).success();
    assert_ne!(
        service_pid(&cli, &wt, "web"),
        changed_host_pid,
        "plain up is the transition back to loopback"
    );
    let local_again =
        std::fs::read_to_string(wt.join("backend/.env.local")).expect("local env again");
    assert!(
        local_again.contains("API_URL=http://localhost:"),
        "{local_again}"
    );
    assert!(local_again.contains("BIND_HOST=127.0.0.1"), "{local_again}");

    cli.run(&wt, &["down"]).success();
}

#[test]
fn invalid_exposure_host_changes_neither_files_state_nor_processes() {
    let cli = Cli::with_config(EXPOSURE_CONFIG);
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();
    let pid = service_pid(&cli, &wt, "web");
    let env = std::fs::read_to_string(wt.join("backend/.env.local")).expect("env");

    let failed = cli
        .run(&wt, &["up", "--expose-host", "http://dev-mac.local"])
        .failure();
    let stderr = String::from_utf8_lossy(&failed.get_output().stderr);
    assert!(stderr.contains("http://dev-mac.local"), "{stderr}");

    assert_eq!(service_pid(&cli, &wt, "web"), pid);
    assert_eq!(
        std::fs::read_to_string(wt.join("backend/.env.local")).expect("env after failure"),
        env
    );
    let status = cli.run(&wt, &["status", "--json"]).success();
    let parsed: serde_json::Value =
        serde_json::from_slice(&status.get_output().stdout).expect("status JSON");
    assert_eq!(parsed["exposed"], false, "{parsed}");

    cli.run(&wt, &["down"]).success();
}

#[test]
fn a_failed_exposed_start_retains_the_rendered_target_without_old_processes() {
    let config = EXPOSURE_CONFIG.replace(
        "command = \"printf",
        "prepare = \"test '{{ host.bind }}' != '0.0.0.0'\"\ncommand = \"printf",
    );
    let cli = Cli::with_config(&config);
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();

    cli.run(&wt, &["up", "--expose-host", "dev-mac.local"])
        .failure();

    let env = std::fs::read_to_string(wt.join("backend/.env.local")).expect("target env");
    assert!(env.contains("API_URL=http://dev-mac.local:"), "{env}");
    assert!(env.contains("BIND_HOST=0.0.0.0"), "{env}");
    let status = cli.run(&wt, &["status", "--json"]).success();
    let parsed: serde_json::Value =
        serde_json::from_slice(&status.get_output().stdout).expect("status JSON");
    assert_eq!(parsed["exposed"], true, "{parsed}");
    assert_eq!(parsed["public_host"], "dev-mac.local", "{parsed}");
    assert_eq!(
        parsed["services"]["web"]["running"], false,
        "the old local process must be stopped before exposed prepare runs: {parsed}"
    );
}

#[test]
fn restart_and_run_reuse_the_persisted_exposure() {
    let cli = Cli::with_config(EXPOSURE_CONFIG);
    let wt = cli.worktree("remote_demo");
    cli.run(&wt, &["up", "--expose-host", "dev-mac.local"])
        .success();
    let before = service_pid(&cli, &wt, "web");

    cli.run(
        &wt,
        &[
            "run",
            "--",
            "sh",
            "-c",
            "printf '%s|%s' \"$API_URL\" \"$BIND_HOST\" > run-exposure.txt",
        ],
    )
    .success();
    let run_env = std::fs::read_to_string(wt.join("run-exposure.txt")).expect("run env");
    assert!(run_env.contains("http://dev-mac.local:"), "{run_env}");
    assert!(run_env.ends_with("|0.0.0.0"), "{run_env}");

    let restarted = cli.run(&wt, &["restart", "web"]).success();
    let stdout = String::from_utf8_lossy(&restarted.get_output().stdout);
    assert!(stdout.contains("http://dev-mac.local:"), "{stdout}");
    assert_ne!(service_pid(&cli, &wt, "web"), before);
    assert_eq!(
        std::fs::read_to_string(wt.join("bind-host.txt")).expect("bind host"),
        "0.0.0.0"
    );

    cli.run(&wt, &["down"]).success();
}

#[test]
fn status_reports_the_persisted_exposure_in_text_and_json() {
    let cli = Cli::with_config(EXPOSURE_CONFIG);
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up", "--expose-host", "dev-mac.local"])
        .success();

    let text = cli.run(&wt, &["status"]).success();
    let stdout = String::from_utf8_lossy(&text.get_output().stdout);
    assert!(stdout.contains("http://dev-mac.local:"), "{stdout}");
    assert!(stdout.contains("exposure"), "{stdout}");
    assert!(stdout.contains("dev-mac.local"), "{stdout}");

    let json = cli.run(&wt, &["status", "--json"]).success();
    let parsed: serde_json::Value =
        serde_json::from_slice(&json.get_output().stdout).expect("status JSON");
    assert_eq!(parsed["exposed"], true, "{parsed}");
    assert_eq!(parsed["public_host"], "dev-mac.local", "{parsed}");

    cli.run(&wt, &["down"]).success();
    let stopped = cli.run(&wt, &["status", "--json"]).success();
    let parsed: serde_json::Value =
        serde_json::from_slice(&stopped.get_output().stdout).expect("stopped status JSON");
    assert_eq!(
        parsed["exposed"], true,
        "down must preserve the mode: {parsed}"
    );
    assert_eq!(parsed["public_host"], "dev-mac.local", "{parsed}");
}

#[test]
fn ls_distinguishes_an_exposed_instance_without_touching_its_local_sibling() {
    let cli = Cli::with_config(EXPOSURE_CONFIG);
    let local = cli.worktree("local_ticket");
    let exposed = cli.worktree("remote_demo");
    cli.run(&local, &["up"]).success();
    let local_pid = service_pid(&cli, &local, "web");
    cli.run(&exposed, &["up", "--expose-host", "dev-mac.local"])
        .success();

    assert_eq!(
        service_pid(&cli, &local, "web"),
        local_pid,
        "exposing one instance must not restart its sibling"
    );

    let text = cli.run(&local, &["ls"]).success();
    let stdout = String::from_utf8_lossy(&text.get_output().stdout);
    let exposed_row = stdout
        .lines()
        .find(|line| line.contains("remote_demo"))
        .expect("exposed row");
    let local_row = stdout
        .lines()
        .find(|line| line.contains("local_ticket"))
        .expect("local row");
    assert!(
        exposed_row.contains("exposed dev-mac.local"),
        "{exposed_row}"
    );
    assert!(!local_row.contains("exposed"), "{local_row}");

    let json = cli.run(&local, &["ls", "--json"]).success();
    let parsed: serde_json::Value =
        serde_json::from_slice(&json.get_output().stdout).expect("ls JSON");
    let instances = parsed["instances"].as_array().expect("instances");
    let by_slug = |slug: &str| {
        instances
            .iter()
            .find(|instance| instance["slug"] == slug)
            .unwrap_or_else(|| panic!("missing {slug}: {parsed}"))
    };
    assert_eq!(by_slug("remote_demo")["exposed"], true);
    assert_eq!(by_slug("remote_demo")["public_host"], "dev-mac.local");
    assert_eq!(by_slug("local_ticket")["exposed"], false);
    assert_eq!(by_slug("local_ticket")["public_host"], "localhost");

    cli.run(&local, &["down"]).success();
    cli.run(&exposed, &["down"]).success();
}

#[test]
fn doctor_warns_that_an_exposed_instance_crosses_the_loopback_boundary() {
    let cli = Cli::with_config(EXPOSURE_CONFIG);
    let wt = cli.worktree("remote_demo");
    cli.run(&wt, &["up", "--expose-host", "dev-mac.local"])
        .success();

    let output = cli.run(&wt, &["doctor"]).success();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout);

    assert!(stdout.contains("warn"), "{stdout}");
    assert!(stdout.contains("dev-mac.local"), "{stdout}");
    assert!(stdout.contains("authentication bypass"), "{stdout}");
    assert!(stdout.contains("other machines"), "{stdout}");

    cli.run(&wt, &["down"]).success();
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
    assert!(stdout.contains("environment overlaid"), "{stdout}");
    assert!(stdout.contains("resource recreation"), "{stdout}");
}

#[test]
fn up_help_names_network_exposure_and_its_host_override() {
    let out = Command::cargo_bin("grove")
        .expect("binary")
        .args(["up", "--help"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out);

    assert!(stdout.contains("--expose"), "{stdout}");
    assert!(stdout.contains("--expose-host <HOST>"), "{stdout}");
    assert!(stdout.contains("local network"), "{stdout}");
    assert!(stdout.contains("default-route"), "{stdout}");
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
    // Unconfigured means nowhere: a main checkout that still has one is the case the
    // fallback covers, not this one.
    std::fs::remove_file(wt.join(".grove.toml")).expect("remove config");
    std::fs::remove_file(cli.fx.main.join(".grove.toml")).expect("remove config");

    let assert = cli.run(&wt, &["up"]).failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(stderr.contains("grove --llm"), "{stderr}");
    assert!(
        stderr.contains(&format!("no .grove.toml in {}", wt.display())),
        "must name the worktree the agent is standing in: {stderr}"
    );
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
    std::fs::remove_file(cli.fx.main.join(".grove.toml")).expect("remove config");

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
    // Binding port zero allocates a nonzero port, so no listener can own this
    // destination. An ephemeral port released here can be reused by another test.
    let port = 0;
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
    assert!(body.contains("dotenv"), "{body}");
    assert!(body.contains("disable dotenv loading"), "{body}");
    assert!(body.contains("process variables"), "{body}");
    assert!(body.contains("container incarnation"), "{body}");
    // `down` reads as cleanup and reclaims no disk; the skill has to say what it keeps
    // and what frees it, or an agent following it believes the machine is clean.
    assert!(body.contains("keeps"), "{body}");
    assert!(body.contains("git worktree remove"), "{body}");
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
            .env("GROVE_CACHE_DIR", self.state.path().join("store"))
            .env("GROVE_PORT_RANGE", self.port_range())
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

/// A service whose readiness URL points somewhere nothing serves. The process runs
/// happily; the endpoint is dead.
const WEDGED_CONFIG: &str = r#"
version = 1

[ports]
names = ["web", "unserved"]

[[secrets]]
from = "backend/.env.local"
into = "backend/.env.local"

[secrets.set]
API_URL = "http://localhost:{{ port.web }}"

[[service]]
name = "web"
command = "python3 -u -m http.server {{ port.web }}"
ready = { http = "http://127.0.0.1:{{ port.web }}/", timeout = "30s" }

# Runs forever and binds nothing. Its readiness URL points at a port no one serves, which
# is what a backend looks like once the datastore it depends on has died.
[[service]]
name = "wedged"
command = "sleep 600"
ready = { http = "http://127.0.0.1:{{ port.unserved }}/", timeout = "1s" }
"#;

/// The misdiagnosis this exists to stop. `src/llm.rs` tells agents "Grove owns application
/// readiness — run `grove status` first", and on a healthy answer sends them off to
/// `agent-browser doctor --fix`. A backend whose process is alive but whose HTTP is dead —
/// what happens when the shared datastore dies mid-suite — must not read as healthy, or
/// grove has scripted an hour of looking in the wrong place.
#[test]
fn status_separates_a_live_process_from_a_dead_endpoint() {
    let cli = Cli::with_config(WEDGED_CONFIG);
    let wt = cli.worktree("feat_search");

    // `up` fails: the wedged service never answers. That is correct, and the instance is
    // left exactly in the state this test is about — process alive, endpoint dead.
    cli.run(&wt, &["up"]).failure();

    let out = cli
        .run(&wt, &["status"])
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out).into_owned();

    // Scoped to the services block: port names and service names are separate namespaces
    // and both are called `web` here, which is exactly why the block is headed.
    let block = stdout
        .split_once("\nservices\n")
        .unwrap_or_else(|| panic!("status must head its service rows: {stdout}"))
        .1;
    let row = |name: &str| -> &str {
        block
            .lines()
            .find(|l| l.trim_start().starts_with(name))
            .unwrap_or_else(|| panic!("status must list {name}: {stdout}"))
    };

    let wedged = row("wedged");
    assert!(
        wedged.contains("running"),
        "the process really is alive; saying otherwise is the opposite error: {wedged}"
    );
    assert!(
        wedged.to_lowercase().contains("not answering"),
        "an alive process with a dead endpoint must not read as healthy: {wedged}"
    );

    let healthy = row("web");
    assert!(healthy.contains("answering"), "{healthy}");
    assert!(
        !healthy.to_lowercase().contains("not answering"),
        "{healthy}"
    );

    cli.run(&wt, &["down"]).success();
}

/// An unasked question is not a passed check. A service with no `ready.http` gives grove
/// nothing to probe, and reporting that as answering would launder ignorance into a
/// health claim — the exact move this whole change exists to undo.
#[test]
fn a_service_with_no_readiness_probe_is_never_called_answering() {
    let cli = Cli::with_config(
        r#"
version = 1

[ports]
names = ["web"]

[[secrets]]
from = "backend/.env.local"
into = "backend/.env.local"

[[service]]
name = "web"
command = "python3 -u -m http.server {{ port.web }}"
"#,
    );
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();

    let parsed: serde_json::Value = serde_json::from_slice(
        &cli.run(&wt, &["status", "--json"])
            .success()
            .get_output()
            .stdout,
    )
    .expect("json");

    assert_eq!(parsed["services"]["web"]["running"], true);
    assert_eq!(parsed["services"]["web"]["ready"], "undeclared");
    assert_eq!(parsed["services"]["web"]["url"], serde_json::Value::Null);

    cli.run(&wt, &["down"]).success();
}

/// `status --json` is what agents branch on, so the new keys have to be there and the
/// old ones have to be untouched.
#[test]
fn status_json_reports_readiness_beside_the_pid() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();

    let parsed: serde_json::Value = serde_json::from_slice(
        &cli.run(&wt, &["status", "--json"])
            .success()
            .get_output()
            .stdout,
    )
    .expect("json");

    let web = &parsed["services"]["web"];
    assert_eq!(web["running"], true);
    assert_eq!(web["ready"], "answering");
    assert!(
        web["url"].as_str().expect("url").starts_with("http://"),
        "a reader told 'not answering' needs the URL to try: {parsed}"
    );
    assert!(web["pid"].as_u64().is_some(), "{parsed}");
    assert_eq!(parsed["slug"], "feat_search", "existing keys must not move");

    cli.run(&wt, &["down"]).success();
}

/// A stopped service serves nothing by definition. Probing it spends the timeout to learn
/// what the pid already said, and `status` has to stay a command you run without thinking.
#[test]
fn a_stopped_service_is_reported_silent_without_being_probed() {
    let cli = Cli::with_config(WEDGED_CONFIG);
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).failure();
    cli.run(&wt, &["down"]).success();

    let began = std::time::Instant::now();
    let parsed: serde_json::Value = serde_json::from_slice(
        &cli.run(&wt, &["status", "--json"])
            .success()
            .get_output()
            .stdout,
    )
    .expect("json");
    let elapsed = began.elapsed();

    assert_eq!(parsed["services"]["wedged"]["running"], false);
    assert_eq!(parsed["services"]["wedged"]["ready"], "silent");
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "status on a stopped instance must not wait on probes it can answer from the pid: {elapsed:?}"
    );
}

/// The decided semantics, and the one a naive implementation gets wrong. `setup` and
/// `[[seed]]` already cover "once per worktree"; running every time is the entire reason
/// `prepare` exists. A generator that skipped a live service would leave the common loop
/// — edit the backend, `grove up` — serving stale generated code, which is the bug it was
/// added to prevent.
#[test]
fn prepare_runs_on_every_up_including_when_the_service_is_already_alive() {
    let cli = Cli::with_config(
        r#"
version = 1

[ports]
names = ["web"]

[[secrets]]
from = "backend/.env.local"
into = "backend/.env.local"

[[service]]
name = "web"
prepare = "printf 'generated\n' >> ../prepared.log"
command = "python3 -u -m http.server {{ port.web }}"
ready = { http = "http://127.0.0.1:{{ port.web }}/", timeout = "30s" }
"#,
    );
    let wt = cli.worktree("feat_search");
    let ran = wt.parent().expect("worktrees dir").join("prepared.log");

    cli.run(&wt, &["up"]).success();
    assert_eq!(
        std::fs::read_to_string(&ran)
            .expect("prepare ran")
            .lines()
            .count(),
        1
    );

    // The service is still up. `up` starts nothing — and must still prepare.
    cli.run(&wt, &["up"]).success();
    assert_eq!(
        std::fs::read_to_string(&ran)
            .expect("prepare log")
            .lines()
            .count(),
        2,
        "prepare skipped a running service, so generated code would now be stale"
    );

    cli.run(&wt, &["down"]).success();
}

/// Generated code is worthless if it was generated from nothing. A frontend's generator
/// reads its own worktree's backend, so `prepare` has to run after the services declared
/// before it are answering — not merely after they were spawned.
#[test]
fn prepare_runs_after_earlier_services_are_answering() {
    let cli = Cli::with_config(
        r#"
version = 1

[ports]
names = ["backend", "frontend"]

[[secrets]]
from = "backend/.env.local"
into = "backend/.env.local"

[[service]]
name = "backend"
command = "python3 -u -m http.server {{ port.backend }}"
ready = { http = "http://127.0.0.1:{{ port.backend }}/", timeout = "30s" }

# Fails outright unless the backend above is already serving.
[[service]]
name = "frontend"
prepare = "python3 -c \"import urllib.request,sys; urllib.request.urlopen('http://127.0.0.1:{{ port.backend }}/')\""
command = "python3 -u -m http.server {{ port.frontend }}"
ready = { http = "http://127.0.0.1:{{ port.frontend }}/", timeout = "30s" }
"#,
    );
    let wt = cli.worktree("feat_search");

    cli.run(&wt, &["up"]).success();

    cli.run(&wt, &["down"]).success();
}

/// A generator that failed produced nothing, or worse produced half a file. Starting the
/// service on top of that hides the cause behind whatever breaks next.
#[test]
fn a_failing_prepare_stops_up_and_shows_what_it_printed() {
    let cli = Cli::with_config(
        r#"
version = 1

[ports]
names = ["web"]

[[secrets]]
from = "backend/.env.local"
into = "backend/.env.local"

[[service]]
name = "web"
prepare = "echo 'contracts generator exploded' >&2; exit 1"
command = "python3 -u -m http.server {{ port.web }}"
ready = { http = "http://127.0.0.1:{{ port.web }}/", timeout = "30s" }
"#,
    );
    let wt = cli.worktree("feat_search");

    let out = cli.run(&wt, &["up"]).failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();

    assert!(stderr.contains("prepare"), "{stderr}");
    assert!(
        stderr.contains("contracts generator exploded"),
        "the failure has to carry what the generator said, or it costs a round trip: {stderr}"
    );

    let parsed: serde_json::Value = serde_json::from_slice(
        &cli.run(&wt, &["status", "--json"])
            .success()
            .get_output()
            .stdout,
    )
    .expect("json");
    assert_eq!(
        parsed["services"]["web"]["running"], false,
        "the service must not start on top of a failed generator"
    );
}

/// `prune` reaches instances whose worktree — and therefore whose `.grove.toml` — is
/// gone, so the registry has to have recorded where the database lived while it still
/// could.
#[test]
fn an_instance_records_where_its_database_lives() {
    // A held listener makes the port answer, so `ensure` reuses it and the test needs no
    // Docker — the same path a developer running their own datastore takes.
    let datastore =
        std::net::TcpListener::bind("127.0.0.1:0").expect("a stand-in for the datastore");
    let port = datastore.local_addr().expect("addr").port();

    let cli = Cli::with_config(&CONFIG.replace(
        "[[service]]",
        &format!(
            r#"[[resource]]
name = "store"
kind = "docker-shared"
port = {port}
db_name = "app_{{{{ slug }}}}"

[[service]]"#
        ),
    ));
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();

    let entry = registry_of(&cli)
        .get(&wt)
        .expect("get")
        .expect("reserved on open");
    assert_eq!(entry.db_name.as_deref(), Some("app_feat_search"));
    let resource = entry
        .db_resource
        .expect("the database's whereabouts must be recorded while the worktree exists");
    assert_eq!(resource.name, "store");
    assert_eq!(resource.port, port);

    cli.run(&wt, &["down"]).success();
}

/// The blast radius is databases, and the worktrees they belonged to are already gone —
/// so naming them before dropping anything is the whole safety story.
#[test]
fn prune_dry_run_names_orphan_databases_and_reclaims_nothing() {
    let cli = Cli::new();
    let doomed = cli.worktree("deleted_later");
    cli.run(&doomed, &["up"]).success();
    cli.run(&doomed, &["down"]).success();

    // Give it a database the config never declared, so this test does not need a datastore.
    let registry = registry_of(&cli);
    let mut entry = registry.get(&doomed).expect("get").expect("entry");
    entry.db_name = Some("app_deleted_later".to_string());
    entry.db_resource = Some(grove::registry::DbResource {
        name: "store".to_string(),
        port: 1,
    });
    registry.record(&entry).expect("record");
    std::fs::remove_dir_all(&doomed).expect("delete the worktree");

    let out = cli
        .run(&cli.fx.main, &["prune", "--dry-run"])
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out).into_owned();

    assert!(stdout.contains("deleted_later"), "{stdout}");
    assert!(stdout.contains("app_deleted_later"), "{stdout}");
    assert!(
        registry.get(&doomed).expect("get").is_some(),
        "a dry run that forgot the instance is not a dry run: {stdout}"
    );
}

/// Entries written before grove recorded the datastore's whereabouts cannot be reached —
/// their worktree is gone, so there is nothing left to read the port from. Saying so, and
/// handing over the command that does work, beats implying it was handled.
#[test]
fn prune_admits_which_databases_it_cannot_reach() {
    let cli = Cli::new();
    let doomed = cli.worktree("from_an_older_grove");
    cli.run(&doomed, &["up"]).success();
    cli.run(&doomed, &["down"]).success();

    let registry = registry_of(&cli);
    let mut entry = registry.get(&doomed).expect("get").expect("entry");
    entry.db_name = Some("app_from_an_older_grove".to_string());
    entry.db_resource = None; // as a v0.1.12 registry has it
    registry.record(&entry).expect("record");
    std::fs::remove_dir_all(&doomed).expect("delete the worktree");

    let out = cli
        .run(&cli.fx.main, &["prune", "--purge"])
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out).into_owned();

    assert!(stdout.contains("app_from_an_older_grove"), "{stdout}");
    assert!(
        stdout.contains("mongosh"),
        "an unreachable database should hand over the command that works: {stdout}"
    );
}

fn disk_bytes_of(cli: &Cli, worktree: &Path) -> Option<u64> {
    registry_of(cli)
        .get(worktree)
        .expect("get")
        .expect("entry")
        .disk_bytes
}

/// The dependency tree is the cost `down` never returns, and load says nothing about it.
/// `ls` is where someone deciding what to reclaim is already looking.
#[test]
fn up_records_what_setup_put_on_disk_and_ls_shows_it() {
    let cli = Cli::with_config(SETUP_CONFIG);
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();

    let bytes = disk_bytes_of(&cli, &wt).expect("measured once setup has run");
    assert!(bytes >= 2 << 20, "setup wrote 2MiB, recorded {bytes}");

    let out = cli.run(&wt, &["ls"]).success().get_output().stdout.clone();
    let listed = String::from_utf8_lossy(&out).into_owned();
    assert!(listed.contains("2.0M"), "{listed}");
    assert!(listed.contains("on disk"), "{listed}");

    let out = cli
        .run(&wt, &["ls", "--json"])
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&out).expect("json");
    assert_eq!(json["instances"][0]["disk_bytes"].as_u64(), Some(bytes));
    assert_eq!(json["disk_bytes"].as_u64(), Some(bytes));

    cli.run(&wt, &["down"]).success();
}

/// Walking node_modules costs seconds per worktree; on a fleet it is only affordable at
/// the one moment the number changes, which is when setup runs.
#[test]
fn an_ordinary_up_does_not_walk_node_modules_again() {
    let cli = Cli::with_config(SETUP_CONFIG);
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();
    let first = disk_bytes_of(&cli, &wt).expect("measured");

    let mut blob = std::fs::OpenOptions::new()
        .append(true)
        .open(wt.join("node_modules/blob"))
        .expect("blob");
    std::io::Write::write_all(&mut blob, &vec![0u8; 2 << 20]).expect("grow");
    drop(blob);
    cli.run(&wt, &["up"]).success();
    assert_eq!(
        disk_bytes_of(&cli, &wt),
        Some(first),
        "an ordinary up re-measured"
    );

    let dir = registry_of(&cli)
        .get(&wt)
        .expect("get")
        .expect("entry")
        .instance_dir
        .expect("instance dir");
    std::fs::remove_file(dir.join(".setup-web")).expect("forget that setup ran");
    cli.run(&wt, &["up"]).success();
    let again = disk_bytes_of(&cli, &wt).expect("measured");
    assert!(
        again > first,
        "setup reran but the figure stayed at {first}"
    );

    cli.run(&wt, &["down"]).success();
}

/// An orphan's directory is already gone, so its stored figure describes freed blocks.
/// Showing it would report disk the machine has already got back.
#[test]
fn ls_shows_no_size_for_an_orphan() {
    let cli = Cli::with_config(SETUP_CONFIG);
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();
    cli.run(&wt, &["down"]).success();
    std::fs::remove_dir_all(&wt).expect("delete the worktree");

    let out = cli
        .run(&cli.fx.main, &["ls"])
        .success()
        .get_output()
        .stdout
        .clone();
    let listed = String::from_utf8_lossy(&out).into_owned();
    assert!(listed.contains("orphaned"), "{listed}");
    assert!(!listed.contains("2.0M"), "{listed}");
    assert!(!listed.contains("on disk"), "{listed}");
}

/// `down` reclaims CPU and, without this line, says nothing about the gigabyte it left
/// behind — so a reader who never learns disk is separate never learns what frees it.
#[test]
fn down_says_what_it_kept_and_how_to_free_it() {
    let cli = Cli::with_config(SETUP_CONFIG);
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();

    let out = cli
        .run(&wt, &["down"])
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out).into_owned();
    assert!(stdout.contains("stopped feat_search"), "{stdout}");
    assert!(stdout.contains("kept"), "{stdout}");
    assert!(stdout.contains("2.0M on disk"), "{stdout}");
    assert!(
        stdout.contains(&format!("git worktree remove {}", wt.display())),
        "{stdout}"
    );
}

#[test]
fn down_without_a_measured_size_still_names_the_disk() {
    let cli = Cli::with_config(SETUP_CONFIG);
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();
    // As an instance whose setup ran under a grove that did not measure has it.
    let registry = registry_of(&cli);
    let mut entry = registry.get(&wt).expect("get").expect("entry");
    entry.disk_bytes = None;
    registry.record(&entry).expect("record");

    let out = cli
        .run(&wt, &["down"])
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out).into_owned();
    assert!(stdout.contains("dependencies on disk"), "{stdout}");
    assert!(stdout.contains("git worktree remove"), "{stdout}");
}

/// Nothing grove put there, nothing grove should claim to have kept.
#[test]
fn down_says_nothing_about_disk_when_no_setup_ran() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();

    let out = cli
        .run(&wt, &["down"])
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out).into_owned();
    assert!(!stdout.contains("kept"), "{stdout}");
    assert!(!stdout.contains("on disk"), "{stdout}");
}

/// A sweep is the fleet case: whoever runs it is the one deciding whether the machine
/// has room, and "stopped, ports kept" alone reads as if it made some.
#[test]
fn a_sweep_says_how_much_disk_it_left_behind() {
    let cli = Cli::with_config(SETUP_CONFIG);
    let here = cli.worktree("feat_search");
    let stale = cli.worktree("fix_login");
    cli.run(&here, &["up"]).success();
    cli.run(&stale, &["up"]).success();
    backdate(&cli, &stale, 3 * 3600);

    let out = cli
        .run(&here, &["down", "--idle", "2h"])
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out).into_owned();
    assert!(stdout.contains("stopped fix_login"), "{stdout}");
    assert!(stdout.contains("2.0M still on disk"), "{stdout}");

    cli.run(&here, &["down"]).success();
}

/// Eighteen orphans meant eighteen "left in place" lines and nothing that said
/// eighteen. The count goes in one place; the names stay, because a number nobody
/// named is a number nobody audits.
#[test]
fn prune_counts_the_databases_it_left_and_names_each_one() {
    let cli = Cli::new();
    let registry = registry_of(&cli);
    for slug in ["fix_login", "feat_search"] {
        let wt = cli.worktree(slug);
        cli.run(&wt, &["up"]).success();
        cli.run(&wt, &["down"]).success();
        let mut entry = registry.get(&wt).expect("get").expect("entry");
        entry.db_name = Some(format!("app_{slug}"));
        entry.db_resource = Some(grove::registry::DbResource {
            name: "mongo".into(),
            port: 27017,
        });
        registry.record(&entry).expect("record");
        std::fs::remove_dir_all(&wt).expect("delete the worktree");
    }

    let out = cli
        .run(&cli.fx.main, &["prune"])
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out).into_owned();
    assert!(stdout.contains("app_fix_login"), "{stdout}");
    assert!(stdout.contains("app_feat_search"), "{stdout}");
    assert!(
        stdout.contains("2 databases left in place (--purge drops them)"),
        "{stdout}"
    );
    assert_eq!(stdout.matches("left in place").count(), 1, "{stdout}");
}

#[test]
fn prune_says_it_for_one_database() {
    let cli = Cli::new();
    let wt = cli.worktree("fix_login");
    cli.run(&wt, &["up"]).success();
    cli.run(&wt, &["down"]).success();
    let registry = registry_of(&cli);
    let mut entry = registry.get(&wt).expect("get").expect("entry");
    entry.db_name = Some("app_fix_login".into());
    entry.db_resource = Some(grove::registry::DbResource {
        name: "mongo".into(),
        port: 27017,
    });
    registry.record(&entry).expect("record");
    std::fs::remove_dir_all(&wt).expect("delete the worktree");

    let out = cli
        .run(&cli.fx.main, &["prune"])
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out).into_owned();
    assert!(stdout.contains("app_fix_login"), "{stdout}");
    assert!(
        stdout.contains("1 database left in place (--purge drops it)"),
        "{stdout}"
    );
}

/// A repo that keeps `.grove.toml` out of git leaves every new worktree without one, and
/// copying seventy-five lines by hand into each is the setup step grove exists to remove.
#[test]
fn a_worktree_without_a_config_uses_the_main_checkouts() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    std::fs::remove_file(wt.join(".grove.toml")).expect("leave the worktree without a config");

    let out = cli.run(&wt, &["up"]).success().get_output().stderr.clone();
    let stderr = String::from_utf8_lossy(&out).into_owned();
    assert!(stderr.contains("main checkout"), "{stderr}");
    assert!(stderr.contains(cli.fx.main.to_str().unwrap()), "{stderr}");

    cli.run(&wt, &["down"]).success();
}

/// Kills a stand-in server when the test ends, however it ends.
struct Squatter(std::process::Child);

impl Drop for Squatter {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Something else is already serving this port when the service starts — a reservation
/// is stable across restarts, and nothing re-checks it. The service would die on bind,
/// the readiness probe would be answered by the squatter, and `up` would report success,
/// after which every request goes to whatever took the port, silently. That is the worst
/// outcome grove has, and it has to be refused before anything starts.
#[test]
fn up_refuses_when_something_else_already_answers_on_the_port() {
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

    let _squatter = Squatter(
        std::process::Command::new("python3")
            .args(["-u", "-m", "http.server", &port.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("squat the port"),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while ureq::get(format!("http://127.0.0.1:{port}/"))
        .call()
        .is_err()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the squatter never answered"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let out = cli.run(&wt, &["up"]).failure().get_output().stderr.clone();
    let stderr = String::from_utf8_lossy(&out).into_owned();
    assert!(stderr.contains("already in use"), "{stderr}");
    assert!(
        stderr.contains(&port.to_string()),
        "must name the port: {stderr}"
    );
}

#[test]
fn up_allocates_inside_the_configured_port_range() {
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
    assert!(
        cli.ports.contains(&port),
        "{port} is outside {:?}",
        cli.ports
    );
    cli.run(&wt, &["down"]).success();
}

/// Without a readiness URL there is no answer to probe, so the port check has to come
/// from the reservation itself: when none of this instance's services is running,
/// nothing should be listening on any of its ports.
#[test]
fn up_refuses_when_a_reserved_port_is_taken_and_nothing_of_its_own_is_running() {
    let config = CONFIG.replace(
        "ready = { http = \"http://127.0.0.1:{{ port.web }}/\", timeout = \"30s\" }\n",
        "",
    );
    assert!(
        !config.contains("ready"),
        "the service must declare no readiness check"
    );
    let cli = Cli::with_config(&config);
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

    // Any listener, not only an HTTP one: a bind test is what tells.
    let _squatter = TcpListener::bind(("0.0.0.0", port)).expect("squat the port");

    let out = cli.run(&wt, &["up"]).failure().get_output().stderr.clone();
    let stderr = String::from_utf8_lossy(&out).into_owned();
    assert!(stderr.contains("already in use"), "{stderr}");
    assert!(
        stderr.contains(&port.to_string()),
        "must name the port: {stderr}"
    );
}

/// With one of its own services still running, the instance's ports cannot be told
/// apart by a bind test, so the readiness URL is what catches a squatter on the other.
#[test]
fn up_refuses_a_squatted_port_while_a_sibling_service_is_still_running() {
    let config = r#"
version = 1

[ports]
names = ["web", "api"]

[[secrets]]
from = "backend/.env.local"
into = "backend/.env.local"

[secrets.set]
API_URL = "http://localhost:{{ port.api }}"

[[service]]
name = "web"
command = "python3 -u -m http.server {{ port.web }}"
ready = { http = "http://127.0.0.1:{{ port.web }}/", timeout = "30s" }

[[service]]
name = "api"
command = "python3 -u -m http.server {{ port.api }}"
ready = { http = "http://127.0.0.1:{{ port.api }}/", timeout = "30s" }
"#;
    let cli = Cli::with_config(config);
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();
    let port: u16 = std::fs::read_to_string(wt.join("backend/.env.local"))
        .expect("env")
        .lines()
        .find_map(|l| l.strip_prefix("API_URL=http://localhost:"))
        .expect("API_URL")
        .parse()
        .expect("port");

    // Kill only `api`, the way a crash would, so `web` keeps the instance "running".
    let pid = service_pid(&cli, &wt, "api");
    rustix::process::kill_process_group(
        rustix::process::Pid::from_raw(i32::try_from(pid).expect("pid fits i32")).expect("pid"),
        rustix::process::Signal::KILL,
    )
    .expect("kill api process group");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while ureq::get(format!("http://127.0.0.1:{port}/"))
        .call()
        .is_ok()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "api never let go of {port}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let _squatter = Squatter(
        std::process::Command::new("python3")
            .args(["-u", "-m", "http.server", &port.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("squat the port"),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while ureq::get(format!("http://127.0.0.1:{port}/"))
        .call()
        .is_err()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the squatter never answered"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let out = cli.run(&wt, &["up"]).failure().get_output().stderr.clone();
    let stderr = String::from_utf8_lossy(&out).into_owned();
    assert!(stderr.contains("api not started"), "{stderr}");
    assert!(stderr.contains("already answers"), "{stderr}");
}

/// Commit a lockfile in the main checkout, so worktrees created afterwards carry it.
fn with_lockfile(cli: &Cli, text: &str) {
    std::fs::write(cli.fx.main.join("package-lock.json"), text).expect("lockfile");
    common::git(&cli.fx.main, &["add", "package-lock.json"]);
    common::git(&cli.fx.main, &["commit", "-m", "lockfile"]);
}

fn installs(cli: &Cli) -> usize {
    std::fs::read_to_string(cli.fx.main.parent().unwrap().join("worktrees/installs.log"))
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

/// The whole point: the second worktree on a lockfile pays nothing for its dependencies.
/// Names are private per worktree, blocks are shared — the inode says which.
#[test]
fn a_second_worktree_on_the_same_lockfile_links_instead_of_installing() {
    let cli = Cli::with_config(CACHE_CONFIG);
    with_lockfile(&cli, "lockfile A\n");
    let a = cli.worktree("feat_search");
    let b = cli.worktree("fix_login");

    cli.run(&a, &["up"]).success();
    assert_eq!(installs(&cli), 1);

    let out = cli.run(&b, &["up"]).success().get_output().stderr.clone();
    let stderr = String::from_utf8_lossy(&out).into_owned();
    assert!(stderr.contains("linked from the store"), "{stderr}");
    assert_eq!(installs(&cli), 1, "the second worktree ran setup again");

    use std::os::unix::fs::MetadataExt;
    let blob_a = a.join("node_modules/blob").metadata().expect("a's blob");
    let blob_b = b.join("node_modules/blob").metadata().expect("b's blob");
    assert_eq!(
        blob_a.ino(),
        blob_b.ino(),
        "the two worktrees hold different blocks"
    );
    assert!(
        blob_b.nlink() >= 3,
        "store + two worktrees, got nlink {}",
        blob_b.nlink()
    );

    cli.run(&a, &["down"]).success();
    cli.run(&b, &["down"]).success();
}

/// A changed lockfile is simply a different key. Nobody decides anything: the worktree
/// installs once and its tree lands in the store beside the other.
#[test]
fn a_changed_lockfile_installs_again_into_its_own_entry() {
    let cli = Cli::with_config(CACHE_CONFIG);
    with_lockfile(&cli, "lockfile A\n");
    let a = cli.worktree("feat_search");
    let b = cli.worktree("fix_login");
    cli.run(&a, &["up"]).success();

    std::fs::write(b.join("package-lock.json"), "lockfile B\n").expect("edit lockfile");
    let out = cli.run(&b, &["up"]).success().get_output().stderr.clone();
    let stderr = String::from_utf8_lossy(&out).into_owned();
    assert!(stderr.contains("stored as"), "{stderr}");
    assert_eq!(installs(&cli), 2);

    let entries = std::fs::read_dir(cli.state.path().join("store"))
        .expect("store")
        .count();
    assert_eq!(entries, 2, "one entry per lockfile");

    cli.run(&a, &["down"]).success();
    cli.run(&b, &["down"]).success();
}

/// `--no-cache` is the escape hatch for a store entry you distrust: it is evicted and the
/// worktree reinstalls from scratch, and what it builds becomes the entry every later
/// worktree on that lockfile links from.
#[test]
fn up_no_cache_reinstalls_and_replaces_the_store_entry() {
    use std::os::unix::fs::MetadataExt;
    let cli = Cli::with_config(CACHE_CONFIG);
    with_lockfile(&cli, "lockfile A\n");
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();
    let before = wt.join("node_modules/blob").metadata().expect("blob").ino();
    assert_eq!(installs(&cli), 1);
    cli.run(&wt, &["down"]).success();

    let out = cli
        .run(&wt, &["up", "--no-cache"])
        .success()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8_lossy(&out).into_owned();
    assert!(stderr.contains("stored as"), "{stderr}");
    assert_eq!(installs(&cli), 2, "--no-cache must run setup again");

    let after = wt.join("node_modules/blob").metadata().expect("blob");
    assert_ne!(
        after.ino(),
        before,
        "the old entry's blocks are still what the worktree holds"
    );
    assert!(
        after.nlink() >= 2,
        "the fresh tree must be the new store entry, got nlink {}",
        after.nlink()
    );

    cli.run(&wt, &["down"]).success();
}

/// A store entry is only worth keeping while some registered worktree would link from
/// it. Deleting one frees nothing a worktree holds, so the only mistake possible is
/// removing an entry a stopped instance would have used on its next `up`.
#[test]
fn prune_removes_store_entries_no_worktree_references() {
    let cli = Cli::with_config(CACHE_CONFIG);
    with_lockfile(&cli, "lockfile A\n");
    let kept = cli.worktree("feat_search");
    let doomed = cli.worktree("fix_login");
    cli.run(&kept, &["up"]).success();
    std::fs::write(doomed.join("package-lock.json"), "lockfile B\n").expect("edit lockfile");
    cli.run(&doomed, &["up"]).success();
    cli.run(&kept, &["down"]).success();
    cli.run(&doomed, &["down"]).success();
    let store = cli.state.path().join("store");
    assert_eq!(std::fs::read_dir(&store).expect("store").count(), 2);
    let kept_hash = registry_of(&cli)
        .get(&kept)
        .expect("get")
        .expect("entry")
        .cache_keys
        .get("web")
        .cloned()
        .expect("kept's key recorded");

    std::fs::remove_dir_all(&doomed).expect("delete the worktree");
    let out = cli
        .run(&cli.fx.main, &["prune"])
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out).into_owned();
    assert!(stdout.contains("store"), "{stdout}");

    let left: Vec<String> = std::fs::read_dir(&store)
        .expect("store")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, vec![kept_hash], "{stdout}");
}

/// A clean machine gets a report that says so, and an exit code scripts can gate on.
#[test]
fn health_is_quiet_on_a_clean_machine() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();

    let out = cli
        .run(&wt, &["health"])
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out).into_owned();
    assert!(!stdout.contains("FAIL"), "{stdout}");
    assert!(stdout.contains("ok"), "{stdout}");
    assert!(stdout.contains("instances running"), "{stdout}");

    cli.run(&wt, &["down"]).success();
}

/// The case the command exists for: something is listening on a grove port and no
/// instance is running there — a `down` that missed a child, a server from an older
/// grove, a squatter. Health names it and hands over the command that ends it.
#[test]
fn health_names_a_listener_no_instance_owns() {
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

    let _squatter = Squatter(
        std::process::Command::new("python3")
            .args(["-u", "-m", "http.server", &port.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("squat the port"),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while ureq::get(format!("http://127.0.0.1:{port}/"))
        .call()
        .is_err()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the squatter never answered"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let out = cli
        .run(&wt, &["health"])
        .failure()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out).into_owned();
    assert!(stdout.contains("FAIL"), "{stdout}");
    assert!(stdout.contains(&format!("port {port}")), "{stdout}");
    assert!(
        stdout.contains("feat_search"),
        "must say who reserved it: {stdout}"
    );
    assert!(stdout.contains("kill"), "must hand over the fix: {stdout}");
}

#[test]
fn health_flags_orphans_and_points_at_prune() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();
    cli.run(&wt, &["down"]).success();
    std::fs::remove_dir_all(&wt).expect("delete the worktree");

    let out = cli
        .run(&cli.fx.main, &["health"])
        .failure()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out).into_owned();
    assert!(stdout.contains("FAIL"), "{stdout}");
    assert!(stdout.contains("orphan"), "{stdout}");
    assert!(stdout.contains("grove prune"), "{stdout}");
}

#[test]
fn health_json_lists_every_finding_with_its_fix() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();
    cli.run(&wt, &["down"]).success();
    std::fs::remove_dir_all(&wt).expect("delete the worktree");

    let out = cli
        .run(&cli.fx.main, &["health", "--json"])
        .failure()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&out).expect("json");
    let findings = json.as_array().expect("an array of findings");
    let orphan = findings
        .iter()
        .find(|f| f["level"] == "fail" && f["what"].as_str().unwrap_or("").contains("orphan"))
        .expect("the orphan finding");
    assert!(
        orphan["fix"].as_str().unwrap_or("").contains("grove prune"),
        "{json}"
    );
    assert!(findings.iter().any(|f| f["level"] == "ok"), "{json}");
}

/// The shared datastore listens inside grove's range on every machine, and it is nobody's
/// stray. The registry records where each instance's database lives, which is enough to
/// tell it from a server a `down` missed.
#[test]
fn health_does_not_mistake_a_recorded_datastore_for_a_stray_listener() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    cli.run(&wt, &["up"]).success();
    cli.run(&wt, &["down"]).success();

    let port = cli.ports.start + 90;
    let _datastore = TcpListener::bind(("127.0.0.1", port)).expect("stand in for mongo");
    let registry = registry_of(&cli);
    let mut entry = registry.get(&wt).expect("get").expect("entry");
    entry.db_name = Some("app_feat_search".into());
    entry.db_resource = Some(grove::registry::DbResource {
        name: "mongo".into(),
        port,
    });
    registry.record(&entry).expect("record");

    let out = cli
        .run(&wt, &["health"])
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out).into_owned();
    assert!(!stdout.contains("FAIL"), "{stdout}");
    assert!(
        stdout.contains("mongo"),
        "should name the datastore it recognised: {stdout}"
    );
}

/// An instance nobody has touched past the repo's window is stopped by the next `up`,
/// whoever runs it. The pile-up forms one agent at a time, and `up` is the one moment the
/// agent adding to it is present — so with the repo opted in, that moment acts.
#[test]
fn up_stops_sibling_instances_idle_past_the_repos_window() {
    let config = CONFIG.replace("[ports]", "[idle]\nstop_after = \"1h\"\n\n[ports]");
    let cli = Cli::with_config(&config);
    let stale = cli.worktree("fix_login");
    let fresh = cli.worktree("feat_search");
    cli.run(&stale, &["up"]).success();
    backdate(&cli, &stale, 3 * 3600);

    let out = cli
        .run(&fresh, &["up"])
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out).into_owned();
    assert!(stdout.contains("stopped fix_login"), "{stdout}");
    assert!(stdout.contains("idle"), "{stdout}");

    let entry = registry_of(&cli).get(&stale).expect("get").expect("entry");
    assert!(!entry.is_running(), "the stale instance is still running");
    assert!(!entry.ports.is_empty(), "its ports must stay reserved");

    cli.run(&fresh, &["down"]).success();
}

/// Without the opt-in, `up` only warns, as it always has.
#[test]
fn up_leaves_idle_siblings_alone_when_the_repo_declares_no_window() {
    let cli = Cli::new();
    let stale = cli.worktree("fix_login");
    let fresh = cli.worktree("feat_search");
    cli.run(&stale, &["up"]).success();
    backdate(&cli, &stale, 3 * 3600);

    cli.run(&fresh, &["up"]).success();
    let entry = registry_of(&cli).get(&stale).expect("get").expect("entry");
    assert!(
        entry.is_running(),
        "up stopped a sibling without being asked to"
    );

    cli.run(&stale, &["down"]).success();
    cli.run(&fresh, &["down"]).success();
}

/// Memory is the number this machine ran out of on 2026-09-10 while load and disk looked
/// survivable. Health reports it from the gauge that separates a healthy machine from a
/// dying one: swap, not free pages.
#[test]
fn health_reports_memory_and_swap() {
    let cli = Cli::new();
    let wt = cli.worktree("feat_search");
    let out = cli
        .run(&wt, &["health", "--json"])
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&out).expect("json");
    let memory = json
        .as_array()
        .expect("array")
        .iter()
        .find(|f| f["what"].as_str().unwrap_or("").contains("swap"))
        .expect("a finding about memory and swap");
    assert!(memory["what"].as_str().unwrap().contains("free"), "{json}");
}

#[test]
fn an_existing_worktree_switches_to_the_cached_changed_lockfile() {
    let config = CACHE_CONFIG.replace(
        "head -c 1048576 /dev/zero > node_modules/blob",
        "cp package-lock.json node_modules/blob",
    );
    let cli = Cli::with_config(&config);
    with_lockfile(&cli, "version A\n");
    let existing = cli.worktree("existing");
    let seed = cli.worktree("seed");
    cli.run(&existing, &["up"]).success();
    std::fs::write(seed.join("package-lock.json"), "version B\n").expect("lockfile");
    cli.run(&seed, &["up"]).success();
    std::fs::write(existing.join("package-lock.json"), "version B\n").expect("lockfile");
    cli.run(&existing, &["up"]).success();
    assert_eq!(
        std::fs::read_to_string(existing.join("node_modules/blob")).unwrap(),
        "version B\n"
    );
    assert_eq!(
        installs(&cli),
        2,
        "reuse the warm entry instead of installing again"
    );
}

#[test]
fn a_failed_store_link_keeps_the_existing_dependencies() {
    let cli = Cli::with_config(CACHE_CONFIG);
    with_lockfile(&cli, "lockfile A\n");
    let existing = cli.worktree("existing");
    cli.run(&existing, &["up"]).success();
    let entry = registry_of(&cli).get(&existing).unwrap().unwrap();
    let hash = &entry.cache_keys["web"];
    let tree = grove::store::entry(&cli.state.path().join("store"), hash);
    std::fs::remove_dir_all(&tree).expect("remove fixture store");
    std::fs::write(&tree, "invalid tree").expect("corrupt fixture store");
    std::fs::remove_file(entry.instance_dir.unwrap().join(".setup-web")).expect("forget setup");
    cli.run(&existing, &["up"]).failure();
    assert!(
        existing.join("node_modules/blob").exists(),
        "failed refresh destroyed the old install"
    );
}

#[test]
fn a_truncated_store_does_not_produce_a_successful_install() {
    let cli = Cli::with_config(CACHE_CONFIG);
    with_lockfile(&cli, "lockfile A\n");
    let seed = cli.worktree("seed");
    let fresh = cli.worktree("fresh");
    cli.run(&seed, &["up"]).success();
    let entry = registry_of(&cli).get(&seed).unwrap().unwrap();
    let tree = grove::store::entry(&cli.state.path().join("store"), &entry.cache_keys["web"]);
    std::fs::remove_file(tree.join("blob")).expect("truncate store");
    let output = cli
        .run(&fresh, &["up"])
        .failure()
        .get_output()
        .stderr
        .clone();
    assert!(String::from_utf8_lossy(&output).contains("incomplete"));
    let state = registry_of(&cli).get(&fresh).unwrap().unwrap();
    assert!(!state.instance_dir.unwrap().join(".setup-web").exists());
    assert!(!fresh.join("node_modules").exists());
}

#[test]
fn a_failed_reinstall_restores_the_previous_install() {
    let cli = Cli::with_config(CACHE_CONFIG);
    with_lockfile(&cli, "lockfile A\n");
    let wt = cli.worktree("existing");
    cli.run(&wt, &["up"]).success();
    let failing = CACHE_CONFIG.replace(
        "mkdir -p node_modules &&",
        "exit 1; mkdir -p node_modules &&",
    );
    std::fs::write(wt.join(".grove.toml"), failing).unwrap();
    cli.run(&wt, &["up"]).failure();
    assert!(wt.join("node_modules/blob").exists());
}

#[test]
fn simultaneous_worktrees_install_one_shared_entry() {
    let cli = Cli::with_config(&CACHE_CONFIG.replace(
        "mkdir -p node_modules &&",
        "sleep 1; mkdir -p node_modules &&",
    ));
    with_lockfile(&cli, "lockfile A\n");
    let worktrees = [
        cli.worktree("first"),
        cli.worktree("second"),
        cli.worktree("third"),
    ];
    let mut children = Vec::new();
    for wt in &worktrees {
        children.push(
            std::process::Command::new(assert_cmd::cargo::cargo_bin("grove"))
                .arg("up")
                .current_dir(wt)
                .env("GROVE_STATE_DIR", cli.state.path())
                .env("GROVE_CACHE_DIR", cli.state.path().join("store"))
                .env("GROVE_PORT_RANGE", cli.port_range())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("start up"),
        );
    }
    for child in children {
        let output = child.wait_with_output().expect("wait up");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(
        installs(&cli),
        1,
        "concurrent cache misses must share one install"
    );
    for wt in worktrees {
        assert!(wt.join("node_modules/blob").exists());
    }
}

#[test]
fn a_warm_nested_cache_creates_the_missing_parent_directory() {
    let config = CACHE_CONFIG.replace("node_modules", "build/deps");
    let cli = Cli::with_config(&config);
    with_lockfile(&cli, "lockfile A\n");
    let seed = cli.worktree("seed");
    let fresh = cli.worktree("fresh");
    cli.run(&seed, &["up"]).success();
    assert!(!fresh.join("build").exists());
    cli.run(&fresh, &["up"]).success();
    assert!(fresh.join("build/deps/blob").exists());
    assert_eq!(installs(&cli), 1);
}

#[test]
fn setup_cannot_publish_a_tree_under_changed_cache_inputs() {
    let config = CACHE_CONFIG.replace(
        "mkdir -p node_modules &&",
        "echo changed > package-lock.json; mkdir -p node_modules &&",
    );
    let cli = Cli::with_config(&config);
    with_lockfile(&cli, "lockfile A\n");
    let wt = cli.worktree("changed");
    let output = cli.run(&wt, &["up"]).failure().get_output().stderr.clone();
    assert!(String::from_utf8_lossy(&output).contains("setup changed its cache key files"));
    let entry = registry_of(&cli).get(&wt).unwrap().unwrap();
    assert!(!entry.instance_dir.unwrap().join(".setup-web").exists());
    assert!(!wt.join("node_modules").exists());
    assert!(!cli.state.path().join("store").exists());
}

#[test]
fn a_frontend_setup_failure_after_backend_setup_names_the_failed_step() {
    let config = CACHE_CONFIG.replace("name = \"web\"", "name = \"frontend\"").replace(
        "[[service]]",
        "[[service]]\nname = \"backend\"\nsetup = \"touch backend-ready\"\ncommand = \"sleep 60\"\n\n[[service]]",
    );
    let cli = Cli::with_config(&config);
    let wt = cli.worktree("missing_lockfile");
    let output = cli.run(&wt, &["up"]).failure().get_output().clone();
    assert!(wt.join("backend-ready").exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("frontend: dependency setup failed"));
}

#[test]
fn every_configured_setup_reports_completion_before_up_succeeds() {
    let config = CACHE_CONFIG.replace("name = \"web\"", "name = \"frontend\"").replace(
        "[[service]]",
        "[[service]]\nname = \"backend\"\nsetup = \"touch backend-ready\"\ncommand = \"sleep 60\"\n\n[[service]]",
    );
    let cli = Cli::with_config(&config);
    with_lockfile(&cli, "lockfile A\n");
    let wt = cli.worktree("complete");
    cli.run(&wt, &["up"]).success();
    let output = cli.run(&wt, &["up"]).success().get_output().clone();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("backend: dependency setup complete"),
        "{stderr}"
    );
    assert!(
        stderr.contains("frontend: dependency setup complete"),
        "{stderr}"
    );
    assert!(wt.join("backend-ready").exists());
    assert!(wt.join("node_modules/blob").exists());
}

fn phase_seconds(stderr: &str, phase: &str, outcome: &str) -> f64 {
    let prefix = format!("timing: {phase}: {outcome} (");
    let value = stderr
        .lines()
        .find_map(|line| {
            line.strip_prefix(&prefix)
                .and_then(|value| value.strip_suffix("s)"))
        })
        .unwrap_or_else(|| panic!("missing {prefix} in {stderr}"));
    let seconds: f64 = value.parse().expect("duration in seconds");
    assert!(seconds.is_finite() && seconds >= 0.0);
    seconds
}

#[test]
fn phase_durations_distinguish_warm_link_work_from_setup_and_seeds() {
    let config = format!("{CACHE_CONFIG}\n[[seed]]\nname = \"demo\"\ncommand = \"sleep 0.1\"\n");
    let config = config.replace("[[service]]", "[[service]]\nprepare = \"sleep 0.1\"");
    let cli = Cli::with_config(&config);
    with_lockfile(&cli, "timing fixture\n");
    let seed = cli.worktree("seed");
    cli.run(&seed, &["up"]).success();
    let wt = cli.worktree("warm");
    std::fs::create_dir(wt.join("node_modules")).unwrap();
    std::fs::write(wt.join("node_modules/old"), "old install").unwrap();
    let output = cli.run(&wt, &["up"]).success().get_output().clone();
    let stderr = String::from_utf8_lossy(&output.stderr);
    for phase in [
        "web: setup lock wait",
        "web: shared cache lock wait",
        "web: dependency setup",
        "web: dependency replacement",
        "web: old install cleanup",
        "dependency footprint",
        "web: process start",
        "web: readiness",
    ] {
        phase_seconds(&stderr, phase, "ok");
    }
    let entry = registry_of(&cli).get(&wt).unwrap().unwrap();
    let hash = &entry.cache_keys["web"];
    for phase in ["inventory read", "tree creation", "inventory validation"] {
        phase_seconds(&stderr, &format!("cache {hash}: {phase}"), "ok");
    }
    assert!(phase_seconds(&stderr, "seed demo", "ok") >= 0.1);
    assert!(phase_seconds(&stderr, "web: prepare", "ok") >= 0.1);
    assert_eq!(installs(&cli), 1);
    assert!(!wt.join("node_modules/old").exists());
    assert!(wt.join("node_modules/blob").exists());
    let output = cli.run(&wt, &["up"]).success().get_output().clone();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("timing: seed demo:"),
        "a skipped seed must not report execution time"
    );
    assert!(!stderr.contains("tree creation:"));
}

#[test]
fn phase_durations_report_failed_seeds_without_changing_the_exit_status() {
    let cli = Cli::with_config(&format!(
        "{CONFIG}\n[[seed]]\nname = \"broken\"\ncommand = \"sleep 0.1; exit 7\"\n"
    ));
    let wt = cli.worktree("broken");
    let output = cli.run(&wt, &["up"]).failure().get_output().clone();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(phase_seconds(&stderr, "seed broken", "failed") >= 0.1);
    assert!(stderr.contains("seed broken failed"));
    assert!(!stderr.contains("timing: web: process start:"));
}

#[test]
fn backup_cleanup_failure_warns_without_failing_a_completed_install() {
    use std::os::unix::fs::PermissionsExt;

    for warm in [false, true] {
        let cli = Cli::with_config(CACHE_CONFIG);
        with_lockfile(&cli, "cleanup fixture\n");
        if warm {
            let seed = cli.worktree("seed");
            cli.run(&seed, &["up"]).success();
        }
        let wt = cli.worktree("old_install");
        let protected = wt.join("node_modules/protected");
        std::fs::create_dir_all(&protected).unwrap();
        std::fs::write(protected.join("old"), "old install").unwrap();
        // A deterministic filesystem error, without a concurrent writer's timing race.
        std::fs::set_permissions(&protected, std::fs::Permissions::from_mode(0o555)).unwrap();
        let output = cli.run(&wt, &["up"]).get_output().clone();
        let stderr = String::from_utf8_lossy(&output.stderr);
        let backup = std::path::PathBuf::from(
            stderr
                .lines()
                .find_map(|line| {
                    line.split_once("backup retained at ")
                        .and_then(|(_, path)| {
                            path.strip_suffix(". The new dependencies are ready.")
                        })
                })
                .expect("failed cleanup retains the backup"),
        );
        // Restore permissions before assertions so fixture cleanup also works on failure.
        std::fs::set_permissions(
            backup.join("protected"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "warm={warm}: {stderr}");
        assert!(
            stderr.contains("warning: web: old install cleanup failed"),
            "{stderr}"
        );
        assert!(stderr.contains(backup.to_str().unwrap()), "{stderr}");
        phase_seconds(&stderr, "web: old install cleanup", "failed");
        assert!(backup.join("protected/old").exists());
        common::git(&wt, &["add", "-A"]);
        let staged = common::git(&wt, &["diff", "--cached", "--name-only"]);
        assert!(
            !staged.contains("protected/old"),
            "retained dependencies must stay out of Git: {staged}"
        );

        assert!(wt.join("node_modules/blob").exists());
        assert!(service_pid(&cli, &wt, "web") > 0);
        let entry = registry_of(&cli).get(&wt).unwrap().unwrap();
        assert!(entry.instance_dir.unwrap().join(".setup-web").exists());
        let retry = cli.run(&wt, &["up"]).success().get_output().clone();
        assert!(!String::from_utf8_lossy(&retry.stderr).contains("tree creation"));
        assert_eq!(installs(&cli), 1);
    }
}

// Pin a directory descriptor like a watcher with in-flight cache operations. The
// writer records whether its original tree still existed when TERM arrived.
fn writer_fixture() -> Cli {
    let config = CACHE_CONFIG.replace(
        "mkdir -p node_modules && head -c 1048576 /dev/zero > node_modules/blob && echo installed >> ../installs.log",
        "python3 setup.py",
    ).replace("python3 -u -m http.server {{ port.web }}", "exec python3 writer.py {{ port.web }}");
    let cli = Cli::with_config(&config);
    with_lockfile(&cli, "old");
    std::fs::write(
        cli.fx.main.join("setup.py"),
        r#"
from pathlib import Path
import time
assert not Path('writer.running').exists(), 'live writer during install'
Path('node_modules/.vite').mkdir(parents=True)
Path('node_modules/version').write_text(Path('package-lock.json').read_text())
if Path('fail-install').exists():
    raise RuntimeError('deliberate install failure')
"#,
    )
    .unwrap();
    std::fs::write(cli.fx.main.join("writer.py"), r#"
import http.server, os, signal, sys, threading, time
from pathlib import Path
root = Path.cwd()
version = Path('node_modules/version').read_text()
fd = os.open('node_modules/.vite', os.O_RDONLY)
Path('writer.running').write_text(str(os.getpid()))
def stop(*args):
    with open(root / 'stops', 'a') as log:
        log.write(version + ':' + str((root / 'node_modules/version').read_text() == version) + '\n')
    (root / 'writer.running').unlink(missing_ok=True)
    os._exit(0)
signal.signal(signal.SIGTERM, stop)
def write():
    while True:
        f = os.open('writer', os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600, dir_fd=fd)
        os.write(f, version.encode())
        os.close(f)
        time.sleep(0.005)
threading.Thread(target=write, daemon=True).start()
Path('served-version').write_text(version)
http.server.HTTPServer(('127.0.0.1', int(sys.argv[1])), http.server.SimpleHTTPRequestHandler).serve_forever()
"#).unwrap();
    common::git(&cli.fx.main, &["add", "setup.py", "writer.py"]);
    common::git(&cli.fx.main, &["commit", "-m", "add dependency writer"]);
    cli
}

#[test]
fn dependency_refresh_stops_real_writer_before_swap_and_restarts_on_new_tree() {
    for warm in [false, true] {
        let cli = writer_fixture();
        let wt = cli.worktree("writer");
        cli.run(&wt, &["up"]).success();
        let old = service_pid(&cli, &wt, "web");
        cli.run(&wt, &["up"]).success();
        assert_eq!(
            service_pid(&cli, &wt, "web"),
            old,
            "unchanged setup must keep its process"
        );
        if warm {
            let prime = cli.worktree("prime");
            std::fs::write(prime.join("package-lock.json"), "new").unwrap();
            cli.run(&prime, &["up"]).success();
        }
        std::fs::write(wt.join("package-lock.json"), "new").unwrap();
        cli.run(&wt, &["up"]).success();
        assert_ne!(service_pid(&cli, &wt, "web"), old);
        assert_eq!(
            std::fs::read_to_string(wt.join("stops")).unwrap(),
            "old:True\n"
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("served-version")).unwrap(),
            "new"
        );
    }
}

#[test]
fn dependency_refresh_failure_restores_tree_and_recovers_real_writer() {
    let cli = writer_fixture();
    let wt = cli.worktree("recover");
    cli.run(&wt, &["up"]).success();
    let old = service_pid(&cli, &wt, "web");
    std::fs::write(wt.join("package-lock.json"), "new").unwrap();
    std::fs::write(wt.join("fail-install"), "").unwrap();
    cli.run(&wt, &["up"]).failure();
    assert_ne!(
        service_pid(&cli, &wt, "web"),
        old,
        "the recovered writer needs a new process"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("node_modules/version")).unwrap(),
        "old"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("stops")).unwrap(),
        "old:True\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("served-version")).unwrap(),
        "old"
    );
}

fn spawn_start(cli: &Cli, wt: &Path, command: &str) -> std::process::Child {
    std::process::Command::new(assert_cmd::cargo::cargo_bin!("grove"))
        .current_dir(wt)
        .env("GROVE_STATE_DIR", cli.state.path())
        .env("GROVE_CACHE_DIR", cli.state.path().join("store"))
        .env("GROVE_PORT_RANGE", cli.port_range())
        .env_remove("GROVE_MACOS_CLONE")
        .arg(command)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn concurrent_up_waits_through_prepare_and_reuses_the_started_service() {
    for command in ["up", "restart"] {
        let config = CACHE_CONFIG.replace(
            "[[service]]",
            "[[service]]\nprepare = \"python3 prepare.py\"",
        );
        let cli = Cli::with_config(&config);
        with_lockfile(&cli, "concurrent");
        std::fs::write(
            cli.fx.main.join("prepare.py"),
            r#"
from pathlib import Path
import time
with open('entered', 'a') as f:
    f.write('prepare\n')
end = time.monotonic() + 15
while not Path('release').exists() and time.monotonic() < end:
    time.sleep(0.01)
assert Path('release').exists(), 'test never released prepare'
"#,
        )
        .unwrap();
        common::git(&cli.fx.main, &["add", "prepare.py"]);
        common::git(&cli.fx.main, &["commit", "-m", "add gated prepare"]);
        let wt = cli.worktree("concurrent");
        let first = spawn_start(&cli, &wt, "up");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !wt.join("entered").exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let second = spawn_start(&cli, &wt, command);
        std::thread::sleep(std::time::Duration::from_secs(1));
        let entries = std::fs::read_to_string(wt.join("entered")).unwrap();
        std::fs::write(wt.join("release"), "").unwrap();
        let first = first.wait_with_output().unwrap();
        let second = second.wait_with_output().unwrap();
        assert_eq!(
            entries, "prepare\n",
            "second up must wait for the whole first up"
        );
        assert!(
            first.status.success(),
            "{}",
            String::from_utf8_lossy(&first.stderr)
        );
        assert!(
            second.status.success(),
            "{}",
            String::from_utf8_lossy(&second.stderr)
        );
        assert!(service_pid(&cli, &wt, "web") > 0);
    }
}

fn up_with_clone(cli: &Cli, wt: &Path) -> std::process::Output {
    Command::cargo_bin("grove")
        .unwrap()
        .current_dir(wt)
        .env("GROVE_STATE_DIR", cli.state.path())
        .env("GROVE_CACHE_DIR", cli.state.path().join("store"))
        .env("GROVE_PORT_RANGE", cli.port_range())
        .env("GROVE_MACOS_CLONE", "1")
        .arg("up")
        .output()
        .unwrap()
}

#[cfg(target_os = "macos")]
#[test]
fn clone_trial_isolates_writes_and_preserves_links_modes_and_replacement() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let config = CACHE_CONFIG.replace(
        "echo installed >> ../installs.log",
        "ln -s blob node_modules/tool && chmod 755 node_modules/blob && echo installed >> ../installs.log",
    );
    let cli = Cli::with_config(&config);
    with_lockfile(&cli, "clone fixture\n");
    let seed = cli.worktree("seed");
    cli.run(&seed, &["up"]).success();
    let wt = cli.worktree("clone");
    for replacement in [false, true] {
        if replacement {
            let entry = registry_of(&cli).get(&wt).unwrap().unwrap();
            std::fs::remove_file(entry.instance_dir.unwrap().join(".setup-web")).unwrap();
            std::fs::write(wt.join("node_modules/obsolete"), "old").unwrap();
        }
        let output = up_with_clone(&cli, &wt);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert!(
            stderr.contains("dependencies cloned from the store"),
            "{stderr}"
        );
        let original = seed.join("node_modules/blob");
        let copy = wt.join("node_modules/blob");
        assert_ne!(
            original.metadata().unwrap().ino(),
            copy.metadata().unwrap().ino()
        );
        assert_eq!(copy.metadata().unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(
            std::fs::read_link(wt.join("node_modules/tool")).unwrap(),
            Path::new("blob")
        );
        assert_eq!(
            std::fs::read(&copy).unwrap(),
            std::fs::read(&original).unwrap()
        );
        std::fs::write(&copy, "private change").unwrap();
        assert_eq!(original.metadata().unwrap().len(), 1048576);
        assert!(!wt.join("node_modules/obsolete").exists());
    }
    let normal = cli.worktree("default");
    cli.run(&normal, &["up"]).success();
    assert_eq!(
        seed.join("node_modules/blob").metadata().unwrap().ino(),
        normal.join("node_modules/blob").metadata().unwrap().ino()
    );
    assert_eq!(installs(&cli), 1);
}

#[cfg(target_os = "macos")]
#[test]
fn clone_trial_rejects_invalid_stores_before_replacing_the_old_install() {
    for corruption in ["incomplete", "not a directory"] {
        let cli = Cli::with_config(CACHE_CONFIG);
        with_lockfile(&cli, "clone fixture\n");
        let seed = cli.worktree("seed");
        cli.run(&seed, &["up"]).success();
        let entry = registry_of(&cli).get(&seed).unwrap().unwrap();
        let tree = grove::store::entry(&cli.state.path().join("store"), &entry.cache_keys["web"]);
        if corruption == "incomplete" {
            std::fs::remove_file(tree.join("blob")).unwrap();
        } else {
            let original = tree.with_file_name("original");
            std::fs::rename(&tree, &original).unwrap();
            std::os::unix::fs::symlink(&original, &tree).unwrap();
        }
        let wt = cli.worktree("old_install");
        std::fs::create_dir(wt.join("node_modules")).unwrap();
        std::fs::write(wt.join("node_modules/old"), "keep me").unwrap();
        let output = up_with_clone(&cli, &wt);
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(corruption), "{stderr}");
        assert_eq!(
            std::fs::read_to_string(wt.join("node_modules/old")).unwrap(),
            "keep me"
        );
        let entry = registry_of(&cli).get(&wt).unwrap().unwrap();
        assert!(!entry.instance_dir.unwrap().join(".setup-web").exists());
        assert!(!std::fs::read_dir(&wt).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".node_modules.grove-stage-")
        }));
    }
}

#[cfg(not(target_os = "macos"))]
#[test]
fn clone_trial_flag_keeps_hardlinks_on_linux() {
    use std::os::unix::fs::MetadataExt;
    let cli = Cli::with_config(CACHE_CONFIG);
    with_lockfile(&cli, "clone fixture\n");
    let seed = cli.worktree("seed");
    cli.run(&seed, &["up"]).success();
    let wt = cli.worktree("linux");
    let output = up_with_clone(&cli, &wt);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        seed.join("node_modules/blob").metadata().unwrap().ino(),
        wt.join("node_modules/blob").metadata().unwrap().ino()
    );
}

const INPUT_SETUP_CONFIG: &str = r#"
version = 1
[ports]
names = ["web"]
[[service]]
name = "web"
cwd = "backend"
setup = "echo installed >> installs; test ! -f fail && if test -f mutate; then echo changed >> uv.lock; fi"
setup_inputs = ["uv.lock", "pyproject.toml"]
command = "sleep 60"
"#;

fn input_setup_worktree(cli: &Cli) -> std::path::PathBuf {
    let wt = cli.worktree("inputs");
    std::fs::create_dir_all(wt.join("backend")).unwrap();
    for file in ["uv.lock", "pyproject.toml"] {
        std::fs::write(wt.join("backend").join(file), "initial").unwrap();
    }
    wt
}

fn input_setup_runs(wt: &Path) -> usize {
    std::fs::read_to_string(wt.join("backend/installs"))
        .unwrap()
        .lines()
        .count()
}

#[test]
fn setup_inputs_rerun_only_when_declared_inputs_or_command_change() {
    let cli = Cli::with_config(INPUT_SETUP_CONFIG);
    let wt = input_setup_worktree(&cli);
    cli.run(&wt, &["up"]).success();
    cli.run(&wt, &["up"]).success();
    assert_eq!(input_setup_runs(&wt), 1);
    for (i, file) in ["uv.lock", "pyproject.toml"].iter().enumerate() {
        std::fs::write(wt.join("backend").join(file), "changed").unwrap();
        cli.run(&wt, &["up"]).success();
        assert_eq!(input_setup_runs(&wt), i + 2);
    }
    std::fs::write(
        wt.join(".grove.toml"),
        INPUT_SETUP_CONFIG.replace("echo installed", "echo updated"),
    )
    .unwrap();
    cli.run(&wt, &["up"]).success();
    assert_eq!(input_setup_runs(&wt), 4);
    std::fs::rename(wt.join("backend/uv.lock"), wt.join("backend/renamed.lock")).unwrap();
    let config = std::fs::read_to_string(wt.join(".grove.toml")).unwrap();
    std::fs::write(
        wt.join(".grove.toml"),
        config.replace(
            "setup_inputs = [\"uv.lock\"",
            "setup_inputs = [\"renamed.lock\"",
        ),
    )
    .unwrap();
    cli.run(&wt, &["up"]).success();
    assert_eq!(input_setup_runs(&wt), 5);
    cli.run(&wt, &["up", "--no-cache"]).success();
    assert_eq!(input_setup_runs(&wt), 6);
    assert!(!cli.state.path().join("store").exists());
}

#[test]
fn setup_inputs_migrate_a_command_only_marker_once() {
    let legacy = INPUT_SETUP_CONFIG.replace("setup_inputs = [\"uv.lock\", \"pyproject.toml\"]", "");
    let cli = Cli::with_config(&legacy);
    let wt = input_setup_worktree(&cli);
    cli.run(&wt, &["up"]).success();
    std::fs::write(wt.join(".grove.toml"), INPUT_SETUP_CONFIG).unwrap();
    cli.run(&wt, &["up"]).success();
    cli.run(&wt, &["up"]).success();
    assert_eq!(input_setup_runs(&wt), 2);
}

#[test]
fn setup_inputs_missing_or_unreadable_files_do_not_reuse_a_marker() {
    for unreadable in [false, true] {
        let cli = Cli::with_config(INPUT_SETUP_CONFIG);
        let wt = input_setup_worktree(&cli);
        cli.run(&wt, &["up"]).success();
        let input = wt.join("backend/uv.lock");
        std::fs::remove_file(&input).unwrap();
        if unreadable {
            std::fs::create_dir(&input).unwrap();
        }
        let output = cli.run(&wt, &["up"]).failure().get_output().stderr.clone();
        let error = String::from_utf8_lossy(&output);
        assert!(
            error.contains("web") && error.contains("uv.lock"),
            "{error}"
        );
        assert_eq!(input_setup_runs(&wt), 1);
    }
}

#[test]
fn setup_inputs_failed_setup_is_retried() {
    let cli = Cli::with_config(INPUT_SETUP_CONFIG);
    let wt = input_setup_worktree(&cli);
    cli.run(&wt, &["up"]).success();
    std::fs::write(wt.join("backend/uv.lock"), "changed").unwrap();
    std::fs::write(wt.join("backend/fail"), "").unwrap();
    cli.run(&wt, &["up"]).failure();
    std::fs::remove_file(wt.join("backend/fail")).unwrap();
    std::fs::write(wt.join("backend/uv.lock"), "initial").unwrap();
    cli.run(&wt, &["up"]).success();
    cli.run(&wt, &["up"]).success();
    assert_eq!(input_setup_runs(&wt), 3);
}

#[test]
fn setup_inputs_changed_during_setup_do_not_get_a_success_marker() {
    let cli = Cli::with_config(INPUT_SETUP_CONFIG);
    let wt = input_setup_worktree(&cli);
    std::fs::write(wt.join("backend/mutate"), "").unwrap();
    let output = cli.run(&wt, &["up"]).failure().get_output().stderr.clone();
    assert!(String::from_utf8_lossy(&output).contains("setup changed its setup_inputs"));
    std::fs::remove_file(wt.join("backend/mutate")).unwrap();
    cli.run(&wt, &["up"]).success();
    cli.run(&wt, &["up"]).success();
    assert_eq!(input_setup_runs(&wt), 2);
}

#[test]
fn doctor_warns_when_setup_cannot_notice_changed_dependency_files() {
    for (declaration, warned) in [
        ("", true),
        ("setup_inputs = ['uv.lock']", false),
        ("cache = {path = 'deps', key = ['uv.lock']}", false),
    ] {
        let config = format!(
            "version = 1\n[ports]\nnames = ['web']\n[[service]]\nname = 'web'\nsetup = 'echo setup'\ncommand = 'sleep 60'\n{declaration}\n"
        );
        let cli = Cli::with_config(&config);
        let wt = cli.worktree("lint");
        let output = cli
            .run(&wt, &["doctor"])
            .success()
            .get_output()
            .stdout
            .clone();
        assert_eq!(
            String::from_utf8_lossy(&output).contains("web: setup tracks only its command"),
            warned
        );
    }
}

#[test]
fn doctor_rejects_a_readiness_probe_without_an_explicit_timeout() {
    let cli = Cli::with_config(&CONFIG.replace(", timeout = \"30s\"", ""));
    let wt = cli.worktree("missing_timeout");
    let output = cli
        .run(&wt, &["doctor"])
        .failure()
        .get_output()
        .stderr
        .clone();
    assert!(String::from_utf8_lossy(&output).contains("missing field `timeout`"));
}

#[test]
fn readiness_timeout_names_the_log_and_observes_tcp_acceptance() {
    for (command, accepted) in [
        ("sleep 60", false),
        (
            "python3 -u -c 'import socket,time; s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1); s.bind((\"127.0.0.1\",{{ port.web }})); s.listen(128); print(\"listening without HTTP\"); time.sleep(60)'",
            true,
        ),
    ] {
        let config = format!(
            "version = 1\n[ports]\nnames = ['web']\n[[service]]\nname = 'web'\ncommand = '''{command}'''\nready = {{http = 'http://localhost:{{{{ port.web }}}}/', timeout = '2s'}}\n"
        );
        let cli = Cli::with_config(&config);
        let wt = cli.worktree("timeout");
        let output = cli.run(&wt, &["up"]).failure().get_output().stderr.clone();
        let error = String::from_utf8_lossy(&output);
        let entry = registry_of(&cli).get(&wt).unwrap().unwrap();
        let log = entry.instance_dir.unwrap().join("web.log");
        assert!(
            error.contains(&format!("full output in {}", log.display())),
            "{error}"
        );
        let expected = if accepted {
            "TCP check: accepted"
        } else {
            "TCP check: refused"
        };
        assert!(error.contains(expected), "{error}");
    }
}

fn saved_up_log(output: &std::process::Output) -> std::path::PathBuf {
    let stderr = String::from_utf8_lossy(&output.stderr);
    std::path::PathBuf::from(
        stderr
            .lines()
            .last()
            .unwrap()
            .strip_prefix("up log: ")
            .expect("last line names run log"),
    )
}

#[test]
fn up_logs_preserve_both_streams_errors_exit_codes_and_prior_runs() {
    use std::os::unix::fs::PermissionsExt;
    let cli = Cli::with_config(INPUT_SETUP_CONFIG);
    let wt = input_setup_worktree(&cli);
    let first = cli.run(&wt, &["up"]).success().get_output().clone();
    let path = saved_up_log(&first);
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(
        body.contains("timing: web: dependency setup: ok") && body.contains("instance  inputs"),
        "{body}"
    );
    assert!(body.contains("grove up exit code: 0"), "{body}");
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::write(wt.join("backend/fail"), "").unwrap();
    let failed = cli
        .run(&wt, &["up", "--no-cache"])
        .failure()
        .get_output()
        .clone();
    let failed_path = saved_up_log(&failed);
    let failed_body = std::fs::read_to_string(&failed_path).unwrap();
    assert!(
        failed_body.contains("dependency setup failed"),
        "{failed_body}"
    );
    assert!(
        failed_body.contains("grove up exit code: 1"),
        "{failed_body}"
    );
    assert_ne!(path, failed_path);
    assert_eq!(std::fs::read_to_string(path).unwrap(), body);
}

#[test]
fn up_logs_preserve_config_errors_before_instance_open() {
    let cli = Cli::with_config(&CONFIG.replace(", timeout = \"30s\"", ""));
    let wt = cli.worktree("bad_config");
    let failed = cli.run(&wt, &["up"]).failure().get_output().clone();
    let body = std::fs::read_to_string(saved_up_log(&failed)).unwrap();
    assert!(
        body.contains("missing field `timeout`") && body.contains("grove up exit code: 1"),
        "{body}"
    );
}

#[test]
fn up_logs_finish_when_a_downstream_reader_closes_early() {
    let cli = Cli::with_config(INPUT_SETUP_CONFIG);
    let wt = input_setup_worktree(&cli);
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("grove"))
        .arg("up")
        .current_dir(&wt)
        .env("GROVE_STATE_DIR", cli.state.path())
        .env("GROVE_PORT_RANGE", cli.port_range())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body = std::fs::read_to_string(saved_up_log(&output)).unwrap();
    assert!(
        body.contains("instance  inputs") && body.contains("grove up exit code: 0"),
        "{body}"
    );
}

#[test]
fn up_logs_accept_the_argument_terminator() {
    let cli = Cli::with_config(INPUT_SETUP_CONFIG);
    let wt = input_setup_worktree(&cli);
    cli.run(&wt, &["up", "--"]).success();
}

#[test]
fn up_logs_cancel_the_worker_before_it_can_start_services() {
    let config = INPUT_SETUP_CONFIG.replace(
        "echo installed >> installs; test ! -f fail && if test -f mutate; then echo changed >> uv.lock; fi",
        "touch started; sleep 3; touch finished",
    );
    let cli = Cli::with_config(&config);
    let wt = input_setup_worktree(&cli);
    let child = std::process::Command::new(assert_cmd::cargo::cargo_bin("grove"))
        .arg("up")
        .current_dir(&wt)
        .env("GROVE_STATE_DIR", cli.state.path())
        .env("GROVE_PORT_RANGE", cli.port_range())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !wt.join("backend/started").exists() {
        assert!(std::time::Instant::now() < deadline, "setup did not start");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // SAFETY: child.id() is the live process this test owns.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(143));
    // A signal to one PID, as before logging existed, does not recursively cancel
    // setup subprocesses. The Grove worker must not start services after they finish.
    std::thread::sleep(std::time::Duration::from_millis(3200));
    assert!(
        registry_of(&cli)
            .get(&wt)
            .unwrap()
            .unwrap()
            .services
            .is_empty()
    );
    let body = std::fs::read_to_string(saved_up_log(&output)).unwrap();
    assert!(body.contains("grove up exit code: 143"), "{body}");
}

#[test]
fn up_logs_keep_the_newest_fifty_without_deleting_active_or_unrelated_files() {
    let cli = Cli::with_config(INPUT_SETUP_CONFIG);
    let wt = input_setup_worktree(&cli);
    let output = cli.run(&wt, &["up"]).success().get_output().clone();
    let log = saved_up_log(&output);
    let dir = log.parent().unwrap();
    for number in 1..=60 {
        std::fs::write(dir.join(format!("{number}-123.log")), "old run").unwrap();
    }
    std::fs::write(dir.join("notes.log"), "keep").unwrap();
    let active = std::fs::File::create(dir.join("0-123.log")).unwrap();
    active.lock().unwrap();
    cli.run(&wt, &["up"]).success();
    assert!(!dir.join("12-123.log").exists());
    assert!(dir.join("13-123.log").exists());
    assert!(
        dir.join("0-123.log").exists(),
        "active log must survive rotation"
    );
    assert!(dir.join("notes.log").exists());
    assert_eq!(
        std::fs::read_dir(dir).unwrap().count(),
        52,
        "50 recent, one active, one unrelated"
    );
    drop(active);
    cli.run(&wt, &["up"]).success();
    assert!(!dir.join("0-123.log").exists());
    assert_eq!(std::fs::read_dir(dir).unwrap().count(), 51);
}

#[test]
fn heavy_run_admission_controls_child_start_and_preserves_its_exit_code() {
    let cases = [
        (true, "1", "", 7, true),
        (true, "4", "", 1, false),
        (true, "8", "", 1, false),
        (true, "8", "max_load = 9\n", 7, true),
        (false, "8", "", 7, true),
        (true, "NaN", "", 1, false),
    ];
    for (heavy, load, threshold, code, started) in cases {
        let cli = Cli::with_config(&format!(
            "{CONFIG}\n[admission]\n{threshold}timeout = \"50ms\"\n"
        ));
        let wt = cli.worktree("admission");
        let mut command = Command::cargo_bin("grove").unwrap();
        command
            .current_dir(&wt)
            .env("GROVE_STATE_DIR", cli.state.path())
            .env("GROVE_PORT_RANGE", cli.port_range())
            .env("GROVE_LOAD", load)
            .env("GROVE_CORES", "4")
            .arg("run");
        if heavy {
            command.arg("--heavy");
        }
        let out = command
            .args(["--", "sh", "-c", "echo $GROVE_SLUG > ran; exit 7"])
            .assert()
            .code(code)
            .get_output()
            .clone();
        assert_eq!(
            wt.join("ran").exists(),
            started,
            "heavy={heavy}, load={load}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        if heavy && !started {
            assert!(stderr.contains("admission"), "{stderr}");
            if load != "NaN" {
                assert!(stderr.contains("timed out"), "{stderr}");
            }
        }
        if started {
            assert_eq!(
                std::fs::read_to_string(wt.join("ran")).unwrap().trim(),
                "admission"
            );
        }
        if !heavy {
            assert!(!stderr.contains("admission:"), "{stderr}");
        }
    }
}

#[test]
fn cancelling_heavy_admission_never_starts_the_child() {
    use std::io::{BufRead, BufReader};
    let cli = Cli::new();
    let wt = cli.worktree("cancel_admission");
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin!("grove"))
        .current_dir(&wt)
        .env("GROVE_STATE_DIR", cli.state.path())
        .env("GROVE_PORT_RANGE", cli.port_range())
        .env("GROVE_LOAD", "100")
        .env("GROVE_CORES", "4")
        .args(["run", "--heavy", "--", "touch", "ran"])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    let waiting = line.contains("admission: waiting");
    let _ = rustix::process::kill_process(
        rustix::process::Pid::from_raw(child.id() as i32).unwrap(),
        rustix::process::Signal::INT,
    );
    assert!(!child.wait().unwrap().success());
    assert!(waiting, "expected admission wait, got {line}");
    assert!(!wt.join("ran").exists());
}

fn stopped_resource_fixture(port: u16, image: &str, bindings: serde_json::Value) -> Cli {
    let cli = Cli::with_fake_docker(
        &resource_seed_config(port, "echo seed >> seeded.log"),
        "stopped-resource",
    );
    let docker = cli.docker.as_ref().expect("docker");
    let inspect = serde_json::json!([{
        "Id": "stopped-resource",
        "State": {"Running": false, "ExitCode": 137},
        "Config": {"Image": image},
        "HostConfig": {"Ulimits": [], "PortBindings": bindings}
    }]);
    std::fs::write(&docker.inspect, inspect.to_string()).expect("inspect fixture");
    std::fs::write(
        &docker.program,
        r#"#!/bin/sh
root=$(dirname "$GROVE_DOCKER_INSPECT")
printf '%s\n' "$*" >> "$root/docker-calls"
case "$1" in
  inspect) cat "$GROVE_DOCKER_INSPECT" ;;
  start)
    if [ -f "$root/fail-start" ]; then echo 'start failed deliberately' >&2; exit 42; fi
    touch "$root/start-requested"
    i=0
    while [ ! -f "$root/port-ready" ] && [ "$i" -lt 500 ]; do
      sleep 0.01; i=$((i + 1))
    done
    test -f "$root/port-ready"
    ;;
  run) echo 'unexpected replacement container' >&2; exit 91 ;;
  *) exit 0 ;;
esac
"#,
    )
    .expect("docker script");
    cli
}

#[test]
fn up_restarts_a_compatible_stopped_resource_without_recreating_it() {
    let reservation = TcpListener::bind("127.0.0.1:0").expect("reserve port");
    let port = reservation.local_addr().expect("address").port();
    let bindings = serde_json::json!({format!("{port}/tcp"): [
        {"HostIp": "", "HostPort": port.to_string()}
    ]});
    let cli = stopped_resource_fixture(port, "mongo:8", bindings);
    let wt = cli.worktree("resource_recovery");
    cli.run(&wt, &["up"]).success();
    let root = cli.state.path().to_path_buf();
    let (stop, stopped) = std::sync::mpsc::channel();
    drop(reservation);
    let server = std::thread::spawn(move || {
        while !root.join("start-requested").exists() {
            match stopped.recv_timeout(std::time::Duration::from_millis(10)) {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                _ => return,
            }
        }
        let _listener = TcpListener::bind(("127.0.0.1", port)).expect("resource port");
        std::fs::write(root.join("port-ready"), "ready").expect("ready");
        let _ = stopped.recv_timeout(std::time::Duration::from_secs(15));
    });
    let result = cli.run(&wt, &["up"]);
    let _ = stop.send(());
    server.join().expect("server");
    result.success();
    assert_eq!(
        std::fs::read_to_string(wt.join("seeded.log")).expect("seed count"),
        "seed\n"
    );
    let calls = std::fs::read_to_string(cli.state.path().join("docker-calls")).expect("calls");
    assert!(
        calls.lines().any(|line| line == "start stopped-resource"),
        "{calls}"
    );
    assert!(
        !calls.lines().any(|line| line.starts_with("run ")),
        "{calls}"
    );
}

#[test]
fn up_refuses_stopped_resources_with_a_different_image_or_port_binding() {
    for (image, bindings) in [
        (
            "mongo:7",
            serde_json::json!({"0/tcp": [{"HostIp":"", "HostPort":"0"}]}),
        ),
        (
            "mongo:8",
            serde_json::json!({"0/tcp": [{"HostIp":"", "HostPort":"1234"}]}),
        ),
        (
            "mongo:8",
            serde_json::json!({"1234/tcp": [{"HostIp":"", "HostPort":"0"}]}),
        ),
        (
            "mongo:8",
            serde_json::json!({"0/tcp": [{"HostIp":"192.0.2.1", "HostPort":"0"}]}),
        ),
    ] {
        let cli = stopped_resource_fixture(0, image, bindings);
        let wt = cli.worktree("resource_mismatch");
        let result = cli.run(&wt, &["up"]).failure();
        let stderr = String::from_utf8_lossy(&result.get_output().stderr);
        assert!(stderr.contains("does not match"), "{stderr}");
        let calls = std::fs::read_to_string(cli.state.path().join("docker-calls")).expect("calls");
        assert!(
            !calls
                .lines()
                .any(|line| line.starts_with("start ") || line.starts_with("run ")),
            "{calls}"
        );
    }
}

#[test]
fn failed_resource_restart_does_not_create_a_replacement() {
    let cli = stopped_resource_fixture(
        0,
        "mongo:8",
        serde_json::json!({
            "0/tcp": [{"HostIp":"", "HostPort":"0"}]
        }),
    );
    std::fs::write(cli.state.path().join("fail-start"), "fail").expect("failure mode");
    let wt = cli.worktree("resource_start_failure");
    let result = cli.run(&wt, &["up"]).failure();
    let stderr = String::from_utf8_lossy(&result.get_output().stderr);
    assert!(stderr.contains("start failed deliberately"), "{stderr}");
    let calls = std::fs::read_to_string(cli.state.path().join("docker-calls")).expect("calls");
    assert!(
        !calls.lines().any(|line| line.starts_with("run ")),
        "{calls}"
    );
}

#[test]
fn resource_inspect_failure_never_creates_or_starts_a_container() {
    let cli = stopped_resource_fixture(0, "mongo:8", serde_json::json!({}));
    cli.docker.as_ref().unwrap().set_unreadable_inspect();
    let wt = cli.worktree("inspect_failure");
    cli.run(&wt, &["up"]).failure();
    let calls = std::fs::read_to_string(cli.state.path().join("docker-calls")).unwrap();
    assert!(
        calls.lines().all(|line| line.starts_with("inspect ")),
        "{calls}"
    );
}

#[test]
fn running_resource_waits_for_its_port_without_starting_it_again() {
    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    let cli = stopped_resource_fixture(
        port,
        "mongo:8",
        serde_json::json!({format!("{port}/tcp"): [{"HostIp":"127.0.0.1", "HostPort":port.to_string()}]}),
    );
    let docker = cli.docker.as_ref().unwrap();
    let mut fixture: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&docker.inspect).unwrap()).unwrap();
    fixture[0]["State"]["Running"] = true.into();
    std::fs::write(&docker.inspect, fixture.to_string()).unwrap();
    let root = cli.state.path().to_owned();
    let (stop, stopped) = std::sync::mpsc::channel();
    drop(reservation);
    let server = std::thread::spawn(move || {
        while !root.join("docker-calls").exists() {
            match stopped.recv_timeout(std::time::Duration::from_millis(10)) {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                _ => return,
            }
        }
        let _listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        let _ = stopped.recv_timeout(std::time::Duration::from_secs(15));
    });
    let wt = cli.worktree("running_resource");
    let result = cli.run(&wt, &["up"]);
    let _ = stop.send(());
    server.join().unwrap();
    result.success();
    let calls = std::fs::read_to_string(cli.state.path().join("docker-calls")).unwrap();
    assert!(
        calls.lines().all(|line| line.starts_with("inspect ")),
        "{calls}"
    );
}

#[test]
fn long_database_names_are_bounded_stable_and_used_by_seed_env_and_registry() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("database listener");
    let port = listener.local_addr().expect("address").port();
    let config = format!(
        r#"
version = 1
[[resource]]
name = "mongo"
kind = "docker-shared"
port = {port}
db_name = "app_{{{{ slug }}}}"
[[secrets]]
from = "backend/.env.local"
into = "backend/.env.local"
[secrets.set]
DATABASE_NAME = "{{{{ db.name }}}}"
[[seed]]
name = "record"
command = "printf '%s' '{{{{ db.name }}}}' > seed-db.txt"
"#
    );
    let cli = Cli::with_config(&config);
    let long_prefix = "feature_".to_owned() + &"long_branch_".repeat(6);
    let slugs = [
        "x".repeat(59),
        "x".repeat(60),
        format!("{long_prefix}a"),
        format!("{long_prefix}b"),
    ];
    let mut names = Vec::new();
    for slug in &slugs {
        let wt = cli.worktree(slug);
        cli.run(&wt, &["up"]).success();
        let entry = registry_of(&cli)
            .get(&wt)
            .expect("registry")
            .expect("instance");
        let name = entry.db_name.expect("database name");
        assert!(name.len() <= 63, "{} bytes: {name}", name.len());
        if slug.len() == 59 {
            assert_eq!(name, format!("app_{slug}"));
        }
        assert_eq!(
            std::fs::read_to_string(wt.join("seed-db.txt")).expect("seed output"),
            name
        );
        let env = std::fs::read_to_string(wt.join("backend/.env.local")).expect("rendered env");
        assert!(
            env.lines()
                .any(|line| line == format!("DATABASE_NAME={name}")),
            "database env differs"
        );
        cli.run(&wt, &["up"]).success();
        assert_eq!(
            registry_of(&cli)
                .get(&wt)
                .expect("registry")
                .expect("instance")
                .db_name
                .as_deref(),
            Some(name.as_str())
        );
        names.push(name);
    }
    let unique: std::collections::BTreeSet<_> = names.iter().collect();
    assert_eq!(
        unique.len(),
        names.len(),
        "long names share a truncated prefix but must differ"
    );
}
