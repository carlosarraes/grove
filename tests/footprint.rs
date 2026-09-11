mod common;

use common::Fixture;
use std::path::Path;

const MIB: u64 = 1 << 20;

fn blob(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
    std::fs::write(path, vec![0u8; MIB as usize]).expect("write blob");
}

/// The figure has to be what `rm -rf` would give back: ignored files only, and only the
/// blocks this worktree holds alone. A block shared with the store, or with uv's cache,
/// survives the worktree's removal, so it is not this worktree's cost.
#[test]
fn measure_counts_only_the_blocks_this_worktree_holds_alone() {
    let fx = Fixture::new();
    std::fs::write(fx.main.join(".gitignore"), "node_modules/\n.venv/\n").expect("gitignore");
    common::git(&fx.main, &["add", ".gitignore"]);
    common::git(&fx.main, &["commit", "-m", "ignore deps"]);
    let wt = &fx.add_worktree("feat_search");

    blob(&wt.join("node_modules/private"));
    let store = fx.main.parent().unwrap().join("store");
    blob(&store.join("shared"));
    std::fs::hard_link(store.join("shared"), wt.join("node_modules/shared")).expect("hardlink");
    blob(&wt.join("untracked-but-not-ignored"));
    blob(&wt.join(".venv/lib/big"));
    std::fs::write(wt.join(".venv/pyvenv.cfg"), "home = /usr/bin\n").expect("pyvenv.cfg");

    let bytes = grove::footprint::measure(wt).expect("git can list the ignored tree");
    assert!(
        (MIB..MIB + 64 * 1024).contains(&bytes),
        "expected the private blob alone, got {bytes}"
    );
}

/// The store's own cost, counted once wherever it is shared from.
#[test]
fn a_tree_is_sized_by_its_blocks() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    blob(&dir.path().join("tree/a/one"));
    blob(&dir.path().join("tree/b/two"));
    let bytes = grove::footprint::tree_size(&dir.path().join("tree"));
    assert!((2 * MIB..2 * MIB + 64 * 1024).contains(&bytes), "{bytes}");
}

#[test]
fn a_worktree_that_is_not_a_repo_has_no_figure() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    assert_eq!(grove::footprint::measure(dir.path()), None);
}

#[test]
fn sizes_read_the_way_du_prints_them() {
    let cases = [
        (0, "0B"),
        (512, "512B"),
        (1536, "1.5K"),
        (350 << 20, "350M"),
        (1_181_116_006, "1.1G"),
    ];
    for (bytes, want) in cases {
        assert_eq!(grove::footprint::human_size(bytes), want, "{bytes}");
    }
}
