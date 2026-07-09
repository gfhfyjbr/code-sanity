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

/// A SIGKILL can land inside an atomic write — after the temp file is created,
/// before the rename. The stranded temp then shows up in `verify`'s mirror
/// sweep as an untracked file (observed as a flaky CI failure of
/// `killed_apply_rolls_back_cleanly` on loaded runners). Recover must sweep
/// such garbage from the state dir and from the real directories the
/// interrupted apply touched.
#[test]
fn recover_sweeps_temp_files_stranded_by_the_crash() {
    let repo = crash_mid_apply();

    // Simulate the kill landing mid-atomic-write in the mirror, the maps dir,
    // and the real repo (same naming as fsutil's temp_path_for).
    let mirror_temp = repo
        .path()
        .join(".code-sanity/mirror/src/.a.rs.code-sanity-tmp-12345-7");
    let maps_temp = repo
        .path()
        .join(".code-sanity/maps/src/.a.rs.map.json.code-sanity-tmp-12345-8");
    let real_temp = repo.path().join("src/.b.rs.code-sanity-tmp-12345-9");
    for temp in [&mirror_temp, &maps_temp, &real_temp] {
        fs::write(temp, "torn half-write").unwrap();
    }

    let report = code_sanity::recover_workspace(repo.path(), true, false).unwrap();
    assert_eq!(report.recovered.len(), 1);
    assert!(report.conflicts.is_empty());
    assert_eq!(report.temp_files_removed, 3);
    for temp in [&mirror_temp, &maps_temp, &real_temp] {
        assert!(!temp.exists(), "stale temp survived: {}", temp.display());
    }
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

#[test]
fn recover_clears_the_inflight_marker() {
    let repo = crash_mid_apply();
    let inflight = repo.path().join(".code-sanity/journal/inflight");
    let markers = || -> usize {
        fs::read_dir(&inflight)
            .map(|entries| entries.count())
            .unwrap_or(0)
    };
    assert_eq!(markers(), 1, "crash must leave exactly one marker");

    code_sanity::recover_workspace(repo.path(), false, false).unwrap();
    assert_eq!(markers(), 0, "terminal write must clear the marker");
    code_sanity::index_workspace(repo.path()).unwrap();
}

#[test]
fn legacy_journal_without_inflight_dir_still_blocks() {
    let repo = crash_mid_apply();
    // Pre-marker workspace shape: journal entries exist, no inflight/ dir.
    fs::remove_dir_all(repo.path().join(".code-sanity/journal/inflight")).unwrap();

    let err = code_sanity::index_workspace(repo.path()).unwrap_err();
    assert!(err.to_string().contains("recover"), "{err:#}");

    code_sanity::recover_workspace(repo.path(), false, false).unwrap();
    code_sanity::index_workspace(repo.path()).unwrap();
    // The one-time upgrade recreated the marker dir for O(1) later checks.
    assert!(repo.path().join(".code-sanity/journal/inflight").is_dir());
}

#[test]
fn corrupt_inflight_entry_blocks_instead_of_quarantining() {
    let repo = crash_mid_apply();
    // The sole record of the in-flight apply is damaged (media error, or a
    // newer binary's schema read by an older one).
    let journal_dir = repo.path().join(".code-sanity/journal");
    let entry_path = fs::read_dir(&journal_dir)
        .unwrap()
        .filter_map(|entry| Some(entry.ok()?.path()))
        .find(|path| path.extension().is_some_and(|ext| ext == "json"))
        .unwrap();
    fs::write(&entry_path, "{ garbage").unwrap();

    let err = code_sanity::index_workspace(repo.path()).unwrap_err();
    assert!(err.to_string().contains("cannot be parsed"), "{err:#}");
    assert!(err.to_string().contains("verify"), "{err:#}");
    // NOT renamed away: quarantining would silently unblock a torn workspace.
    assert!(entry_path.exists(), "corrupt entry was quarantined");

    // verify still works while blocked (shared lock, no journal check)...
    let _ = code_sanity::verify_workspace(repo.path());
    // ...and recover reports the damage instead of half-fixing around it.
    let report = code_sanity::recover_workspace(repo.path(), false, false).unwrap();
    assert!(
        report
            .conflicts
            .iter()
            .any(|conflict| conflict.contains("cannot be parsed")),
        "{:?}",
        report.conflicts
    );

    // The documented manual override: move entry + marker aside -> unblocked.
    fs::rename(&entry_path, entry_path.with_extension("json.aside")).unwrap();
    let marker_dir = journal_dir.join("inflight");
    for marker in fs::read_dir(&marker_dir).unwrap() {
        fs::remove_file(marker.unwrap().path()).unwrap();
    }
    // The torn real file is still torn; sync repairs the mirror afterwards.
    code_sanity::index_workspace(repo.path()).unwrap();
}

#[test]
fn corrupt_terminal_history_never_blocks_the_fast_path() {
    let repo = tempfile::tempdir().unwrap();
    write_repo(repo.path());
    code_sanity::index_workspace(repo.path()).unwrap();
    // A successful apply leaves a terminal entry...
    code_sanity::apply_patch_text(repo.path(), PATCH).unwrap();
    let journal_dir = repo.path().join(".code-sanity/journal");
    let entry_path = fs::read_dir(&journal_dir)
        .unwrap()
        .filter_map(|entry| Some(entry.ok()?.path()))
        .find(|path| path.extension().is_some_and(|ext| ext == "json"))
        .unwrap();
    // ...that later rots. The hot path never parses terminal history, so
    // nothing blocks; only `recover` (a human action) reports it.
    fs::write(&entry_path, "{ rotten").unwrap();
    code_sanity::index_workspace(repo.path()).unwrap();
    assert!(entry_path.exists());
}

#[test]
fn stale_marker_for_terminal_entry_self_heals() {
    let repo = tempfile::tempdir().unwrap();
    write_repo(repo.path());
    code_sanity::index_workspace(repo.path()).unwrap();
    code_sanity::apply_patch_text(repo.path(), PATCH).unwrap();

    // Simulate a crash between the terminal entry write and marker removal.
    let journal_dir = repo.path().join(".code-sanity/journal");
    let entry_id = fs::read_dir(&journal_dir)
        .unwrap()
        .filter_map(|entry| Some(entry.ok()?.path()))
        .find(|path| path.extension().is_some_and(|ext| ext == "json"))
        .map(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .trim_end_matches(".patch.json")
                .to_string()
        })
        .unwrap();
    let marker = journal_dir.join("inflight").join(&entry_id);
    fs::write(&marker, "").unwrap();

    // The entry is authoritative (terminal): the stale marker heals silently.
    code_sanity::index_workspace(repo.path()).unwrap();
    assert!(!marker.exists(), "stale marker survived");
}
