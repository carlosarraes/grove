use grove::supervise;
use std::collections::BTreeMap;
use std::io::Write;
use std::net::TcpListener;
use std::time::Duration;
use tempfile::TempDir;

fn no_env() -> BTreeMap<String, String> {
    BTreeMap::new()
}

fn settle() {
    std::thread::sleep(Duration::from_millis(300));
}

#[test]
fn captures_stdout_and_stderr_into_the_services_log() {
    let dir = TempDir::new().expect("tempdir");
    let log = dir.path().join("backend.log");

    let handle = supervise::spawn(
        "echo to-stdout; echo to-stderr >&2",
        dir.path(),
        &no_env(),
        &log,
    )
    .expect("spawn");
    settle();

    let body = std::fs::read_to_string(&log).expect("read log");
    assert!(body.contains("to-stdout"), "{body}");
    assert!(
        body.contains("to-stderr"),
        "a service that dies writes to stderr; losing it costs an agent the diagnosis: {body}"
    );
    let _ = supervise::stop(&handle);
}

#[test]
fn passes_the_instance_environment_to_the_service() {
    let dir = TempDir::new().expect("tempdir");
    let log = dir.path().join("backend.log");
    let env = BTreeMap::from([("GROVE_TEST_PORT".to_string(), "24311".to_string())]);

    let handle =
        supervise::spawn("echo port=$GROVE_TEST_PORT", dir.path(), &env, &log).expect("spawn");
    settle();

    let body = std::fs::read_to_string(&log).expect("read log");
    assert!(body.contains("port=24311"), "{body}");
    let _ = supervise::stop(&handle);
}

#[test]
fn runs_the_service_in_its_configured_directory() {
    let dir = TempDir::new().expect("tempdir");
    let backend = dir.path().join("backend");
    std::fs::create_dir(&backend).expect("mkdir");
    std::fs::write(backend.join("marker"), "").expect("marker");
    let log = dir.path().join("backend.log");

    let handle = supervise::spawn("ls", &backend, &no_env(), &log).expect("spawn");
    settle();

    let body = std::fs::read_to_string(&log).expect("read log");
    assert!(body.contains("marker"), "{body}");
    let _ = supervise::stop(&handle);
}

/// `npm run dev` is a shell that spawns Vite. Killing only the shell leaves Vite holding
/// the port, and the next `grove up` cannot bind it.
#[test]
fn stopping_a_service_kills_its_children_too() {
    let dir = TempDir::new().expect("tempdir");
    let log = dir.path().join("backend.log");
    let marker = dir.path().join("grandchild-survived");

    let handle = supervise::spawn(
        &format!("sh -c 'sleep 1; touch {}' & wait", marker.display()),
        dir.path(),
        &no_env(),
        &log,
    )
    .expect("spawn");

    supervise::stop(&handle).expect("stop");
    std::thread::sleep(Duration::from_millis(1600));

    assert!(
        !marker.exists(),
        "a grandchild outlived the process group and would still hold its port"
    );
}

#[test]
fn a_running_service_reports_alive_and_a_stopped_one_does_not() {
    let dir = TempDir::new().expect("tempdir");
    let log = dir.path().join("backend.log");

    let handle = supervise::spawn("sleep 30", dir.path(), &no_env(), &log).expect("spawn");
    settle();
    assert!(supervise::is_alive(&handle), "should be running");

    supervise::stop(&handle).expect("stop");
    settle();
    assert!(!supervise::is_alive(&handle), "should be stopped");
}

/// A stand-in service answering every request with `status`.
///
/// It reads the request before replying, and keeps listening rather than serving once.
/// Both matter on macOS: closing a socket with the request still unread sends an RST, and
/// the client sees a connection error where a response was written.
fn serving(status: &str) -> u16 {
    use std::io::{BufRead, BufReader};

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let response = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|n| n > 0) {
                if line == "\r\n" || line == "\n" {
                    break;
                }
                line.clear();
            }
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    port
}

#[test]
fn waiting_for_ready_returns_once_the_service_answers() {
    let port = serving("200 OK");

    supervise::wait_ready(
        &format!("http://127.0.0.1:{port}/openapi.json"),
        Duration::from_secs(5),
    )
    .expect("should become ready");
}

/// mondrio's `/health` reports degraded by design, which is why its readiness probe points
/// at `/openapi.json`. Any HTTP answer means the process is up; the status is not ours to
/// judge.
#[test]
fn an_error_status_still_counts_as_answering() {
    let port = serving("503 Service Unavailable");

    supervise::wait_ready(
        &format!("http://127.0.0.1:{port}/health"),
        Duration::from_secs(5),
    )
    .expect("503 is still an answer");
}

#[test]
fn waiting_for_ready_gives_up_and_names_the_url() {
    // Port 1 is reserved and nothing will ever answer there.
    let err = supervise::wait_ready(
        "http://127.0.0.1:1/openapi.json",
        Duration::from_millis(400),
    )
    .expect_err("must time out");

    let msg = format!("{err:#}");
    assert!(msg.contains("127.0.0.1:1"), "should name the url: {msg}");
}
