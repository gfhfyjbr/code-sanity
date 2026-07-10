//! File permission bits must survive the patch bridge: back-projecting an
//! agent edit onto an executable script previously replaced the inode with a
//! fresh 0644 temp file, silently stripping the executable bit.

use code_sanity::{apply_patch_text, index_workspace, verify_workspace};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

fn init_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    fs::write(
        repo.path().join("tool.sh"),
        "#!/bin/sh\necho step one\necho step two\n",
    )
    .unwrap();
    fs::set_permissions(
        repo.path().join("tool.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    index_workspace(repo.path()).unwrap();
    repo
}

#[test]
fn apply_patch_keeps_executable_bit() {
    let repo = init_repo();
    let patch = "--- a/tool.sh\n\
         +++ b/tool.sh\n\
         @@ -2,2 +2,2 @@\n\
         -echo step one\n\
         +echo step ONE\n \
         echo step two\n";
    apply_patch_text(repo.path(), patch).unwrap();
    let real = fs::read_to_string(repo.path().join("tool.sh")).unwrap();
    assert!(real.contains("echo step ONE"));
    assert_eq!(
        mode_of(&repo.path().join("tool.sh")),
        0o755,
        "back-projection must not strip the executable bit"
    );
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn recover_rollback_recreates_deleted_file_with_journaled_mode() {
    let repo = init_repo();
    let real_path = repo.path().join("tool.sh");
    let before = fs::read_to_string(&real_path).unwrap();

    // Simulate a crash mid-apply of a delete: the journal recorded the intent
    // (including the mode) and the real file is already gone.
    let layout = code_sanity::config::Layout::new(repo.path());
    let entry = code_sanity::journal::JournalEntry {
        id: code_sanity::journal::new_journal_id(),
        status: code_sanity::journal::JournalStatus::Applying,
        session_id: None,
        agent: None,
        files: vec!["tool.sh".to_string()],
        sanitized_patch: String::new(),
        original_patch: String::new(),
        error: None,
        created_at: "now".to_string(),
        pending: Some(vec![code_sanity::journal::PendingFile {
            rel: "tool.sh".to_string(),
            before: Some(before.clone()),
            after: None,
            before_mode: Some(0o755),
            after_mode: None,
        }]),
    };
    code_sanity::journal::write_journal(&layout, &entry).unwrap();
    fs::remove_file(&real_path).unwrap();

    let report = code_sanity::recover_workspace(repo.path(), true, false).unwrap();
    assert_eq!(report.recovered.len(), 1);
    assert_eq!(fs::read_to_string(&real_path).unwrap(), before);
    assert_eq!(
        mode_of(&real_path),
        0o755,
        "a re-created file must restore its journaled mode"
    );
    assert!(verify_workspace(repo.path()).is_ok());
}
