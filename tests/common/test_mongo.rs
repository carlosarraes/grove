use grove::{config, test_mongo::Manager};
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

pub fn fixture(mode: &str) -> (TempDir, Manager, config::TestMongo) {
    let root = TempDir::new().unwrap();
    let program = root.path().join("docker");
    std::fs::write(&program, r#"#!/usr/bin/env python3
import json, pathlib, sys
root = pathlib.Path(__file__).parent
args = sys.argv[1:]
with (root / 'calls').open('a') as f: f.write(json.dumps(args) + '\n')
mode = (root / 'mode').read_text()
container = root / 'container.json'
if args[0] == 'inspect':
    if not container.exists():
        print('error: no such object', file=sys.stderr); sys.exit(1)
    print(json.dumps([json.loads(container.read_text())]))
elif args[0] == 'run':
    label = args[args.index('--label') + 1].split('=', 1)
    mapping = args[args.index('-p') + 1].split(':')
    data = {'Id': 'owned-id', 'Config': {'Image': 'mongo:8.0.20', 'Labels': dict([label]), 'Cmd': ['--replSet', 'rs0', '--bind_ip_all']}, 'HostConfig': {'PortBindings': {'27017/tcp': [{'HostIp': mapping[0], 'HostPort': mapping[1]}]}}, 'State': {'Running': True}}
    container.write_text(json.dumps(data)); print('owned-id')
elif args[0] == 'exec':
    if mode == 'starting' and not (root / 'attempted').exists():
        (root / 'attempted').touch()
        print('MongoNetworkError: connect ECONNREFUSED 127.0.0.1:27017', file=sys.stderr); sys.exit(1)
    if mode == 'init-error': print('replica init failed', file=sys.stderr); sys.exit(9)
    if mode == 'wrong-version': print(json.dumps({'version':'8.0.23', 'set_name':'rs0', 'primary': True})); sys.exit(0)
    print(json.dumps({'version': '8.0.20', 'set_name':'rs0', 'primary': True}))
else:
    raise RuntimeError(args)
"#).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(root.path().join("mode"), mode).unwrap();
    let mut config =
        config::parse("version = 1\n[test_mongo]\nimage = 'mongo:8.0.20'\nport = 27118")
            .unwrap()
            .test_mongo
            .unwrap();
    config.port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let manager = Manager::new(root.path().join("state"), program.into_os_string()).unwrap();
    (root, manager, config)
}
