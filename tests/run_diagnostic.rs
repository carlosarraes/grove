use grove::run::not_found_diagnostic;
use std::{ffi::OsStr, fs, os::unix::fs::symlink};
use tempfile::TempDir;

#[test]
fn missing_relative_path_names_the_absolute_location() {
    let dir = TempDir::new().unwrap();
    assert_eq!(
        not_found_diagnostic("does/not/exist", dir.path(), dir.path(), None),
        format!(
            "grove run: `does/not/exist` not found (looked in {})",
            dir.path().join("does/not/exist").display()
        )
    );
}

#[test]
fn bare_name_reports_path_lookup() {
    let dir = TempDir::new().unwrap();
    assert_eq!(
        not_found_diagnostic("missing", dir.path(), dir.path(), Some(OsStr::new(""))),
        "grove run: `missing` is not on PATH"
    );
}

#[test]
fn existing_script_names_its_missing_interpreter() {
    let dir = TempDir::new().unwrap();
    let interpreter = dir.path().join("absent-interpreter");
    fs::write(
        dir.path().join("script"),
        format!("#!{} -x\n", interpreter.display()),
    )
    .unwrap();
    assert_eq!(
        not_found_diagnostic("./script", dir.path(), dir.path(), None),
        format!(
            "grove run: `./script` exists but its interpreter `{}` is missing",
            interpreter.display()
        )
    );
}

#[test]
fn dangling_symlink_names_its_target() {
    let dir = TempDir::new().unwrap();
    symlink("absent-target", dir.path().join("script")).unwrap();
    assert_eq!(
        not_found_diagnostic("./script", dir.path(), dir.path(), None),
        "grove run: `./script` is a dangling symlink to absent-target"
    );
}

#[test]
fn missing_nested_binary_suggests_worktree_root_binary() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("node_modules/.bin")).unwrap();
    fs::write(dir.path().join("node_modules/.bin/lefthook"), "").unwrap();
    assert!(
        not_found_diagnostic(
            "frontend/node_modules/.bin/lefthook",
            dir.path(),
            dir.path(),
            None
        )
        .ends_with("; did you mean node_modules/.bin/lefthook?")
    );
}

#[test]
fn path_lookup_inspects_an_existing_script() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("script"), "#!/grove-missing-interpreter\n").unwrap();
    assert_eq!(
        not_found_diagnostic(
            "script",
            dir.path(),
            dir.path(),
            Some(dir.path().as_os_str())
        ),
        "grove run: `script` exists but its interpreter `/grove-missing-interpreter` is missing"
    );
}
