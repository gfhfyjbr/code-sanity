use assert_cmd::Command;
use code_sanity::{
    apply_patch_text, index_workspace, read_sanitized_file, search_mirror, verify_workspace,
    write_sanitized_content,
};
use predicates::prelude::*;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn copy_fixture(name: &str) -> TempDir {
    let temp = tempfile::tempdir().unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name);
    copy_dir(&source, temp.path()).unwrap();
    temp
}

fn copy_dir(source: &Path, dest: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let next_dest = dest.join(entry.file_name());
        if ty.is_dir() {
            copy_dir(&entry.path(), &next_dest)?;
        } else if ty.is_file() {
            fs::copy(entry.path(), next_dest)?;
        }
    }
    Ok(())
}

#[test]
fn index_read_search_and_ignore_rules_work() {
    let repo = copy_fixture("basic-rust");
    let report = index_workspace(repo.path()).unwrap();
    assert!(report.indexed >= 2);
    let repeat = index_workspace(repo.path()).unwrap();
    assert_eq!(repeat.indexed, 0);
    assert!(repeat.unchanged >= 2);

    let sanitized = read_sanitized_file(repo.path(), Path::new("src/lib.rs")).unwrap();
    assert!(sanitized.contains("neutral comment"));
    assert!(sanitized.contains("fn neutral_parser()"));
    assert!(sanitized.contains("\"dangerous runtime string should stay real\""));
    assert!(sanitized.contains("\"neutral fixture text\""));

    let hits = search_mirror(repo.path(), "neutral_parser", None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].rel_path, "src/lib.rs");

    assert!(!repo.path().join(".code-sanity/mirror/ignored.txt").exists());
    assert!(
        !repo
            .path()
            .join(".code-sanity/mirror/target/generated.rs")
            .exists()
    );
}

#[test]
fn span_map_records_utf8_offsets_and_roundtrips_aliases() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let map_path = repo.path().join(".code-sanity/maps/src/lib.rs.map.json");
    let raw = fs::read_to_string(map_path).unwrap();
    let span_map: code_sanity::map::SpanMap = serde_json::from_str(&raw).unwrap();
    let real = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    let mirror = fs::read_to_string(repo.path().join(".code-sanity/mirror/src/lib.rs")).unwrap();

    let replacement = span_map
        .replacements
        .iter()
        .find(|replacement| replacement.original_text == "dangerous")
        .unwrap();
    assert_eq!(
        &real[replacement.original_start..replacement.original_end],
        "dangerous"
    );
    assert_eq!(
        &mirror[replacement.sanitized_start..replacement.sanitized_end],
        "neutral"
    );
    assert_eq!(replacement.sanitized_text, "neutral");
}

#[test]
fn read_write_and_patch_reject_path_traversal() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();

    let read_err = read_sanitized_file(repo.path(), Path::new("../../Cargo.toml")).unwrap_err();
    assert!(read_err.to_string().contains("escapes sanitized mirror"));

    let write_err =
        write_sanitized_content(repo.path(), Path::new("../../Cargo.toml"), "x").unwrap_err();
    assert!(write_err.to_string().contains("escapes sanitized mirror"));

    let patch = "\
--- a/../../Cargo.toml
+++ b/../../Cargo.toml
@@ -1,1 +1,1 @@
-x
+y
";
    let patch_err = apply_patch_text(repo.path(), patch).unwrap_err();
    assert!(
        patch_err
            .to_string()
            .contains("patch paths are not inside sanitized mirror or repo")
    );
}

#[test]
fn empty_search_and_grep_return_clear_error() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();

    let err = search_mirror(repo.path(), "", None).unwrap_err();
    assert!(err.to_string().contains("must not be empty"));

    Command::cargo_bin("code-sanity")
        .unwrap()
        .args(["--root", repo.path().to_str().unwrap(), "grep", ""])
        .assert()
        .failure()
        .stderr(predicate::str::contains("search query must not be empty"));
}

#[test]
fn apply_patch_outside_replacement_updates_real_and_mirror() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let patch = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -2,3 +2,3 @@
 fn neutral_parser() -> usize {
-    1
+    2
 }
";
    let report = apply_patch_text(repo.path(), patch).unwrap();
    assert_eq!(report.files, vec!["src/lib.rs"]);
    let real = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    assert!(real.contains("fn dangerous_parser() -> usize"));
    assert!(real.contains("    2"));
    let mirror = read_sanitized_file(repo.path(), Path::new("src/lib.rs")).unwrap();
    assert!(mirror.contains("fn neutral_parser() -> usize"));
    assert!(mirror.contains("    2"));
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn apply_patch_adjacent_to_replacement_keeps_original_alias() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let patch = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -2,1 +2,1 @@
-fn neutral_parser() -> usize {
+fn neutral_parser(input: usize) -> usize {
";
    apply_patch_text(repo.path(), patch).unwrap();
    let real = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    assert!(real.contains("fn dangerous_parser(input: usize) -> usize"));
    let mirror = read_sanitized_file(repo.path(), Path::new("src/lib.rs")).unwrap();
    assert!(mirror.contains("fn neutral_parser(input: usize) -> usize"));
}

#[test]
fn apply_patch_does_not_reverse_new_alias_collision_text() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let patch = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -2,3 +2,4 @@
 fn neutral_parser() -> usize {
+    let neutral = 10;
     1
 }
";
    apply_patch_text(repo.path(), patch).unwrap();
    let real = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    assert!(real.contains("fn dangerous_parser() -> usize"));
    assert!(real.contains("let neutral = 10;"));
    assert!(!real.contains("let dangerous = 10;"));
    let mirror = read_sanitized_file(repo.path(), Path::new("src/lib.rs")).unwrap();
    assert!(mirror.contains("fn neutral_parser() -> usize"));
    assert!(mirror.contains("let neutral = 10;"));
}

#[test]
fn public_rust_api_symbol_and_call_stay_consistent() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("Cargo.toml"),
        "[package]\nname = \"public-api-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("src/lib.rs"),
        "pub fn dangerous_parser() -> usize {\n    1\n}\n\npub fn call_parser() -> usize {\n    dangerous_parser()\n}\n",
    )
    .unwrap();

    index_workspace(repo.path()).unwrap();
    let sanitized = read_sanitized_file(repo.path(), Path::new("src/lib.rs")).unwrap();
    assert!(sanitized.contains("pub fn dangerous_parser() -> usize"));
    assert!(sanitized.contains("dangerous_parser()"));
    assert!(!sanitized.contains("neutral_parser"));
}

#[test]
fn apply_patch_inside_replacement_conflicts_and_leaves_real_file() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let before = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    let patch = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -2,1 +2,1 @@
-fn neutral_parser() -> usize {
+fn pleasant_parser() -> usize {
";
    let err = apply_patch_text(repo.path(), patch).unwrap_err();
    assert!(err.to_string().contains("replacement span"));
    let after = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    assert_eq!(before, after);
    let journal_entries = fs::read_dir(repo.path().join(".code-sanity/journal"))
        .unwrap()
        .collect::<Vec<_>>();
    assert_eq!(journal_entries.len(), 1);
}

#[test]
fn sync_repairs_external_real_edit() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let real_path = repo.path().join("src/lib.rs");
    let mut real = fs::read_to_string(&real_path).unwrap();
    real.push_str("\n// dangerous external edit\n");
    fs::write(&real_path, real).unwrap();

    assert!(verify_workspace(repo.path()).is_err());
    index_workspace(repo.path()).unwrap();
    assert!(verify_workspace(repo.path()).is_ok());
    let mirror = read_sanitized_file(repo.path(), Path::new("src/lib.rs")).unwrap();
    assert!(mirror.contains("neutral external edit"));
}

#[test]
fn mixed_language_fixture_is_sanitized_and_searchable() {
    let repo = copy_fixture("mixed");
    index_workspace(repo.path()).unwrap();
    let hits = search_mirror(repo.path(), "neutral", None).unwrap();
    let paths = hits
        .into_iter()
        .map(|hit| hit.rel_path)
        .collect::<std::collections::BTreeSet<_>>();
    assert!(paths.contains("README.md"));
    assert!(paths.contains("app.py"));
    assert!(paths.contains("ui.ts"));
}

#[test]
fn write_command_back_projects_sanitized_content() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let mirror = read_sanitized_file(repo.path(), Path::new("src/lib.rs")).unwrap();
    let edited = mirror.replace("    1\n", "    5\n");
    write_sanitized_content(repo.path(), Path::new("src/lib.rs"), &edited).unwrap();
    let real = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    assert!(real.contains("    5"));
    assert!(real.contains("dangerous_parser"));
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn cli_index_read_search_verify_smoke() {
    let repo = copy_fixture("basic-rust");
    Command::cargo_bin("code-sanity")
        .unwrap()
        .args(["--root", repo.path().to_str().unwrap(), "index"])
        .assert()
        .success()
        .stdout(predicate::str::contains("indexed="));

    Command::cargo_bin("code-sanity")
        .unwrap()
        .args([
            "--root",
            repo.path().to_str().unwrap(),
            "read",
            "src/lib.rs",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("neutral_parser"));

    Command::cargo_bin("code-sanity")
        .unwrap()
        .args([
            "--root",
            repo.path().to_str().unwrap(),
            "grep",
            "neutral_parser",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("src/lib.rs"));

    Command::cargo_bin("code-sanity")
        .unwrap()
        .args(["--root", repo.path().to_str().unwrap(), "verify"])
        .assert()
        .success()
        .stdout(predicate::str::contains("verified tracked_files="));
}
