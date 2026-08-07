use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

/// A real git repository with a real commit, so worktree resolution is exercised
/// against git's own bookkeeping rather than a stand-in.
pub struct Fixture {
    _dir: TempDir,
    pub main: PathBuf,
}

impl Fixture {
    pub fn new() -> Self {
        let dir = TempDir::new().expect("tempdir");
        // macOS puts temp dirs behind a /var -> /private/var symlink; canonicalize so
        // path comparisons in tests match what resolve() returns.
        let root = dir.path().canonicalize().expect("canonicalize tempdir");
        let main = root.join("repo");
        std::fs::create_dir(&main).expect("mkdir repo");

        git(&main, &["init", "--initial-branch=main"]);
        git(&main, &["config", "user.email", "test@example.com"]);
        git(&main, &["config", "user.name", "Test"]);
        std::fs::write(main.join("README.md"), "fixture\n").expect("write README");
        git(&main, &["add", "."]);
        git(&main, &["commit", "-m", "initial"]);

        Fixture { _dir: dir, main }
    }

    /// Add a linked worktree as a sibling of the repo, matching the
    /// `<repo-parent>/worktrees/<slug>` convention treeish expects.
    pub fn add_worktree(&self, slug: &str) -> PathBuf {
        let path = self.main.parent().unwrap().join("worktrees").join(slug);
        git(
            &self.main,
            &[
                "worktree",
                "add",
                "-b",
                slug,
                path.to_str().unwrap(),
                "main",
            ],
        );
        path
    }
}

pub fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}
