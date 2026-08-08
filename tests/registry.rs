mod common;

use common::Fixture;
use grove::{registry::Registry, resolve};
use tempfile::TempDir;

fn names() -> Vec<String> {
    vec!["frontend".into(), "backend".into()]
}

struct Harness {
    _state: TempDir,
    registry: Registry,
    fx: Fixture,
}

impl Harness {
    fn new() -> Self {
        let state = TempDir::new().expect("tempdir");
        let registry = Registry::at(state.path().join("registry.json"));
        Harness {
            _state: state,
            registry,
            fx: Fixture::new(),
        }
    }

    fn resolved(&self, slug: &str) -> resolve::Resolved {
        resolve::resolve(&self.fx.add_worktree(slug)).expect("resolve")
    }
}

/// The guarantee `ports::allocate` cannot make on its own: an instance keeps its ports
/// across restarts, so a URL an agent wrote down an hour ago still works.
#[test]
fn an_instance_keeps_its_ports_across_restarts() {
    let h = Harness::new();
    let r = h.resolved("mon_2695");

    let first = h.registry.reserve(&r, &names()).expect("reserve");
    let second = h.registry.reserve(&r, &names()).expect("reserve again");

    assert_eq!(first.ports, second.ports);
}

#[test]
fn a_second_worktree_never_gets_the_first_worktrees_ports() {
    let h = Harness::new();
    let a = h
        .registry
        .reserve(&h.resolved("mon_2694"), &names())
        .expect("reserve a");
    let b = h
        .registry
        .reserve(&h.resolved("mon_2695"), &names())
        .expect("reserve b");

    for port in b.ports.values() {
        assert!(
            !a.ports.values().any(|p| p == port),
            "{port} was handed to both instances"
        );
    }
}

#[test]
fn reserving_records_the_instance_so_it_can_be_looked_up() {
    let h = Harness::new();
    let r = h.resolved("mon_2695");

    let reserved = h.registry.reserve(&r, &names()).expect("reserve");
    let found = h
        .registry
        .get(&r.worktree)
        .expect("get")
        .expect("the instance must be recorded");

    assert_eq!(found.ports, reserved.ports);
    assert_eq!(found.slug, "mon_2695");
}

#[test]
fn an_unknown_worktree_is_simply_absent() {
    let h = Harness::new();

    let found = h.registry.get(&h.fx.main).expect("get");

    assert!(found.is_none());
}

#[test]
fn listing_reports_every_reserved_instance() {
    let h = Harness::new();
    h.registry
        .reserve(&h.resolved("mon_2694"), &names())
        .expect("reserve a");
    h.registry
        .reserve(&h.resolved("mon_2695"), &names())
        .expect("reserve b");

    let mut slugs: Vec<String> = h
        .registry
        .list()
        .expect("list")
        .into_iter()
        .map(|e| e.slug)
        .collect();
    slugs.sort();

    assert_eq!(slugs, ["mon_2694", "mon_2695"]);
}

#[test]
fn releasing_frees_the_ports_for_the_next_instance() {
    let h = Harness::new();
    let a = h.resolved("mon_2694");
    let reserved = h.registry.reserve(&a, &names()).expect("reserve");

    h.registry.release(&a.worktree).expect("release");

    assert!(h.registry.get(&a.worktree).expect("get").is_none());
    // The freed block is available again rather than being leaked for the session.
    let b = h.registry.reserve(&a, &names()).expect("re-reserve");
    assert_eq!(b.ports, reserved.ports);
}

/// 47 worktrees accumulate on a real machine and get deleted without ceremony. A removed
/// worktree must stop holding its ports.
#[test]
fn reaping_releases_the_ports_of_worktrees_that_are_gone() {
    let h = Harness::new();
    let alive = h.resolved("still_here");
    let doomed = h.resolved("deleted_later");
    h.registry.reserve(&alive, &names()).expect("reserve alive");
    h.registry
        .reserve(&doomed, &names())
        .expect("reserve doomed");

    std::fs::remove_dir_all(&doomed.worktree).expect("delete the worktree");
    let reaped = h.registry.reap().expect("reap");

    assert_eq!(reaped.len(), 1);
    assert_eq!(reaped[0].slug, "deleted_later");
    assert!(h.registry.get(&doomed.worktree).expect("get").is_none());
    assert!(
        h.registry.get(&alive.worktree).expect("get").is_some(),
        "reaping must not disturb a live instance"
    );
}

/// Two `grove up` calls racing must not hand out one port twice. The registry file is
/// the shared resource, so the lock lives there.
#[test]
fn concurrent_reservations_never_overlap() {
    let state = TempDir::new().expect("tempdir");
    let path = state.path().join("registry.json");
    let fx = Fixture::new();

    let worktrees: Vec<_> = (0..8)
        .map(|i| resolve::resolve(&fx.add_worktree(&format!("t{i}"))).expect("resolve"))
        .collect();

    let reserved: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = worktrees
            .iter()
            .map(|r| {
                let path = path.clone();
                // A fresh Registry per thread, the way separate processes each open the
                // file themselves — a lock held only within one handle proves nothing.
                s.spawn(move || Registry::at(path).reserve(r, &names()).expect("reserve"))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("thread"))
            .collect()
    });

    let mut all: Vec<u16> = reserved
        .iter()
        .flat_map(|e| e.ports.values().copied())
        .collect();
    let total = all.len();
    all.sort_unstable();
    all.dedup();

    assert_eq!(
        all.len(),
        total,
        "a port was handed to two instances at once"
    );
}
