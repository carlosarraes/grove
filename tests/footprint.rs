mod common;

use common::Fixture;
use std::path::Path;

const MIB: u64 = 1 << 20;

fn blob(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
    std::fs::write(path, vec![0u8; MIB as usize]).expect("write blob");
}

/// The figure has to be what `rm -rf` would give back: ignored files only, each block
/// once however many names it has, and nothing from a venv that uv linked out of its
/// cache.
#[test]
fn measure_counts_ignored_blocks_once_and_skips_venvs() {
    let fx = Fixture::new();
    std::fs::write(fx.main.join(".gitignore"), "node_modules/\n.venv/\n").expect("gitignore");
    common::git(&fx.main, &["add", ".gitignore"]);
    common::git(&fx.main, &["commit", "-m", "ignore deps"]);
    let wt = &fx.add_worktree("feat_search");

    blob(&wt.join("node_modules/blob"));
    std::fs::hard_link(wt.join("node_modules/blob"), wt.join("node_modules/again"))
        .expect("hardlink");
    blob(&wt.join("untracked-but-not-ignored"));
    blob(&wt.join(".venv/lib/big"));
    std::fs::write(wt.join(".venv/pyvenv.cfg"), "home = /usr/bin\n").expect("pyvenv.cfg");

    let bytes = grove::footprint::measure(wt).expect("git can list the ignored tree");
    assert!(
        (MIB..MIB + 64 * 1024).contains(&bytes),
        "expected about one blob's worth, got {bytes}"
    );
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
