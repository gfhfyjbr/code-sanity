//! Single-file sync/index must never read or write outside the repo: hooks
//! feed `sync --path` relpaths computed from the editor cwd, so `../…` shapes
//! arrive routinely, and a DB poisoned by a pre-validation version must not
//! direct the stale sweep at files outside the mirror.

use code_sanity::config::Layout;
use code_sanity::index::{index_single_file, sync_single_file};
use std::fs;
use std::path::Path;

/// outer/ contains victim.env and repo/; the workspace root is repo/.
fn outer_with_repo() -> (tempfile::TempDir, std::path::PathBuf) {
    let outer = tempfile::tempdir().unwrap();
    fs::write(outer.path().join("victim.env"), "SECRET=1\n").unwrap();
    let repo = outer.path().join("repo");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(repo.join("src/a.rs"), "fn alpha() {}\n").unwrap();
    code_sanity::index_workspace(&repo).unwrap();
    (outer, repo)
}

#[test]
fn sync_path_outside_repo_is_a_clean_skip() {
    let (outer, repo) = outer_with_repo();
    let report = sync_single_file(&repo, Path::new("../victim.env")).unwrap();
    assert_eq!(report.skipped, 1);
    assert_eq!(report.indexed + report.unchanged + report.removed, 0);

    // Nothing was read, mirrored, tracked, or deleted.
    assert!(outer.path().join("victim.env").exists());
    let layout = Layout::new(&repo);
    let conn = code_sanity::db::connect(&layout).unwrap();
    for tracked in code_sanity::db::tracked_files(&conn).unwrap() {
        assert!(!tracked.contains(".."), "poisoned rel tracked: {tracked}");
        assert!(
            !tracked.contains("victim"),
            "outside file tracked: {tracked}"
        );
    }
    assert!(!layout.mirror_dir.join("../victim.env").exists());
    // A later full pass must not sweep-delete anything outside the mirror.
    code_sanity::index_workspace(&repo).unwrap();
    assert!(outer.path().join("victim.env").exists());
}

#[test]
fn sync_force_path_outside_repo_is_an_error() {
    let (_outer, repo) = outer_with_repo();
    let err = index_single_file(&repo, Path::new("../victim.env")).unwrap_err();
    assert!(err.to_string().contains("escapes"), "{err:#}");
}

#[test]
fn absolute_paths_inside_repo_and_mirror_are_accepted() {
    let (_outer, repo) = outer_with_repo();
    // Hooks pass absolute real paths...
    let report = sync_single_file(&repo, &repo.join("src/a.rs")).unwrap();
    assert_eq!(report.indexed + report.unchanged, 1);
    // ...and absolute mirror paths.
    let layout = Layout::new(&repo);
    let report = sync_single_file(&repo, &layout.mirror_dir.join("src/a.rs")).unwrap();
    assert_eq!(report.indexed + report.unchanged, 1);
}

#[test]
fn poisoned_db_row_never_deletes_outside_the_mirror() {
    let (outer, repo) = outer_with_repo();
    // Simulate a DB row written by a pre-validation version.
    let layout = Layout::new(&repo);
    let conn = code_sanity::db::connect(&layout).unwrap();
    conn.execute_batch(
        "insert into files(rel_path, original_hash, sanitized_hash, original_size, \
         sanitized_size, language, updated_at) \
         values('../victim.env', 'x', 'x', 1, 1, null, 'now');",
    )
    .unwrap();
    drop(conn);

    // The stale sweep must drop the row without touching the outside file.
    let report = code_sanity::index_workspace(&repo).unwrap();
    assert!(report.removed >= 1);
    assert!(
        outer.path().join("victim.env").exists(),
        "sweep deleted a file outside the mirror"
    );
    let conn = code_sanity::db::connect(&layout).unwrap();
    assert!(
        !code_sanity::db::tracked_files(&conn)
            .unwrap()
            .iter()
            .any(|rel| rel.contains("..")),
    );
}

#[test]
fn cli_sync_path_outside_repo_exits_zero_with_skip() {
    let (_outer, repo) = outer_with_repo();
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("code-sanity"))
        .arg("--root")
        .arg(&repo)
        .args(["sync", "--path", "../victim.env"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("skipped=1"), "stdout: {stdout}");
}

#[test]
fn recover_refuses_journal_entry_with_escaping_path() {
    let (outer, repo) = outer_with_repo();
    // Hand-tamper a Applying journal entry whose pending rel escapes the repo.
    let layout = Layout::new(&repo);
    let entry = serde_json::json!({
        "id": "2099-01-01T00-00-00.000000000Z",
        "status": "applying",
        "session_id": null,
        "agent": null,
        "files": ["../victim.env"],
        "sanitized_patch": "",
        "original_patch": "",
        "error": null,
        "created_at": "2099-01-01T00:00:00Z",
        "pending": [{
            "rel": "../victim.env",
            "before": "SECRET=1\n",
            "after": "OWNED=1\n"
        }]
    });
    fs::write(
        layout
            .journal_dir
            .join("2099-01-01T00-00-00.000000000Z.patch.json"),
        serde_json::to_string_pretty(&entry).unwrap(),
    )
    .unwrap();

    let report = code_sanity::recover_workspace(&repo, false, false).unwrap();
    assert!(
        report
            .conflicts
            .iter()
            .any(|conflict| conflict.contains("escap")),
        "conflicts: {:?}",
        report.conflicts
    );
    assert_eq!(
        fs::read_to_string(outer.path().join("victim.env")).unwrap(),
        "SECRET=1\n",
        "recover wrote outside the repo"
    );
}
