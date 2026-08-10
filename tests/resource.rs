use grove::config;
use grove::resource::{self, Decision};
use std::net::TcpListener;

fn mongo(port: u16) -> config::Resource {
    let toml = format!(
        r#"
version = 1

[[resource]]
name = "mongo"
kind = "docker-shared"
image = "mongo:8.0.23"
args = ["--replSet", "rs0"]
port = {port}
init = "rs.initiate()"
db_name = "app_{{{{ slug }}}}"
"#
    );
    config::parse(&toml)
        .expect("parse")
        .resources
        .pop()
        .expect("one resource")
}

fn a_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

/// The arch box runs rootful Docker; the mac runs Docker under lima and forwards 27017
/// over SSH. "Start a container" is wrong there, so grove asks the port, not the
/// runtime.
#[test]
fn a_reachable_datastore_is_reused_rather_than_started() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();

    match resource::decide(&mongo(port)) {
        Decision::Reuse { port: found } => assert_eq!(found, port),
        Decision::Start(argv) => panic!("would have started a container: {argv:?}"),
    }
}

#[test]
fn an_absent_datastore_is_started_from_its_declared_image() {
    let port = a_free_port();

    let Decision::Start(argv) = resource::decide(&mongo(port)) else {
        panic!("should have decided to start a container");
    };

    let line = argv.join(" ");
    assert!(line.starts_with("run -d"), "{line}");
    assert!(line.contains("mongo:8.0.23"), "{line}");
    assert!(
        line.contains(&format!("{port}:{port}")),
        "must publish the declared port: {line}"
    );
    assert!(
        line.contains("--replSet rs0"),
        "declared args must reach the container command: {line}"
    );
    assert!(
        line.contains("grove-mongo"),
        "the container needs a stable name so a second run reuses it: {line}"
    );
}

/// Container args belong after the image; publishing and naming belong before it. Getting
/// this backwards makes docker treat `--replSet` as its own flag and fail obscurely.
#[test]
fn container_arguments_come_after_the_image() {
    let Decision::Start(argv) = resource::decide(&mongo(a_free_port())) else {
        panic!("should start");
    };

    let image = argv
        .iter()
        .position(|a| a == "mongo:8.0.23")
        .expect("image");
    let repl = argv.iter().position(|a| a == "--replSet").expect("replSet");
    let publish = argv.iter().position(|a| a == "-p").expect("publish");

    assert!(image < repl, "{argv:?}");
    assert!(publish < image, "{argv:?}");
}

#[test]
fn the_instance_database_name_is_reported_for_purging() {
    let command = resource::drop_database_command(&mongo(27017), "app_feat_search");

    let line = command.join(" ");
    assert!(line.contains("app_feat_search"), "{line}");
    assert!(line.contains("dropDatabase"), "{line}");
}

/// `ensure` asks the port rather than the container runtime, and dropping has to work the
/// same way — the datastore is often one grove did not start, whose container has a name
/// grove never chose. Reaching it over the port works whether it is a local container, a
/// VM, or a forwarded socket.
#[test]
fn dropping_a_database_reaches_the_datastore_by_port() {
    let command = resource::drop_database_command(&mongo(27017), "app_feat_search").join(" ");

    assert!(
        command.contains("27017"),
        "must address the port: {command}"
    );
    assert!(command.contains("app_feat_search"), "{command}");
    assert!(command.contains("dropDatabase"), "{command}");
    assert!(
        !command.contains("exec"),
        "a `docker exec` only reaches a container grove started: {command}"
    );
}
