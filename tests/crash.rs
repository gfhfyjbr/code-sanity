//! Crash-recovery: SIGKILL the real binary mid-apply and drive the workspace
//! back to a coherent state through `recover`.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

fn write_repo(root: &Path) {
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "fn alpha() -> usize {\n    1\n}\n").unwrap();
    fs::write(root.join("src/b.rs"), "fn beta() -> usize {\n    1\n}\n").unwrap();
}

const PATCH: &str = "\
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,3 @@
 fn alpha() -> usize {
-    1
+    2
 }
--- a/src/b.rs
+++ b/src/b.rs
@@ -1,3 +1,3 @@
 fn beta() -> usize {
-    1
+    2
 }
";

/// Spawn the real binary applying a two-file patch, SIGKILL it after the first
/// file is written (the env hook pauses there), and return the repo.
fn crash_mid_apply() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    write_repo(repo.path());
    code_sanity::index_workspace(repo.path()).unwrap();
    fs::write(repo.path().join("patch.diff"), PATCH).unwrap();

    let bin = assert_cmd::cargo::cargo_bin("code-sanity");
    let mut child = std::process::Command::new(bin)
        .args(["apply-patch", "--patch", "patch.diff"])
        .current_dir(repo.path())
        .env("CODE_SANITY_TEST_SLEEP_AFTER_FIRST_WRITE", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    // Wait until the first real file is written, then kill mid-apply.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let a = fs::read_to_string(repo.path().join("src/a.rs")).unwrap();
        if a.contains("    2\n") {
            break;
        }
        assert!(Instant::now() < deadline, "first write never happened");
        assert!(
            child.try_wait().unwrap().is_none(),
            "apply finished before it could be killed"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    child.kill().unwrap();
    child.wait().unwrap();

    // Torn state: a.rs written, b.rs untouched, journal stuck in `applying`.
    assert!(
        fs::read_to_string(repo.path().join("src/a.rs"))
            .unwrap()
            .contains("    2\n")
    );
    assert!(
        fs::read_to_string(repo.path().join("src/b.rs"))
            .unwrap()
            .contains("    1\n")
    );
    repo
}

#[test]
fn killed_apply_blocks_commands_until_recover_replays_it() {
    let repo = crash_mid_apply();

    // Every mutating command refuses to run on the torn workspace.
    let err = code_sanity::apply_patch_text(repo.path(), PATCH).unwrap_err();
    assert!(err.to_string().contains("recover"), "{err:#}");
    let err = code_sanity::index_workspace(repo.path()).unwrap_err();
    assert!(err.to_string().contains("recover"), "{err:#}");

    // Roll forward: both files reach the target state.
    let report = code_sanity::recover_workspace(repo.path(), false, false).unwrap();
    assert_eq!(report.recovered.len(), 1);
    assert!(report.conflicts.is_empty());
    assert!(
        fs::read_to_string(repo.path().join("src/a.rs"))
            .unwrap()
            .contains("    2\n")
    );
    assert!(
        fs::read_to_string(repo.path().join("src/b.rs"))
            .unwrap()
            .contains("    2\n")
    );
    assert!(code_sanity::verify_workspace(repo.path()).is_ok());
    code_sanity::index_workspace(repo.path()).unwrap();
}

#[test]
fn killed_apply_rolls_back_cleanly() {
    let repo = crash_mid_apply();
    let report = code_sanity::recover_workspace(repo.path(), true, false).unwrap();
    assert_eq!(report.recovered.len(), 1);
    assert!(report.conflicts.is_empty());
    assert!(
        fs::read_to_string(repo.path().join("src/a.rs"))
            .unwrap()
            .contains("    1\n")
    );
    assert!(
        fs::read_to_string(repo.path().join("src/b.rs"))
            .unwrap()
            .contains("    1\n")
    );
    assert!(code_sanity::verify_workspace(repo.path()).is_ok());
}

#[test]
fn recover_refuses_to_clobber_content_changed_after_the_crash() {
    let repo = crash_mid_apply();

    // Someone edited the already-written file after the crash: neither the
    // recorded snapshot nor the target matches any more.
    fs::write(
        repo.path().join("src/a.rs"),
        "fn alpha() -> usize {\n    99\n}\n",
    )
    .unwrap();

    let report = code_sanity::recover_workspace(repo.path(), false, false).unwrap();
    assert_eq!(report.conflicts.len(), 1, "{:?}", report.conflicts);
    // The stale entry still blocks the workspace.
    let err = code_sanity::index_workspace(repo.path()).unwrap_err();
    assert!(err.to_string().contains("recover"), "{err:#}");
    // The edited file was not clobbered.
    assert!(
        fs::read_to_string(repo.path().join("src/a.rs"))
            .unwrap()
            .contains("    99\n")
    );

    // --force overrides and unblocks.
    let report = code_sanity::recover_workspace(repo.path(), false, true).unwrap();
    assert!(report.conflicts.is_empty());
    assert_eq!(report.recovered.len(), 1);
    assert!(
        fs::read_to_string(repo.path().join("src/a.rs"))
            .unwrap()
            .contains("    2\n")
    );
    assert!(code_sanity::verify_workspace(repo.path()).is_ok());
}
