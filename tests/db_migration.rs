//! Schema migration: `PRAGMA user_version` drives a drop-and-recreate of the
//! derived tables (the database is fully derived state; only `patch_journal`
//! history is preserved), and a database from a NEWER build is refused instead
//! of silently downgraded.

use code_sanity::config::Layout;
use code_sanity::db;
use std::fs;
use std::path::Path;

fn indexed_workspace() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("src/a.rs"),
        "fn alpha() -> usize {\n    1\n}\n",
    )
    .unwrap();
    code_sanity::index_workspace(repo.path()).unwrap();
    repo
}

fn set_user_version(root: &Path, version: i64) {
    let layout = Layout::new(root);
    let conn = db::connect(&layout).unwrap();
    conn.pragma_update(None, "user_version", version).unwrap();
}

fn user_version(root: &Path) -> i64 {
    let layout = Layout::new(root);
    let conn = db::connect(&layout).unwrap();
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn old_user_version_drops_derived_tables_and_preserves_journal() {
    let repo = indexed_workspace();
    let layout = Layout::new(repo.path());
    {
        let mut conn = db::connect(&layout).unwrap();
        db::insert_journal_row(
            &conn,
            Some("session"),
            Some("agent"),
            "sanitized",
            "original",
            "success",
            "2026-07-07T00:00:00Z",
        )
        .unwrap();
        db::replace_embeddings(
            &mut conn,
            "src/a.rs",
            "sha",
            "fingerprint",
            &[(1, 3, "chunk text", vec![0u8; 4])],
        )
        .unwrap();
        // An old schema had a different `files` shape; the marker column must
        // vanish with the drop-and-recreate.
        conn.execute_batch("alter table files add column legacy_marker text")
            .unwrap();
    }
    set_user_version(repo.path(), 1);

    let conn = db::connect(&layout).unwrap();
    db::ensure_schema(&conn).unwrap();

    assert_eq!(user_version(repo.path()), 2);
    let files: i64 = conn
        .query_row("select count(*) from files", [], |row| row.get(0))
        .unwrap();
    assert_eq!(files, 0, "derived rows must be dropped");
    assert!(
        conn.query_row("select legacy_marker from files limit 1", [], |_| Ok(()))
            .is_err(),
        "old-shape files table must be recreated"
    );
    let journal: i64 = conn
        .query_row("select count(*) from patch_journal", [], |row| row.get(0))
        .unwrap();
    assert_eq!(journal, 1, "patch_journal history must survive migration");
    // Documented current behavior: embedding tables were added in v2, so the
    // v1->v2 drop list does not touch them. A future SCHEMA_VERSION bump must
    // extend the drop list (see the NOTE in ensure_schema).
    let chunks: i64 = conn
        .query_row("select count(*) from embedding_chunks", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(chunks, 1);
}

#[test]
fn pre_versioning_v0_database_is_not_dropped() {
    // user_version 0 is indistinguishable from a freshly created database, so
    // the drop branch deliberately skips it.
    let repo = indexed_workspace();
    set_user_version(repo.path(), 0);

    let layout = Layout::new(repo.path());
    let conn = db::connect(&layout).unwrap();
    db::ensure_schema(&conn).unwrap();

    assert_eq!(user_version(repo.path()), 2);
    // init created a .gitignore next to src/a.rs; both rows must survive.
    let files: i64 = conn
        .query_row("select count(*) from files", [], |row| row.get(0))
        .unwrap();
    assert_eq!(files, 2, "v0 rows must survive");
}

#[test]
fn future_schema_version_is_refused_without_downgrade() {
    let repo = indexed_workspace();
    set_user_version(repo.path(), 3);

    let layout = Layout::new(repo.path());
    let conn = db::connect(&layout).unwrap();
    let err = db::ensure_schema(&conn).unwrap_err();
    assert!(err.to_string().contains("schema version 3"), "{err:#}");
    drop(conn);

    // What users actually hit: any command opening the workspace.
    let err = code_sanity::index_workspace(repo.path()).unwrap_err();
    assert!(err.to_string().contains("newer"), "{err:#}");

    assert_eq!(user_version(repo.path()), 3, "no silent downgrade");
}

#[test]
fn read_paths_never_migrate_or_stamp_the_version() {
    let repo = indexed_workspace();
    set_user_version(repo.path(), 1);

    // A read command on an outdated schema must direct the user to `index`,
    // not migrate in place (it holds only the shared lock) and not write the
    // version header.
    let err = code_sanity::verify_workspace(repo.path()).unwrap_err();
    assert!(err.to_string().contains("code-sanity index"), "{err:#}");
    assert_eq!(user_version(repo.path()), 1, "read path wrote user_version");

    let layout = Layout::new(repo.path());
    let conn = db::connect(&layout).unwrap();
    let files: i64 = conn
        .query_row("select count(*) from files", [], |row| row.get(0))
        .unwrap();
    assert!(files > 0, "read path dropped derived tables");
}

#[test]
fn check_schema_covers_current_newer_and_older() {
    let repo = indexed_workspace();
    let layout = Layout::new(repo.path());
    let conn = db::connect(&layout).unwrap();
    db::check_schema(&conn).unwrap();
    drop(conn);

    set_user_version(repo.path(), 3);
    let conn = db::connect(&layout).unwrap();
    let err = db::check_schema(&conn).unwrap_err();
    assert!(err.to_string().contains("newer"), "{err:#}");
    drop(conn);

    set_user_version(repo.path(), 1);
    let conn = db::connect(&layout).unwrap();
    let err = db::check_schema(&conn).unwrap_err();
    assert!(err.to_string().contains("older"), "{err:#}");
    assert!(err.to_string().contains("code-sanity index"), "{err:#}");
}

#[test]
fn concurrent_index_after_upgrade_is_serialized() {
    // The drop-tables-under-a-writer race: several processes hit the v1->v2
    // migration at once. The exclusive lock inside init must serialize them so
    // every run exits clean and the final state verifies.
    let repo = indexed_workspace();
    set_user_version(repo.path(), 1);

    let binary = assert_cmd::cargo::cargo_bin("code-sanity");
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let binary = binary.clone();
            let root = repo.path().to_path_buf();
            std::thread::spawn(move || {
                std::process::Command::new(&binary)
                    .arg("--root")
                    .arg(&root)
                    .arg("index")
                    .output()
                    .unwrap()
            })
        })
        .collect();
    for handle in handles {
        let output = handle.join().unwrap();
        assert!(
            output.status.success(),
            "concurrent index failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(user_version(repo.path()), 2);
    assert!(code_sanity::verify_workspace(repo.path()).is_ok());
}

#[test]
fn reindex_after_migration_repopulates_derived_state() {
    let repo = indexed_workspace();
    set_user_version(repo.path(), 1);

    // index_workspace calls ensure_schema internally: migrate + rebuild.
    code_sanity::index_workspace(repo.path()).unwrap();

    let layout = Layout::new(repo.path());
    let conn = db::connect(&layout).unwrap();
    let tracked = db::tracked_files(&conn).unwrap();
    assert!(
        tracked.contains(&"src/a.rs".to_string()),
        "tracked after reindex: {tracked:?}"
    );
    assert!(code_sanity::verify_workspace(repo.path()).is_ok());
}
