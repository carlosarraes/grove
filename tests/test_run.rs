mod common;
#[path = "common/test_mongo.rs"]
mod mongo;
use std::process::Command;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_grove")
}

#[test]
fn concurrent_test_runs_allocate_unique_recorded_databases_and_preserve_exit_codes() {
    let fx = common::Fixture::new();
    let (root, _, config) = mongo::fixture("ok");
    std::fs::write(
        fx.main.join(".grove.toml"),
        format!(
            "version = 1\n[test_mongo]\nimage = 'mongo:8.0.20'\nport = {}",
            config.port
        ),
    )
    .unwrap();
    let wt = fx.add_worktree("mongo-pilot");
    let execute = || {
        Command::new(binary()).current_dir(&wt)
            .env("GROVE_STATE_DIR", root.path().join("state"))
            .env("GROVE_DOCKER", root.path().join("docker"))
            .args(["run", "--test-mongo", "--", "python3", "-c", r#"
import concurrent.futures, json, os, subprocess
with concurrent.futures.ThreadPoolExecutor(4) as pool:
    names = list(pool.map(lambda _: subprocess.check_output([os.environ['GROVE_TEST_ALLOCATOR'], 'test-db', 'allocate'], text=True).strip(), range(8)))
print(json.dumps([os.environ['GROVE_TEST_DB_NAME'], *names]))
raise SystemExit(23)
"#]).output().unwrap()
    };
    let (a, b) = std::thread::scope(|s| {
        let a = s.spawn(execute);
        let b = s.spawn(execute);
        (a.join().unwrap(), b.join().unwrap())
    });
    let mut names = std::collections::BTreeSet::new();
    for output in [a, b] {
        assert_eq!(
            output.status.code(),
            Some(23),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let run: Vec<String> = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(run.len(), 9);
        for name in run {
            assert!(names.insert(name));
        }
    }
    let calls = std::fs::read_to_string(root.path().join("calls")).unwrap();
    let drops: Vec<serde_json::Value> = calls
        .lines()
        .filter(|line| line.contains("dropDatabase"))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(drops.len(), 2);
    for name in names {
        assert_eq!(
            drops
                .iter()
                .filter(|call| call.to_string().contains(&name))
                .count(),
            1
        );
    }
    let records = root.path().join("state/test-mongo/runs");
    assert!(
        !std::fs::read_dir(records).unwrap().any(|e| e
            .unwrap()
            .path()
            .extension()
            .is_some_and(|e| e == "json"))
    );
}

#[test]
fn allocator_without_a_run_fails_without_docker() {
    let output = Command::new(binary())
        .args(["test-db", "allocate"])
        .env_remove("GROVE_TEST_RUN_ID")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("active test run"));
}

fn run_command(fx: &common::Fixture, root: &tempfile::TempDir, port: u16) -> Command {
    std::fs::write(
        fx.main.join(".grove.toml"),
        format!("version = 1\n[test_mongo]\nimage = 'mongo:8.0.20'\nport = {port}"),
    )
    .unwrap();
    let mut command = Command::new(binary());
    command
        .current_dir(&fx.main)
        .env("GROVE_STATE_DIR", root.path().join("state"))
        .env("GROVE_DOCKER", root.path().join("docker"));
    command
}
fn wait_file(path: &std::path::Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "{} missing",
            path.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn cancellation_stops_the_child_and_external_allocation_is_rejected() {
    let fx = common::Fixture::new();
    let (root, _, config) = mongo::fixture("ok");
    let ready = root.path().join("ready");
    let mut command = run_command(&fx, &root, config.port);
    let mut supervisor = command
        .args([
            "run",
            "--test-mongo",
            "--",
            "python3",
            "-c",
            r#"
import json, os, pathlib, time
pathlib.Path(os.environ['READY']).write_text(json.dumps(dict(os.environ)))
time.sleep(60)
"#,
        ])
        .env("READY", &ready)
        .spawn()
        .unwrap();
    wait_file(&ready);
    let env: std::collections::BTreeMap<String, String> =
        serde_json::from_slice(&std::fs::read(&ready).unwrap()).unwrap();
    let external = Command::new(binary())
        .args(["test-db", "allocate"])
        .envs(env)
        .output()
        .unwrap();
    assert!(!external.status.success());
    assert!(String::from_utf8_lossy(&external.stderr).contains("process group"));
    unsafe {
        libc::kill(supervisor.id() as i32, libc::SIGTERM);
    }
    assert_eq!(supervisor.wait().unwrap().code(), Some(143));
    let calls = std::fs::read_to_string(root.path().join("calls")).unwrap();
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.contains("dropDatabase"))
            .count(),
        1
    );
}

#[test]
fn failed_cleanup_keeps_the_child_exit_code_and_retries_the_exact_inventory() {
    let fx = common::Fixture::new();
    let (root, _, config) = mongo::fixture("drop-fail");
    let output = run_command(&fx, &root, config.port)
        .args(["run", "--test-mongo", "--", "sh", "-c", "exit 17"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(17));
    let records = root.path().join("state/test-mongo/runs");
    let record = std::fs::read_dir(&records)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "json"))
        .expect("failed cleanup retained");
    let data: serde_json::Value = serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    std::fs::write(root.path().join("mode"), "ok").unwrap();
    let output = run_command(&fx, &root, config.port)
        .args(["run", "--test-mongo", "--", "true"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!record.exists());
    let calls = std::fs::read_to_string(root.path().join("calls")).unwrap();
    let name = data["databases"][0].as_str().unwrap();
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.contains("dropDatabase") && line.contains(name))
            .count(),
        2
    );
}

#[test]
fn killed_supervisor_with_a_live_child_does_not_authorize_cleanup() {
    let fx = common::Fixture::new();
    let (root, _, config) = mongo::fixture("ok");
    let ready = root.path().join("ready");
    let mut supervisor = run_command(&fx, &root, config.port)
        .args([
            "run",
            "--test-mongo",
            "--",
            "python3",
            "-c",
            r#"
import os, pathlib, time
pathlib.Path(os.environ['READY']).write_text(str(os.getpid()))
time.sleep(60)
"#,
        ])
        .env("READY", &ready)
        .spawn()
        .unwrap();
    wait_file(&ready);
    supervisor.kill().unwrap();
    supervisor.wait().unwrap();
    let pid: i32 = std::fs::read_to_string(&ready).unwrap().parse().unwrap();
    let output = run_command(&fx, &root, config.port)
        .args(["run", "--test-mongo", "--", "true"])
        .output()
        .unwrap();
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("live process group"));
    let records = root.path().join("state/test-mongo/runs");
    assert_eq!(
        std::fs::read_dir(records)
            .unwrap()
            .filter(|e| e
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|e| e == "json"))
            .count(),
        1
    );
}

#[test]
fn uncertain_registration_and_foreign_inventory_are_retained() {
    for uncertain in [true, false] {
        let fx = common::Fixture::new();
        let (root, _, config) = mongo::fixture("drop-fail");
        let output = run_command(&fx, &root, config.port)
            .args(["run", "--test-mongo", "--", "true"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let records = root.path().join("state/test-mongo/runs");
        let path = std::fs::read_dir(&records)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "json"))
            .unwrap();
        let mut record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        if uncertain {
            record["pgid"] = serde_json::Value::Null;
        } else {
            record["databases"] = serde_json::json!(["grove_test_another_run"]);
        }
        std::fs::write(&path, record.to_string()).unwrap();
        std::fs::write(root.path().join("mode"), "ok").unwrap();
        let before = std::fs::read_to_string(root.path().join("calls")).unwrap();
        let output = run_command(&fx, &root, config.port)
            .args(["run", "--test-mongo", "--", "true"])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(path.exists());
        let after = std::fs::read_to_string(root.path().join("calls")).unwrap();
        let calls = &after[before.len()..];
        assert_eq!(
            calls
                .lines()
                .filter(|line| line.contains("dropDatabase"))
                .count(),
            1
        );
        assert!(!calls.contains("grove_test_another_run"));
    }
}

#[test]
fn interactive_test_command_can_read_its_foreground_terminal() {
    let fx = common::Fixture::new();
    let (root, _, config) = mongo::fixture("ok");
    let _ = run_command(&fx, &root, config.port);
    let output = Command::new("python3").current_dir(&fx.main)
        .env("GROVE_STATE_DIR", root.path().join("state"))
        .env("GROVE_DOCKER", root.path().join("docker"))
        .env("GROVE_BINARY", binary())
        .args(["-c", r#"
import os, pty, select, signal, time
pid, fd = pty.fork()
if pid == 0:
    binary = os.environ['GROVE_BINARY']
    os.execv(binary, [binary, 'run', '--test-mongo', '--', 'python3', '-c', "print('READY', flush=True); print('GOT=' + input(), flush=True)"])
data = b''
sent = False
deadline = time.monotonic() + 12
try:
    while time.monotonic() < deadline:
        if select.select([fd], [], [], .1)[0]:
            try: chunk = os.read(fd, 4096)
            except OSError: break
            if not chunk: break
            data += chunk
        if b'READY' in data and not sent:
            os.write(fd, b'hello\n'); sent = True
        if b'GOT=hello' in data: break
finally:
    if b'GOT=hello' not in data: os.kill(pid, signal.SIGTERM)
    os.waitpid(pid, 0)
    os.close(fd)
print(data.decode())
assert b'GOT=hello' in data, 'child could not read foreground terminal'
"#]).output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
