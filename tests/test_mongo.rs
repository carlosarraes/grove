use grove::{config, test_mongo::Manager};
#[path = "common/test_mongo.rs"]
mod support;
use support::fixture;
use tempfile::TempDir;

#[test]
fn test_resource_reuses_only_its_owned_pinned_loopback_container() {
    let (root, manager, config) = fixture("ok");
    let first = manager.ensure(&config).unwrap();
    let second = manager.ensure(&config).unwrap();
    assert_eq!(first.id, second.id);
    assert!(
        first
            .uri()
            .starts_with(&format!("mongodb://127.0.0.1:{}/", config.port))
    );
    let calls = std::fs::read_to_string(root.path().join("calls")).unwrap();
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.starts_with("[\"run\""))
            .count(),
        1
    );
    let file = root.path().join("container.json");
    let mut data: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    data["Config"]["Labels"] = serde_json::json!({});
    std::fs::write(&file, data.to_string()).unwrap();
    assert!(
        manager
            .ensure(&config)
            .unwrap_err()
            .to_string()
            .contains("ownership")
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), data.to_string());
}

#[test]
fn test_resource_rejects_wrong_version_and_initialization_failure() {
    for mode in ["wrong-version", "init-error"] {
        let (_root, manager, config) = fixture(mode);
        let error = manager.ensure(&config).unwrap_err();
        assert!(format!("{error:#}").contains(if mode == "wrong-version" {
            "version"
        } else {
            "replica init failed"
        }));
    }
}

#[test]
fn test_resource_config_requires_an_explicit_pin_and_safe_coordinates() {
    for fields in [
        "image = 'mongo:latest'",
        "image = 'mongo:8'",
        "image = 'mongo:8.0.20'\nport = 0",
        "image = 'mongo:8.0.20'\nname = 'development-mongo'",
        "image = 'mongo:8.0.20'\nname = 'grove-test-../mongo'",
    ] {
        assert!(config::parse(&format!("version = 1\n[test_mongo]\n{fields}")).is_err());
    }
}

#[test]
#[ignore = "requires Docker; creates and removes one isolated test Mongo"]
fn real_test_resource_reaches_primary_and_reuses_the_container() {
    use std::process::Command;
    let root = TempDir::new().unwrap();
    let name = format!("grove-test-probe-{}", std::process::id());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let config = config::TestMongo {
        image: "mongo:8.0.20".into(),
        port,
        name,
    };
    let manager = Manager::new(root.path().to_owned(), "docker".into()).unwrap();
    let began = std::time::Instant::now();
    let result = manager.ensure(&config);
    if result.is_err() {
        let _ = Command::new("docker")
            .args(["rm", "-f", &config.name])
            .output();
    }
    let owned = result.unwrap();
    println!("cold resource: {:?}", began.elapsed());
    let began = std::time::Instant::now();
    let reused = manager.ensure(&config);
    println!("warm resource: {:?}", began.elapsed());
    let cleanup = Command::new("docker")
        .args(["rm", "-f", &owned.id])
        .output()
        .unwrap();
    assert!(cleanup.status.success());
    assert_eq!(reused.unwrap().id, owned.id);
}

#[test]
fn test_resource_cleanup_rejects_a_replaced_container_and_development_database() {
    let (root, manager, config) = fixture("ok");
    let owned = manager.ensure(&config).unwrap();
    assert!(
        manager
            .drop_databases(&owned, &["development".into()])
            .is_err()
    );
    let file = root.path().join("container.json");
    let mut data: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    data["Id"] = "replacement".into();
    std::fs::write(&file, data.to_string()).unwrap();
    let before = std::fs::read_to_string(root.path().join("calls")).unwrap();
    assert!(
        manager
            .drop_databases(&owned, &["grove_test_recorded".into()])
            .unwrap_err()
            .to_string()
            .contains("identity")
    );
    let after = std::fs::read_to_string(root.path().join("calls")).unwrap();
    assert!(!after[before.len()..].contains("dropDatabase"));
}

#[test]
fn test_resource_waits_for_mongo_to_accept_connections() {
    let (_root, manager, config) = fixture("starting");
    assert!(manager.ensure(&config).is_ok());
}

#[test]
fn test_resource_serializes_concurrent_cold_initialization() {
    let (root, manager, config) = fixture("ok");
    std::thread::scope(|scope| {
        let first = scope.spawn(|| manager.ensure(&config).unwrap());
        let second = scope.spawn(|| manager.ensure(&config).unwrap());
        assert_eq!(first.join().unwrap().id, second.join().unwrap().id);
    });
    let calls = std::fs::read_to_string(root.path().join("calls")).unwrap();
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.starts_with("[\"run\""))
            .count(),
        1
    );
}

#[test]
fn test_resource_rejects_public_binding_or_different_replica_command() {
    for (field, replacement) in [
        (
            "HostConfig",
            serde_json::json!({"PortBindings":{"27017/tcp":[{"HostIp":"0.0.0.0","HostPort":"27118"}]}}),
        ),
        (
            "Config",
            serde_json::json!({"Image":"mongo:8.0.20","Cmd":["--bind_ip_all"]}),
        ),
    ] {
        let (root, manager, config) = fixture("ok");
        manager.ensure(&config).unwrap();
        let path = root.path().join("container.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let labels = value["Config"]["Labels"].clone();
        value[field] = replacement;
        value["Config"]["Labels"] = labels;
        std::fs::write(&path, value.to_string()).unwrap();
        let error = manager.ensure(&config).unwrap_err().to_string();
        assert!(error.contains(if field == "HostConfig" {
            "loopback"
        } else {
            "replica-set command"
        }));
    }
}
