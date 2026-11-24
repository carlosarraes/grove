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

fn listed_main(cwd: &std::path::Path) -> std::path::PathBuf {
    common::git(cwd, &["worktree", "list", "--porcelain"])
        .lines()
        .find_map(|l| l.strip_prefix("worktree "))
        .map(std::path::PathBuf::from)
        .expect("porcelain names a main worktree")
}

#[test]
fn a_separate_git_dir_resolves_to_the_listed_main_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let main = root.join("repo");
    let gitdir = root.join("store.git");
    std::fs::create_dir(&main).unwrap();
    common::git(
        &main,
        &["init", "-q", "--separate-git-dir", gitdir.to_str().unwrap()],
    );
    common::git(
        &main,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "i",
        ],
    );
    let wt = root.join("linked");
    common::git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]);

    let r = resolve::resolve(&wt).expect("resolve");

    assert_eq!(r.main_worktree, listed_main(&wt));
    assert_eq!(r.worktree, wt);
}

#[test]
fn a_bare_repo_cloned_into_dot_git_resolves_to_the_listed_main_worktree() {
    let fx = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let project = root.join("project");
    std::fs::create_dir(&project).unwrap();
    common::git(
        &root,
        &[
            "clone",
            "-q",
            "--bare",
            fx.main.to_str().unwrap(),
            project.join(".git").to_str().unwrap(),
        ],
    );
    let wt = project.join("linked");
    common::git(
        &project.join(".git"),
        &["worktree", "add", "-q", wt.to_str().unwrap()],
    );

    let r = resolve::resolve(&wt).expect("resolve");

    assert_eq!(r.main_worktree, listed_main(&wt));
}

#[test]
fn a_relocated_core_worktree_resolves_to_the_listed_main_worktree() {
    let fx = Fixture::new();
    let elsewhere = fx.main.parent().unwrap().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    common::git(
        &fx.main,
        &["config", "core.worktree", elsewhere.to_str().unwrap()],
    );
    let wt = fx.add_worktree("t1");

    let r = resolve::resolve(&wt).expect("resolve");

    assert_eq!(r.main_worktree, listed_main(&wt));
}
