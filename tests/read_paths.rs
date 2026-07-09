//! Read commands must fail cleanly in a directory that was never initialized —
//! and, critically, must not conjure a `.code-sanity/` state dir there (the
//! old behavior: acquiring the shared lock created `.code-sanity/tmp/` in
//! arbitrary directories).

use std::path::Path;

fn assert_no_state_dir(root: &Path) {
    assert!(
        !root.join(".code-sanity").exists(),
        "read path created .code-sanity in an uninitialized directory"
    );
}

#[test]
fn read_sanitized_file_requires_an_initialized_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let err = code_sanity::read_sanitized_file(dir.path(), Path::new("src/lib.rs")).unwrap_err();
    assert!(err.to_string().contains("init"), "{err:#}");
    assert_no_state_dir(dir.path());
}

#[test]
fn search_mirror_requires_an_initialized_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let err = code_sanity::search_mirror(dir.path(), "query", None).unwrap_err();
    assert!(err.to_string().contains("init"), "{err:#}");
    assert_no_state_dir(dir.path());
}

#[test]
fn verify_requires_an_initialized_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let err = code_sanity::verify_workspace(dir.path()).unwrap_err();
    assert!(err.to_string().contains("init"), "{err:#}");
    assert_no_state_dir(dir.path());
}

#[test]
fn semantic_search_requires_an_initialized_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let err = code_sanity::embed::semantic_search(dir.path(), "query", 5).unwrap_err();
    assert!(err.to_string().contains("init"), "{err:#}");
    assert_no_state_dir(dir.path());
}

#[test]
fn cli_search_in_uninitialized_dir_fails_without_creating_state() {
    let dir = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("code-sanity"))
        .arg("--root")
        .arg(dir.path())
        .args(["search", "anything"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("init"), "stderr: {stderr}");
    assert_no_state_dir(dir.path());
}
