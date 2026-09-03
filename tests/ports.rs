use grove::ports;
use std::net::TcpListener;

fn names() -> Vec<String> {
    vec!["frontend".into(), "backend".into()]
}

// Each test uses its own repo key so the blocks they probe never overlap. Allocation
// reads live machine state, so two tests racing on one block is a test bug, not a
// finding — stability across a restart is the registry's job, and is tested there.

#[test]
fn allocation_is_stable_while_the_block_stays_free() {
    let a = ports::allocate("repo-stable", "feat_search", &names()).expect("allocate");
    let b = ports::allocate("repo-stable", "feat_search", &names()).expect("allocate");

    assert_eq!(a, b);
}

#[test]
fn ports_land_far_from_the_defaults_a_stale_config_falls_back_to() {
    let a = ports::allocate("repo-range", "feat_search", &names()).expect("allocate");

    for (name, port) in &a {
        assert!(
            ports::RANGE.contains(port),
            "{name} got {port}, outside {:?}",
            ports::RANGE
        );
    }
}

#[test]
fn two_worktrees_of_one_repo_get_different_ports() {
    let a = ports::allocate("repo-distinct", "fix_login", &names()).expect("allocate");
    let b = ports::allocate("repo-distinct", "feat_search", &names()).expect("allocate");

    assert_ne!(a["frontend"], b["frontend"]);
    assert_ne!(a["backend"], b["backend"]);
    assert_ne!(
        a["frontend"], b["backend"],
        "blocks must not overlap either"
    );
}

#[test]
fn an_occupied_port_moves_the_whole_block() {
    let names = names();
    let first = ports::allocate("repo-occupied", "t1", &names).expect("allocate");

    // Hold every port it wanted, the way another process on this machine would.
    let _held: Vec<TcpListener> = first
        .values()
        .map(|p| TcpListener::bind(("127.0.0.1", *p)).expect("hold port"))
        .collect();

    let second = ports::allocate("repo-occupied", "t1", &names).expect("allocate");

    for (name, port) in &second {
        assert!(
            !first.values().any(|p| p == port),
            "{name} was handed the occupied port {port}"
        );
    }
}

#[test]
fn a_repo_needing_more_ports_than_a_block_holds_says_so() {
    let too_many: Vec<String> = (0..64).map(|i| format!("svc{i}")).collect();

    let err = ports::allocate("repo-toomany", "t1", &too_many).expect_err("must refuse");

    assert!(format!("{err:#}").contains("block"), "{err:#}");
}

/// One machine, many registries — a test suite, or two state dirs — share one port space,
/// and the allocator's bind check only sees what is already listening. A range each
/// keeps them apart.
#[test]
fn allocation_stays_inside_the_range_it_is_given() {
    let names = vec!["web".to_string(), "api".to_string()];
    let got = ports::allocate_within(
        20500..20600,
        "repo-sliced",
        "feat_search",
        &names,
        &std::collections::HashSet::new(),
    )
    .expect("allocate");
    for port in got.values() {
        assert!(
            (20500..20600).contains(port),
            "{port} is outside 20500..20600"
        );
    }
}

#[test]
fn a_range_is_written_as_low_dash_high() {
    assert_eq!(ports::parse_range("20500-20600"), Some(20500..20600));
    assert_eq!(
        ports::parse_range("20600-20500"),
        None,
        "empty ranges are refused"
    );
    assert_eq!(ports::parse_range("all of them"), None);
}
