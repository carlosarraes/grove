mod common;

use assert_cmd::Command;
use common::{Fixture, git};
use std::path::Path;
use std::process::{Child, Command as Process};

fn free(cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::cargo_bin("grove")
        .expect("binary")
        .current_dir(cwd)
        .arg("free")
        .args(args)
        .output()
        .expect("run grove free")
}

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Whether `git worktree add` can now take the branch, which is what the PM bots care about.
fn can_add_worktree(fx: &Fixture, branch: &str) -> bool {
    let target = fx.main.parent().unwrap().join("probe").join(branch);
    let ok = Process::new("git")
        .current_dir(&fx.main)
        .args(["worktree", "add"])
        .arg(&target)
        .arg(branch)
        .output()
        .expect("git worktree add")
        .status
        .success();
    if ok {
        git(&fx.main, &["worktree", "remove", target.to_str().unwrap()]);
    }
    ok
}

struct Sleeper(Child);

impl Sleeper {
    fn in_dir(dir: &Path) -> Self {
        Sleeper(
            Process::new("sleep")
                .arg("60")
                .current_dir(dir)
                .spawn()
                .expect("spawn sleep"),
        )
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn frees_a_clean_worktree_and_keeps_its_folder_and_branch() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("feat-x");
    assert!(
        !can_add_worktree(&fx, "feat-x"),
        "precondition: branch held"
    );

    let out = free(&fx.main, &["feat-x"]);

    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), format!("freed {}\n", wt.display()));
    assert!(wt.is_dir(), "the worktree folder stays");
    assert_eq!(
        git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "HEAD"
    );
    git(&fx.main, &["rev-parse", "--verify", "refs/heads/feat-x"]);
    assert!(can_add_worktree(&fx, "feat-x"));
}

#[test]
fn keeps_a_dirty_worktree() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("feat-x");
    std::fs::write(wt.join("scratch.txt"), "wip\n").unwrap();

    let out = free(&fx.main, &["feat-x"]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stdout(&out), format!("kept {}: dirty\n", wt.display()));
    assert!(!can_add_worktree(&fx, "feat-x"));
}

#[test]
fn keeps_a_locked_worktree() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("feat-x");
    git(&fx.main, &["worktree", "lock", wt.to_str().unwrap()]);

    let out = free(&fx.main, &["feat-x"]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stdout(&out), format!("kept {}: locked\n", wt.display()));
    assert!(!can_add_worktree(&fx, "feat-x"));
}

#[test]
fn keeps_a_worktree_a_process_is_working_in() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("feat-x");
    let sub = wt.join("src");
    std::fs::create_dir(&sub).unwrap();
    let sleeper = Sleeper::in_dir(&sub);

    let out = free(&fx.main, &["feat-x"]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stdout(&out),
        format!("kept {}: in use by pid {}\n", wt.display(), sleeper.0.id())
    );
    assert!(!can_add_worktree(&fx, "feat-x"));
}

#[test]
fn succeeds_quietly_when_nothing_holds_the_branch() {
    let fx = Fixture::new();
    git(&fx.main, &["branch", "spare"]);

    for branch in ["spare", "no-such-branch"] {
        let out = free(&fx.main, &[branch]);
        assert_eq!(out.status.code(), Some(0), "{branch}");
        assert_eq!(stdout(&out), "", "{branch}");
    }
}

#[test]
fn dry_run_reports_without_detaching() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("feat-x");

    let out = free(&fx.main, &["feat-x", "--dry-run"]);

    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), format!("would free {}\n", wt.display()));
    assert_eq!(
        git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "feat-x"
    );
    assert!(!can_add_worktree(&fx, "feat-x"));
}

#[test]
fn dry_run_still_exits_1_when_a_worktree_would_be_kept() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("feat-x");
    std::fs::write(wt.join("scratch.txt"), "wip\n").unwrap();

    let out = free(&fx.main, &["feat-x", "--dry-run"]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stdout(&out), format!("kept {}: dirty\n", wt.display()));
}

#[test]
fn never_detaches_the_worktree_it_runs_in() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("feat-x");

    let out = free(&wt, &["feat-x"]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stdout(&out),
        format!("kept {}: current worktree\n", wt.display())
    );
    assert_eq!(
        git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "feat-x"
    );
}

#[test]
fn never_detaches_the_main_checkout() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("feat-x");

    let out = free(&wt, &["main"]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stdout(&out),
        format!("kept {}: main checkout\n", fx.main.display())
    );
    assert_eq!(
        git(&fx.main, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "main"
    );
}

#[test]
fn an_empty_branch_name_is_a_usage_error() {
    let fx = Fixture::new();

    let out = free(&fx.main, &[""]);

    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("branch"));
}

#[test]
fn outside_a_git_repo_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();

    let out = free(dir.path(), &["feat-x"]);

    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("git"));
}
