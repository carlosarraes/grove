use assert_cmd::Command;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

const EMPTY_SOCKETS: &str =
    "  sl local_address rem_address st\n  sl local_address remote_address st\n";

fn id(name: &str) -> String {
    let hex: String = name.bytes().map(|byte| format!("{byte:02x}")).collect();
    format!("{hex:0<64}")
}

fn container(name: &str) -> Value {
    json!({"id": id(name), "name": format!("/{name}"), "running": true,
        "started": "2020-01-01T00:00:00.000000000Z", "image": "mongo:7", "label": "true",
        "ports": {"27017/tcp": [{"HostIp":"127.0.0.1", "HostPort":"33950"}]}})
}

fn recent_start() -> String {
    let out = std::process::Command::new("python3").args(["-c",
        "from datetime import datetime,timezone,timedelta; print((datetime.now(timezone.utc)-timedelta(minutes=30)).isoformat().replace('+00:00','Z'))"])
        .output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

struct Rig(TempDir);
impl Rig {
    fn new(containers: Vec<Value>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("containers.json"),
            json!(containers).to_string(),
        )
        .unwrap();
        std::fs::write(dir.path().join("sockets"), EMPTY_SOCKETS).unwrap();
        for (name, body) in [
            (
                "docker",
                r#"#!/usr/bin/env python3
import json, pathlib, sys
root=pathlib.Path(__file__).parent
args=sys.argv[1:]
rows=json.loads((root/'containers.json').read_text())
if args[0]=='ps':
    print('\n'.join(row['id'] for row in rows))
elif args[0]=='inspect':
    row=next(row for row in rows if row['id']==args[-1])
    marker=root/('seen-'+row['id'])
    if marker.exists() and (root/'change-state').exists(): row['started']='2999-01-01T00:00:00Z'
    marker.touch()
    print(json.dumps(row))
elif args[0]=='exec':
    counter=root/('probes-'+args[1])
    count=int(counter.read_text())+1 if counter.exists() else 1
    counter.write_text(str(count))
    if count==2 and (root/'change-after-probe').exists():
        for row in rows:
            if row['id']==args[1]: row['started']='2999-01-01T00:00:00Z'
        (root/'containers.json').write_text(json.dumps(rows))
    if (root/('fail-exec-'+args[1])).exists(): sys.exit(1)
    source=root/('sockets-'+args[1])
    if not source.exists(): source=root/'sockets'
    print(source.read_text(),end='')
elif args[0]=='rm':
    assert args[1]=='-f' and len(args)==3
    with (root/'removed').open('a') as out: out.write(args[2]+'\n')
else: sys.exit(91)
"#,
            ),
            (
                "lsof",
                r#"#!/usr/bin/env python3
import pathlib, sys
root=pathlib.Path(__file__).parent
assert any(p in sys.argv for p in ['-iTCP:33950','-iTCP:33951']) and '-sTCP:ESTABLISHED' in sys.argv
if (root/'host-large').exists(): print('x'*1048577);sys.exit(0)
if (root/'host-second-port').exists() and '-iTCP:33951' in sys.argv:
    print('p123\nn127.0.0.1:50000->127.0.0.1:33951');sys.exit(0)
if (root/'host-detached').exists():
    import os, time
    if os.fork(): sys.exit(0)
    os.setsid()
    (root/'detached-pid').write_text(str(os.getpid()))
    time.sleep(60)
if (root/'host-hung').exists():
    import os, time
    (root/'probe-pid').write_text(str(os.getpid()))
    time.sleep(60)
if (root/'host-after-first').exists():
    seen=root/'host-seen'
    if seen.exists(): print('p123\nn127.0.0.1:50000->127.0.0.1:33950');sys.exit(0)
    seen.touch()
if (root/'host-error').exists():
    print('cannot inspect host sockets',file=sys.stderr);sys.exit(1)
if (root/'host-connected').exists():
    print('p123\nn127.0.0.1:50000->127.0.0.1:33950');sys.exit(0)
sys.exit(1)
"#,
            ),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self(dir)
    }
    fn run(&self, remove: bool) -> assert_cmd::assert::Assert {
        let mut paths = vec![self.0.path().to_path_buf()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let mut cmd = Command::cargo_bin("grove").unwrap();
        cmd.current_dir(self.0.path())
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("GROVE_DOCKER", self.0.path().join("docker"))
            .arg("test-containers");
        if remove {
            cmd.arg("--remove-stale");
        }
        cmd.assert()
    }
}

#[test]
fn test_container_cleanup_reports_by_default_without_removing_anything() {
    let rig = Rig::new(vec![container("old")]);
    let output = rig.run(false).success().get_output().clone();
    assert!(String::from_utf8_lossy(&output.stdout).contains("candidate old"));
    assert!(!rig.0.path().join("removed").exists());
}

#[test]
fn test_container_cleanup_removes_only_old_labelled_disconnected_mongo() {
    let mut rows = vec![container("eligible")];
    for (name, key, value) in [
        ("unlabelled", "label", json!("")),
        ("postgres", "image", json!("postgres:17")),
        ("stopped", "running", json!(false)),
        ("young", "started", json!(recent_start())),
        ("future", "started", json!("2999-01-01T00:00:00Z")),
        ("invalid_calendar", "started", json!("2020-02-31T00:00:00Z")),
        ("invalid_age", "started", json!("invalid")),
        ("no_ports", "ports", json!({})),
    ] {
        let mut row = container(name);
        row[key] = value;
        rows.push(row);
    }
    rows.extend([
        container("grove-mongo"),
        container("connected"),
        container("connected_v6"),
        container("exec_failure"),
        container("unknown_sockets"),
        container("other_port"),
    ]);
    let mut multi = container("multi_port");
    multi["ports"]["27017/tcp"]
        .as_array_mut()
        .unwrap()
        .push(json!({"HostIp":"::", "HostPort":"33951"}));
    rows.push(multi);
    let rig = Rig::new(rows);
    std::fs::write(rig.0.path().join("host-second-port"), "").unwrap();
    std::fs::write(
        rig.0.path().join(format!("sockets-{}", id("connected"))),
        format!("{EMPTY_SOCKETS} 0: 0100007F:6989 0100007F:9999 01\n"),
    )
    .unwrap();
    std::fs::write(rig.0.path().join(format!("sockets-{}", id("connected_v6"))),
        format!("{EMPTY_SOCKETS} 0: 00000000000000000000000001000000:6989 00000000000000000000000001000000:9999 01\n")).unwrap();
    std::fs::write(
        rig.0
            .path()
            .join(format!("fail-exec-{}", id("exec_failure"))),
        "",
    )
    .unwrap();
    std::fs::write(
        rig.0
            .path()
            .join(format!("sockets-{}", id("unknown_sockets"))),
        "garbled\n",
    )
    .unwrap();
    std::fs::write(
        rig.0.path().join(format!("sockets-{}", id("other_port"))),
        format!("{EMPTY_SOCKETS} 0: 0100007F:6981 0100007F:9999 01\n"),
    )
    .unwrap();
    rig.run(true).success();
    assert_eq!(
        std::fs::read_to_string(rig.0.path().join("removed")).unwrap(),
        format!("{}\n{}\n", id("eligible"), id("other_port"))
    );
}

#[test]
fn test_container_cleanup_keeps_host_connections_uncertain_probes_and_restarted_candidates() {
    for marker in [
        "host-connected",
        "host-error",
        "host-after-first",
        "change-state",
        "change-after-probe",
    ] {
        let rig = Rig::new(vec![container("old")]);
        std::fs::write(rig.0.path().join(marker), "").unwrap();
        rig.run(true).success();
        assert!(!rig.0.path().join("removed").exists(), "{marker}");
    }
}

#[test]
fn test_container_cleanup_times_out_an_unknown_host_probe_without_removal() {
    let rig = Rig::new(vec![container("old")]);
    std::fs::write(rig.0.path().join("host-hung"), "").unwrap();
    let started = std::time::Instant::now();
    rig.run(true).success();
    assert!(started.elapsed() < std::time::Duration::from_secs(15));
    assert!(!rig.0.path().join("removed").exists());
    let pid: libc::pid_t = std::fs::read_to_string(rig.0.path().join("probe-pid"))
        .unwrap()
        .parse()
        .unwrap();
    // SAFETY: signal zero observes the fixture probe and sends it no signal.
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "timed-out probe survived"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

#[test]
fn test_container_cleanup_bounds_pipes_held_by_a_detached_probe_descendant() {
    let rig = Rig::new(vec![container("old")]);
    std::fs::write(rig.0.path().join("host-detached"), "").unwrap();
    let started = std::time::Instant::now();
    rig.run(true).success();
    assert!(started.elapsed() < std::time::Duration::from_secs(15));
    assert!(!rig.0.path().join("removed").exists());
}

impl Drop for Rig {
    fn drop(&mut self) {
        if let Ok(text) = std::fs::read_to_string(self.0.path().join("detached-pid"))
            && let Ok(pid) = text.parse::<libc::pid_t>()
        {
            // SAFETY: this PID belongs to the detached fixture child recorded above.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
    }
}

#[test]
fn test_container_cleanup_rejects_oversized_probe_output() {
    let rig = Rig::new(vec![container("old")]);
    std::fs::write(rig.0.path().join("host-large"), "").unwrap();
    let output = rig.run(true).success().get_output().clone();
    assert!(String::from_utf8_lossy(&output.stdout).contains("output exceeded its limit"));
    assert!(!rig.0.path().join("removed").exists());
}
