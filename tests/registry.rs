mod common;

use common::Fixture;
use grove::exposure::Exposure;
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
    let r = h.resolved("feat_search");

    let first = h.registry.reserve(&r, &names()).expect("reserve");
    let second = h.registry.reserve(&r, &names()).expect("reserve again");

    assert_eq!(first.ports, second.ports);
}

#[test]
fn a_second_worktree_never_gets_the_first_worktrees_ports() {
    let h = Harness::new();
    let a = h
        .registry
        .reserve(&h.resolved("fix_login"), &names())
        .expect("reserve a");
    let b = h
        .registry
        .reserve(&h.resolved("feat_search"), &names())
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
    let r = h.resolved("feat_search");

    let reserved = h.registry.reserve(&r, &names()).expect("reserve");
    let found = h
        .registry
        .get(&r.worktree)
        .expect("get")
        .expect("the instance must be recorded");

    assert_eq!(found.ports, reserved.ports);
    assert_eq!(found.slug, "feat_search");
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
        .reserve(&h.resolved("fix_login"), &names())
        .expect("reserve a");
    h.registry
        .reserve(&h.resolved("feat_search"), &names())
        .expect("reserve b");

    let mut slugs: Vec<String> = h
        .registry
        .list()
        .expect("list")
        .into_iter()
        .map(|e| e.slug)
        .collect();
    slugs.sort();

    assert_eq!(slugs, ["feat_search", "fix_login"]);
}

#[test]
fn releasing_frees_the_ports_for_the_next_instance() {
    let h = Harness::new();
    let a = h.resolved("fix_login");
    let reserved = h.registry.reserve(&a, &names()).expect("reserve");

    h.registry.release(&a.worktree).expect("release");

    assert!(h.registry.get(&a.worktree).expect("get").is_none());

    // Re-reserving must succeed rather than trip over a stale entry. Which block it
    // lands on depends on what else on this machine is listening, so asserting exact
    // equality would make this test fail whenever a sibling suite holds the preferred
    // block. Stability without an intervening release is covered separately, and is
    // exact by construction because the recorded entry is returned untouched.
    let b = h.registry.reserve(&a, &names()).expect("re-reserve");
    assert_eq!(b.ports.len(), reserved.ports.len());
    for (name, port) in &b.ports {
        assert!(grove::ports::RANGE.contains(port), "{name} got {port}");
    }
    assert!(h.registry.get(&a.worktree).expect("get").is_some());
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

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}

/// Idle age is what `grove down --idle` acts on, so an entry that predates the field must
/// not be readable as "idle forever" — or a v0.1.9 registry would hand the first sweep a
/// list of every instance on the machine.
#[test]
fn an_entry_written_before_idle_tracking_still_parses() {
    let state = TempDir::new().expect("tempdir");
    let path = state.path().join("registry.json");
    std::fs::write(
        &path,
        r#"{"instances":{"/tmp/old":{"worktree":"/tmp/old","slug":"old",
           "ports":{"web":24310},"services":{},"db_name":null}}}"#,
    )
    .expect("write a registry from before these fields existed");

    let entry = Registry::at(&path)
        .get(std::path::Path::new("/tmp/old"))
        .expect("get")
        .expect("the old entry must still be readable");

    assert_eq!(entry.slug, "old");
    assert_eq!(entry.last_used, None);
    assert_eq!(entry.instance_dir, None);
    assert_eq!(entry.exposure, Exposure::local());
    assert_eq!(entry.idle_seconds(now()), None);
}

#[test]
fn touching_records_that_someone_is_working_here() {
    let h = Harness::new();
    let r = h.resolved("feat_search");
    h.registry.reserve(&r, &names()).expect("reserve");

    h.registry.touch(&r.worktree).expect("touch");

    let entry = h.registry.get(&r.worktree).expect("get").expect("entry");
    let idle = entry
        .idle_seconds(now())
        .expect("a touched instance has an age");
    assert!(idle < 60, "just touched, but reads as {idle}s idle");
}

/// The hazard this exists to prevent. An agent forty minutes into browser-driven QA —
/// clicking dialogs, waiting on autosave, reading DOM — issues no grove commands at all
/// while being maximally busy. On `last_used` alone its box reads as abandoned, and a
/// sibling's `grove down --idle 30m` kills its backend mid-run.
///
/// A backend serving that QA is writing request logs the whole time, so the log is the
/// evidence `last_used` cannot supply.
#[test]
fn an_instance_serving_traffic_is_not_idle_though_no_grove_command_ran() {
    let h = Harness::new();
    let r = h.resolved("feat_search");
    let mut entry = h.registry.reserve(&r, &names()).expect("reserve");

    let logs = TempDir::new().expect("tempdir");
    entry.instance_dir = Some(logs.path().to_path_buf());
    entry.last_used = Some(now() - 3600);
    entry.services.insert(
        "backend".to_string(),
        grove::supervise::Handle {
            pid: 1,
            started_at: now() - 28_800,
        },
    );
    std::fs::write(logs.path().join("backend.log"), "GET /api/quotes 200\n").expect("log");

    let idle = entry.idle_seconds(now()).expect("age");
    assert!(
        idle < 60,
        "an instance whose backend is still logging requests must not read as idle: {idle}s"
    );
}

/// A log belonging to a service that is no longer running says nothing about now.
#[test]
fn a_stale_log_does_not_keep_an_instance_looking_busy() {
    let h = Harness::new();
    let r = h.resolved("feat_search");
    let mut entry = h.registry.reserve(&r, &names()).expect("reserve");

    let logs = TempDir::new().expect("tempdir");
    let log = logs.path().join("backend.log");
    std::fs::write(&log, "old\n").expect("log");
    std::fs::File::options()
        .write(true)
        .open(&log)
        .expect("open")
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(7200))
        .expect("backdate the log");

    entry.instance_dir = Some(logs.path().to_path_buf());
    entry.last_used = Some(now() - 3600);
    entry.services.insert(
        "backend".to_string(),
        grove::supervise::Handle {
            pid: 1,
            started_at: now() - 28_800,
        },
    );

    let idle = entry.idle_seconds(now()).expect("age");
    assert!(
        (3500..3700).contains(&idle),
        "with an older log, the last grove command is the freshest evidence: {idle}s"
    );
}

/// Never sweep what cannot be measured.
#[test]
fn an_instance_with_no_evidence_of_use_has_no_idle_age() {
    let h = Harness::new();
    let r = h.resolved("feat_search");
    let entry = h.registry.reserve(&r, &names()).expect("reserve");

    assert_eq!(entry.idle_seconds(now()), None);
}

/// These ages are read at a glance while deciding what to stop, so they round to the
/// largest unit that still says something useful.
#[test]
fn ages_read_the_way_a_person_says_them() {
    use grove::registry::human_age;
    assert_eq!(human_age(45), "45s");
    assert_eq!(human_age(12 * 60), "12m");
    assert_eq!(human_age(90 * 60), "1h30m");
    assert_eq!(human_age(6 * 3600), "6h");
    assert_eq!(human_age(50 * 3600), "2d2h");
}

/// A command holds the `Entry` it read at startup and writes it back when it starts a
/// service. Doing that verbatim would roll back the touch the same command just made, so
/// every instance would look as idle as it was before anyone worked in it.
#[test]
fn recording_a_stale_entry_does_not_roll_back_the_idle_clock() {
    let h = Harness::new();
    let r = h.resolved("feat_search");
    let read_before_working = h.registry.reserve(&r, &names()).expect("reserve");

    h.registry.touch(&r.worktree).expect("touch");
    h.registry.record(&read_before_working).expect("record");

    let entry = h.registry.get(&r.worktree).expect("get").expect("entry");
    assert!(
        entry.last_used.is_some(),
        "recording an entry read before the touch must not undo it"
    );
}

#[test]
fn exposure_is_updated_through_its_own_locked_registry_operation() {
    let h = Harness::new();
    let r = h.resolved("feat_search");
    h.registry.reserve(&r, &names()).expect("reserve");
    let exposed = Exposure::explicit("dev-mac.local").expect("host");

    h.registry
        .set_exposure(&r.worktree, exposed.clone())
        .expect("set exposure");

    let entry = h.registry.get(&r.worktree).expect("get").expect("entry");
    assert_eq!(entry.exposure, exposed);
}

#[test]
fn recording_a_stale_entry_does_not_roll_back_exposure() {
    let h = Harness::new();
    let r = h.resolved("feat_search");
    let read_while_local = h.registry.reserve(&r, &names()).expect("reserve");
    let exposed = Exposure::explicit("dev-mac.local").expect("host");
    h.registry
        .set_exposure(&r.worktree, exposed.clone())
        .expect("set exposure");

    h.registry.record(&read_while_local).expect("record stale");

    let entry = h.registry.get(&r.worktree).expect("get").expect("entry");
    assert_eq!(
        entry.exposure, exposed,
        "an unrelated stale service/database write must not return the instance to loopback"
    );
}
