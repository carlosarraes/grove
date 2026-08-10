mod common;

use common::Fixture;
use grove::resolve;

#[test]
fn finds_the_main_worktree_from_inside_a_linked_worktree() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("checkout-redesign");

    let r = resolve::resolve(&wt).expect("resolve");

    assert_eq!(r.main_worktree, fx.main);
}

#[test]
fn finds_the_worktree_root_from_a_subdirectory() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("checkout-redesign");
    let sub = wt.join("backend");
    std::fs::create_dir(&sub).expect("mkdir backend");

    let r = resolve::resolve(&sub).expect("resolve");

    assert_eq!(r.worktree, wt);
}

#[test]
fn knows_it_is_in_the_main_worktree() {
    let fx = Fixture::new();

    let r = resolve::resolve(&fx.main).expect("resolve");

    assert!(r.is_main(), "the main checkout must be recognised as main");
}

#[test]
fn knows_it_is_in_a_linked_worktree() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("checkout-redesign");

    let r = resolve::resolve(&wt).expect("resolve");

    assert!(
        !r.is_main(),
        "a linked worktree must not be treated as main"
    );
}

#[test]
fn slug_lowercases_and_underscores_the_worktree_directory_name() {
    let fx = Fixture::new();
    // Real shape: an uppercase ticket prefix with hyphens. The slug becomes a
    // Mongo database name, so it has to come out as [a-z0-9_].
    let wt = fx.add_worktree("PROJ-797-Overage-Scripts");

    let r = resolve::resolve(&wt).expect("resolve");

    assert_eq!(r.slug, "proj_797_overage_scripts");
}

#[test]
fn every_worktree_of_a_repo_shares_one_state_key() {
    let fx = Fixture::new();
    let a = resolve::resolve(&fx.main).expect("resolve main");
    let b = resolve::resolve(&fx.add_worktree("t1")).expect("resolve t1");

    assert_eq!(a.state_key(), b.state_key());
}

#[test]
fn repos_with_the_same_directory_name_get_different_state_keys() {
    // `~/a/checkout` and `~/b/checkout` must not share a registry entry.
    let one = Fixture::new();
    let two = Fixture::new();
    assert_eq!(
        one.main.file_name(),
        two.main.file_name(),
        "fixture precondition: both repos are named the same"
    );

    let a = resolve::resolve(&one.main).expect("resolve one");
    let b = resolve::resolve(&two.main).expect("resolve two");

    assert_ne!(a.state_key(), b.state_key());
}
