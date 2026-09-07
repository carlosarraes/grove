use grove::config::Cache;
use grove::store;
use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use tempfile::TempDir;

fn cache() -> Cache {
    Cache {
        path: "node_modules".into(),
        key: vec!["package-lock.json".into()],
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

/// The key is what makes two worktrees share or not share a tree, so it has to move
/// with everything that changes the tree and nothing else.
#[test]
fn the_key_follows_the_key_files_and_the_setup_command() {
    let dir = TempDir::new().expect("tempdir");
    write(&dir.path().join("package-lock.json"), "lockfile A");

    let a = store::key(dir.path(), &cache(), "npm ci").expect("key");
    let again = store::key(dir.path(), &cache(), "npm ci").expect("key");
    assert_eq!(a, again, "the same inputs must give the same key");
    assert_eq!(a.len(), 16, "{a}");
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");

    write(&dir.path().join("package-lock.json"), "lockfile B");
    let b = store::key(dir.path(), &cache(), "npm ci").expect("key");
    assert_ne!(a, b, "a changed lockfile is a different tree");

    write(&dir.path().join("package-lock.json"), "lockfile A");
    let other_command = store::key(dir.path(), &cache(), "npm install").expect("key");
    assert_ne!(
        a, other_command,
        "a different setup command is a different tree"
    );
}

#[test]
fn a_missing_key_file_is_named() {
    let dir = TempDir::new().expect("tempdir");
    let err = store::key(dir.path(), &cache(), "npm ci").expect_err("no lockfile");
    assert!(format!("{err:#}").contains("package-lock.json"), "{err:#}");
}

/// Ten worktrees make ten sets of names and one set of blocks. Symlinks are recreated
/// rather than followed: `node_modules/.bin` is relative links into the tree itself.
#[test]
fn linking_a_tree_shares_blocks_and_keeps_symlinks() {
    let dir = TempDir::new().expect("tempdir");
    let from = dir.path().join("store/tree");
    write(&from.join("pkg/index.js"), "module.exports = 1;\n");
    write(&from.join("pkg/lib/deep/leaf.js"), "leaf\n");
    std::fs::create_dir_all(from.join(".bin")).expect("mkdir");
    std::os::unix::fs::symlink("../pkg/index.js", from.join(".bin/tool")).expect("symlink");

    let to = dir.path().join("worktree/node_modules");
    let linked = store::link_tree(&from, &to).expect("link");
    assert!(
        linked.hardlinked,
        "same filesystem, so blocks must be shared"
    );

    let original = from.join("pkg/index.js").metadata().expect("meta");
    let link = to.join("pkg/index.js").metadata().expect("meta");
    assert_eq!(link.ino(), original.ino(), "not the same inode");
    assert_eq!(link.nlink(), 2);
    assert_eq!(
        std::fs::read_to_string(to.join("pkg/lib/deep/leaf.js")).expect("read"),
        "leaf\n"
    );
    let bin = to.join(".bin/tool");
    assert!(bin.symlink_metadata().expect("meta").is_symlink());
    assert_eq!(
        std::fs::read_link(&bin).expect("read_link"),
        Path::new("../pkg/index.js")
    );
    assert_eq!(
        std::fs::read_to_string(&bin).expect("through the link"),
        "module.exports = 1;\n"
    );
}

/// Deleting a store entry frees nothing a worktree still names, so GC can never hurt a
/// worktree; what it must not do is delete an entry a registered worktree will link from.
#[test]
fn gc_removes_only_what_nobody_references() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path().join("store");
    write(&root.join("aaaa000000000000/tree/x"), "a");
    write(&root.join("bbbb000000000000/tree/x"), "b");

    let referenced: HashSet<String> = ["aaaa000000000000".to_string()].into_iter().collect();
    let removed = store::gc(&root, &referenced).expect("gc");

    assert_eq!(removed, vec!["bbbb000000000000".to_string()]);
    assert!(root.join("aaaa000000000000/tree/x").exists());
    assert!(!root.join("bbbb000000000000").exists());
}

#[test]
fn gc_on_a_store_that_does_not_exist_yet_removes_nothing() {
    let dir = TempDir::new().expect("tempdir");
    let removed = store::gc(&dir.path().join("never-created"), &HashSet::new()).expect("gc");
    assert!(removed.is_empty());
}
