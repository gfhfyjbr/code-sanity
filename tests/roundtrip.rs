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

/// The workspace's actual alias for `term`. Default aliases carry a suffix
/// derived from the per-workspace random salt (`neutral_3fd1`-style), so
/// tests read the mapping instead of hardcoding a spelling.
fn alias_of(repo: &Path, term: &str) -> String {
    code_sanity::Config::load_or_default(&code_sanity::Layout::new(repo))
        .unwrap()
        .sanitizer
        .dictionary
        .get(term)
        .cloned()
        .unwrap_or_else(|| panic!("{term} is not in the workspace dictionary"))
}

fn python3_bin() -> Option<&'static str> {
    for candidate in ["python3", "python"] {
        let ok = std::process::Command::new(candidate)
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false);
        if ok {
            return Some(candidate);
        }
    }
    None
}

fn set_mode(repo: &Path, mode: &str) {
    let cfg = repo.join(".code-sanity/config.toml");
    let body = fs::read_to_string(&cfg).unwrap();
    let body = body.replace("mode = \"guided\"", &format!("mode = \"{mode}\""));
    fs::write(&cfg, body).unwrap();
}

fn run_hook(py: &str, script: &Path, cwd: &Path, stdin: &str) -> String {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new(py)
        .arg(script)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
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

    let alias = alias_of(repo.path(), "dangerous");
    let sanitized = read_sanitized_file(repo.path(), Path::new("src/lib.rs")).unwrap();
    assert!(sanitized.contains(&format!("{alias} comment")));
    assert!(sanitized.contains(&format!("fn {alias}_parser()")));
    // Terms are sanitized in every string literal, not only test fixtures.
    assert!(sanitized.contains(&format!("\"{alias} runtime string should stay real\"")));
    assert!(sanitized.contains(&format!("\"{alias} fixture text\"")));
    assert!(!sanitized.to_lowercase().contains("dangerous"));

    let hits = search_mirror(repo.path(), &format!("{alias}_parser"), None).unwrap();
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

    let alias = alias_of(repo.path(), "dangerous");
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
        alias
    );
    assert_eq!(replacement.sanitized_text, alias);
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
    let alias = alias_of(repo.path(), "dangerous");
    let patch = format!(
        "--- a/src/lib.rs\n\
         +++ b/src/lib.rs\n\
         @@ -2,3 +2,3 @@\n \
         fn {alias}_parser() -> usize {{\n\
         -    1\n\
         +    2\n \
         }}\n"
    );
    let report = apply_patch_text(repo.path(), &patch).unwrap();
    assert_eq!(report.files, vec!["src/lib.rs"]);
    let real = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    assert!(real.contains("fn dangerous_parser() -> usize"));
    assert!(real.contains("    2"));
    let mirror = read_sanitized_file(repo.path(), Path::new("src/lib.rs")).unwrap();
    assert!(mirror.contains(&format!("fn {alias}_parser() -> usize")));
    assert!(mirror.contains("    2"));
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn apply_patch_adjacent_to_replacement_keeps_original_alias() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let alias = alias_of(repo.path(), "dangerous");
    let patch = format!(
        "--- a/src/lib.rs\n\
         +++ b/src/lib.rs\n\
         @@ -2,1 +2,1 @@\n\
         -fn {alias}_parser() -> usize {{\n\
         +fn {alias}_parser(input: usize) -> usize {{\n"
    );
    apply_patch_text(repo.path(), &patch).unwrap();
    let real = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    assert!(real.contains("fn dangerous_parser(input: usize) -> usize"));
    let mirror = read_sanitized_file(repo.path(), Path::new("src/lib.rs")).unwrap();
    assert!(mirror.contains(&format!("fn {alias}_parser(input: usize) -> usize")));
}

#[test]
fn apply_patch_rejects_existing_alias_as_a_new_declaration() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let alias = alias_of(repo.path(), "dangerous");
    let patch = format!(
        "--- a/src/lib.rs\n\
         +++ b/src/lib.rs\n\
         @@ -2,3 +2,4 @@\n \
         fn {alias}_parser() -> usize {{\n\
         +    let {alias} = 10;\n     \
         1\n \
         }}\n"
    );
    let error = apply_patch_text(repo.path(), &patch).unwrap_err();
    assert!(error.to_string().contains("new declaration"));
    let real = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    assert!(real.contains("fn dangerous_parser() -> usize"));
    assert!(!real.contains("let dangerous = 10;"));
    assert!(!real.contains(&format!("let {alias} = 10;")));
    let mirror = read_sanitized_file(repo.path(), Path::new("src/lib.rs")).unwrap();
    assert!(mirror.contains(&format!("fn {alias}_parser() -> usize")));
    assert!(!mirror.contains(&format!("let {alias} = 10;")));
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn apply_patch_reverse_maps_alias_call_in_added_line() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let alias = alias_of(repo.path(), "dangerous");
    // The agent adds a call to the alias it sees in the mirror; the real file
    // must call the real function, not the (nonexistent) alias.
    let patch = format!(
        "--- a/src/lib.rs\n\
         +++ b/src/lib.rs\n\
         @@ -6,3 +6,7 @@\n \
         fn safe_helper() -> &'static str {{\n     \
         \"{alias} runtime string should stay real\"\n \
         }}\n\
         +\n\
         +fn call_it() -> usize {{\n\
         +    {alias}_parser()\n\
         +}}\n"
    );
    apply_patch_text(repo.path(), &patch).unwrap();
    let real = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    assert!(real.contains("dangerous_parser()"));
    assert!(!real.contains(&format!("{alias}_parser()")));
    let mirror = read_sanitized_file(repo.path(), Path::new("src/lib.rs")).unwrap();
    assert!(mirror.contains(&format!("{alias}_parser()")));
    assert!(!mirror.to_lowercase().contains("dangerous"));
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn apply_patch_leaves_innocent_alias_containing_identifier_alone() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let alias = alias_of(repo.path(), "dangerous");
    // An added identifier that is NOT an alias word must land verbatim in the
    // real file — the reverse mapper only fires on whole-run alias matches.
    let patch = format!(
        "--- a/src/lib.rs\n\
         +++ b/src/lib.rs\n\
         @@ -2,3 +2,4 @@\n \
         fn {alias}_parser() -> usize {{\n\
         +    let count_things = 10;\n     \
         1\n \
         }}\n"
    );
    apply_patch_text(repo.path(), &patch).unwrap();
    let real = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    assert!(real.contains("let count_things = 10;"));
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn verify_fails_on_planted_dictionary_term_in_mirror() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let mirror_path = repo.path().join(".code-sanity/mirror/src/lib.rs");
    let mirror = fs::read_to_string(&mirror_path).unwrap();
    fs::write(&mirror_path, format!("{mirror}// planted dangerous term\n")).unwrap();

    let err = verify_workspace(repo.path()).unwrap_err();
    let message = format!("{err}");
    assert!(message.contains("leak of term"), "got: {message}");

    // CLI prints every failure and exits with the dedicated "broken" code 3.
    Command::cargo_bin("code-sanity")
        .unwrap()
        .args(["--root", repo.path().to_str().unwrap(), "verify"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("leak of term"));

    // Plain sync preserves the (possibly agent-owned) mirror bytes; the
    // recovery path is sync --force, which resets the mirror to sanitize(real).
    Command::cargo_bin("code-sanity")
        .unwrap()
        .args(["--root", repo.path().to_str().unwrap(), "sync", "--force"])
        .assert()
        .success();
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn verify_fails_on_planted_untracked_mirror_file() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    fs::write(
        repo.path().join(".code-sanity/mirror/src/planted.rs"),
        "// looks innocent\n",
    )
    .unwrap();
    let err = verify_workspace(repo.path()).unwrap_err();
    assert!(format!("{err}").contains("untracked file in mirror"));
}

#[test]
fn apply_patch_conflict_exits_with_code_2() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let patch = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -2,1 +2,1 @@
-fn neutral_parser() -> usize {
+fn pleasant_parser() -> usize {
";
    let patch_path = repo.path().join("conflict.patch");
    fs::write(&patch_path, patch).unwrap();
    Command::cargo_bin("code-sanity")
        .unwrap()
        .args([
            "--root",
            repo.path().to_str().unwrap(),
            "apply-patch",
            "--patch",
            patch_path.to_str().unwrap(),
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("replacement span"));
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
    // Count entry files only (the journal dir also holds the inflight/ marker
    // dir): exactly one conflict record.
    let journal_entries = fs::read_dir(repo.path().join(".code-sanity/journal"))
        .unwrap()
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect::<Vec<_>>();
    assert_eq!(journal_entries.len(), 1, "{journal_entries:?}");
}

#[test]
fn sync_preserves_pending_mirror_edit() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let mirror_path = repo.path().join(".code-sanity/mirror/src/lib.rs");

    // The agent edited the mirror in place; the edit has not been projected
    // yet (mirror hash != db sanitized hash). A sync storm must not clobber it.
    let mirror = fs::read_to_string(&mirror_path).unwrap();
    let edited = mirror.replace("    1\n", "    6\n");
    assert_ne!(mirror, edited);
    fs::write(&mirror_path, &edited).unwrap();

    // Unchanged real file: the fast path leaves the mirror alone entirely.
    index_workspace(repo.path()).unwrap();
    assert_eq!(fs::read_to_string(&mirror_path).unwrap(), edited);

    // Changed real file: the file is re-rendered, but the pending mirror edit
    // still wins over the fresh render and is reported.
    let real_path = repo.path().join("src/lib.rs");
    let mut real = fs::read_to_string(&real_path).unwrap();
    real.push_str("// external note\n");
    fs::write(&real_path, real).unwrap();
    let report = index_workspace(repo.path()).unwrap();
    assert_eq!(report.pending, 1, "pending mirror edit not detected");
    assert_eq!(fs::read_to_string(&mirror_path).unwrap(), edited);

    // Reconciling a pending mirror edit against a real file that ALSO drifted
    // externally cannot be done automatically: project-edit conflicts, the
    // real file keeps the external change, and the workspace stays coherent.
    let real_before = fs::read_to_string(&real_path).unwrap();
    let err = code_sanity::project_mirror_edit(
        repo.path(),
        Path::new("src/lib.rs"),
        code_sanity::patch::ApplyOptions::default(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("conflict journal"));
    assert_eq!(fs::read_to_string(&real_path).unwrap(), real_before);
    let after = index_workspace(repo.path()).unwrap();
    assert_eq!(after.pending, 0);
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn init_generates_random_workspace_salt() {
    let repo_a = tempfile::tempdir().unwrap();
    let repo_b = tempfile::tempdir().unwrap();
    code_sanity::init_workspace(repo_a.path()).unwrap();
    code_sanity::init_workspace(repo_b.path()).unwrap();
    let layout_a = code_sanity::config::Layout::new(repo_a.path());
    let layout_b = code_sanity::config::Layout::new(repo_b.path());
    let salt_a = code_sanity::config::Config::load_or_default(&layout_a)
        .unwrap()
        .salt;
    let salt_b = code_sanity::config::Config::load_or_default(&layout_b)
        .unwrap()
        .salt;
    assert_ne!(salt_a, "code-sanity-local-stub");
    assert_ne!(salt_a, salt_b, "salts must differ per workspace");
    // init is idempotent: reinitializing keeps the existing salt.
    code_sanity::init_workspace(repo_a.path()).unwrap();
    let salt_a_again = code_sanity::config::Config::load_or_default(&layout_a)
        .unwrap()
        .salt;
    assert_eq!(salt_a, salt_a_again);
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
    let alias = alias_of(repo.path(), "dangerous");
    assert!(mirror.contains(&format!("{alias} external edit")));
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
fn create_patch_adds_new_real_file() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let patch = "\
--- /dev/null
+++ b/src/added.rs
@@ -0,0 +1,3 @@
+pub fn added() -> usize {
+    7
+}
";
    let report = apply_patch_text(repo.path(), patch).unwrap();
    assert_eq!(report.files, vec!["src/added.rs"]);
    let real = fs::read_to_string(repo.path().join("src/added.rs")).unwrap();
    assert_eq!(real, "pub fn added() -> usize {\n    7\n}\n");
    let mirror = read_sanitized_file(repo.path(), Path::new("src/added.rs")).unwrap();
    assert_eq!(mirror, real);
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn create_patch_with_sanitizable_content_conflicts() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let patch = "\
--- /dev/null
+++ b/src/added.rs
@@ -0,0 +1,1 @@
+// dangerous new comment
";
    let err = apply_patch_text(repo.path(), patch).unwrap_err();
    assert!(err.to_string().contains("sanitizable"));
    assert!(!repo.path().join("src/added.rs").exists());
}

#[test]
fn delete_patch_removes_file_mirror_and_map() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let mirror = read_sanitized_file(repo.path(), Path::new("src/lib.rs")).unwrap();
    let count = mirror.lines().count();
    let mut patch = String::from("--- a/src/lib.rs\n+++ /dev/null\n");
    patch.push_str(&format!("@@ -1,{count} +0,0 @@\n"));
    for line in mirror.lines() {
        patch.push_str(&format!("-{line}\n"));
    }
    let report = apply_patch_text(repo.path(), &patch).unwrap();
    assert_eq!(report.files, vec!["src/lib.rs"]);
    assert!(!repo.path().join("src/lib.rs").exists());
    assert!(!repo.path().join(".code-sanity/mirror/src/lib.rs").exists());
    assert!(
        !repo
            .path()
            .join(".code-sanity/maps/src/lib.rs.map.json")
            .exists()
    );
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn recover_replays_interrupted_apply() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let real_path = repo.path().join("src/lib.rs");
    let before = fs::read_to_string(&real_path).unwrap();
    let after = before.replace("    1\n", "    9\n");
    assert_ne!(before, after);

    // Simulate a crash: the `applying` journal is durably written, but the real
    // file was not modified yet, and the stale apply lock is still on disk.
    let layout = code_sanity::config::Layout::new(repo.path());
    fs::write(layout.tmp_dir.join("apply.lock"), "stale").unwrap();
    let entry = code_sanity::journal::JournalEntry {
        id: code_sanity::journal::new_journal_id(),
        status: code_sanity::journal::JournalStatus::Applying,
        session_id: None,
        agent: None,
        files: vec!["src/lib.rs".to_string()],
        sanitized_patch: String::new(),
        original_patch: String::new(),
        error: None,
        created_at: "now".to_string(),
        pending: Some(vec![code_sanity::journal::PendingFile {
            rel: "src/lib.rs".to_string(),
            before: Some(before.clone()),
            after: Some(after.clone()),
            before_mode: None,
            after_mode: None,
        }]),
    };
    code_sanity::journal::write_journal(&layout, &entry).unwrap();

    let report = code_sanity::recover_workspace(repo.path(), false, false).unwrap();
    assert_eq!(report.recovered.len(), 1);
    assert!(!report.rolled_back);
    assert_eq!(fs::read_to_string(&real_path).unwrap(), after);
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn recover_rolls_back_interrupted_apply() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let real_path = repo.path().join("src/lib.rs");
    let before = fs::read_to_string(&real_path).unwrap();
    let after = before.replace("    1\n", "    9\n");
    // Simulate a crash after the real file was written but before finalize.
    fs::write(&real_path, &after).unwrap();

    let layout = code_sanity::config::Layout::new(repo.path());
    let entry = code_sanity::journal::JournalEntry {
        id: code_sanity::journal::new_journal_id(),
        status: code_sanity::journal::JournalStatus::Applying,
        session_id: None,
        agent: None,
        files: vec!["src/lib.rs".to_string()],
        sanitized_patch: String::new(),
        original_patch: String::new(),
        error: None,
        created_at: "now".to_string(),
        pending: Some(vec![code_sanity::journal::PendingFile {
            rel: "src/lib.rs".to_string(),
            before: Some(before.clone()),
            after: Some(after.clone()),
            before_mode: None,
            after_mode: None,
        }]),
    };
    code_sanity::journal::write_journal(&layout, &entry).unwrap();

    let report = code_sanity::recover_workspace(repo.path(), true, false).unwrap();
    assert_eq!(report.recovered.len(), 1);
    assert!(report.rolled_back);
    assert_eq!(fs::read_to_string(&real_path).unwrap(), before);
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn recover_treats_torn_non_utf8_file_as_conflict_not_abort() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let real_path = repo.path().join("src/lib.rs");
    let before = fs::read_to_string(&real_path).unwrap();
    let after = before.replace("    1\n", "    9\n");
    assert_ne!(before, after);

    let layout = code_sanity::config::Layout::new(repo.path());
    let entry = code_sanity::journal::JournalEntry {
        id: code_sanity::journal::new_journal_id(),
        status: code_sanity::journal::JournalStatus::Applying,
        session_id: None,
        agent: None,
        files: vec!["src/lib.rs".to_string()],
        sanitized_patch: String::new(),
        original_patch: String::new(),
        error: None,
        created_at: "now".to_string(),
        pending: Some(vec![code_sanity::journal::PendingFile {
            rel: "src/lib.rs".to_string(),
            before: Some(before.clone()),
            after: Some(after.clone()),
            before_mode: None,
            after_mode: None,
        }]),
    };
    code_sanity::journal::write_journal(&layout, &entry).unwrap();
    // Power loss tore the file: invalid UTF-8 that matches neither snapshot.
    fs::write(&real_path, b"\x80torn mid-write\xff").unwrap();

    // The run must complete with a conflict — not abort before --force is
    // even consulted — and the workspace must stay blocked.
    let report = code_sanity::recover_workspace(repo.path(), false, false).unwrap();
    assert!(report.recovered.is_empty());
    assert_eq!(report.conflicts.len(), 1);
    assert!(report.conflicts[0].contains("--force"));
    let blocked = index_workspace(repo.path()).unwrap_err().to_string();
    assert!(blocked.contains("recover"));

    // --force replays the journaled target over the torn bytes.
    let forced = code_sanity::recover_workspace(repo.path(), false, true).unwrap();
    assert_eq!(forced.recovered.len(), 1);
    assert_eq!(fs::read_to_string(&real_path).unwrap(), after);
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn rename_alias_renames_real_symbol() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();

    let alias = alias_of(repo.path(), "dangerous");
    let report = code_sanity::rename_alias(
        repo.path(),
        Path::new("src/lib.rs"),
        &format!("{alias}_parser"),
        "friendly_parser",
        code_sanity::patch::ApplyOptions::default(),
    )
    .unwrap();
    assert_eq!(report.real_from, "dangerous_parser");
    assert_eq!(report.sanitized_to, "friendly_parser");
    assert_eq!(report.occurrences, 1);

    let real = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    assert!(real.contains("fn friendly_parser()"));
    assert!(!real.contains("dangerous_parser"));
    let mirror = read_sanitized_file(repo.path(), Path::new("src/lib.rs")).unwrap();
    assert!(mirror.contains("fn friendly_parser()"));
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn gitignore_full_syntax_is_respected() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src/logs")).unwrap();
    fs::create_dir_all(repo.path().join("secret")).unwrap();
    fs::write(
        repo.path().join(".gitignore"),
        "**/*.log\n!keep.log\nsecret/\n",
    )
    .unwrap();
    fs::write(repo.path().join("src/app.rs"), "fn safe() {}\n").unwrap();
    fs::write(repo.path().join("src/logs/debug.log"), "log line\n").unwrap();
    fs::write(repo.path().join("keep.log"), "keep line\n").unwrap();
    fs::write(repo.path().join("secret/data.rs"), "fn secret_thing() {}\n").unwrap();

    index_workspace(repo.path()).unwrap();
    let mirror = repo.path().join(".code-sanity/mirror");
    assert!(mirror.join("src/app.rs").exists());
    assert!(!mirror.join("src/logs/debug.log").exists()); // matched by **/*.log
    assert!(mirror.join("keep.log").exists()); // negated by !keep.log
    assert!(!mirror.join("secret/data.rs").exists()); // dir pattern secret/
}

#[test]
fn dotenv_secrets_are_ignored_even_before_gitignore_exists() {
    let repo = tempfile::tempdir().unwrap();
    fs::write(repo.path().join(".env"), "BYESU_API_KEY=secret-value\n").unwrap();
    fs::write(repo.path().join("main.rs"), "fn main() {}\n").unwrap();

    index_workspace(repo.path()).unwrap();

    assert!(!repo.path().join(".code-sanity/mirror/.env").exists());
    let gitignore = fs::read_to_string(repo.path().join(".gitignore")).unwrap();
    assert!(gitignore.lines().any(|line| line.trim() == ".env"));
}

#[test]
fn opencode_install_generates_working_plugin() {
    let repo = copy_fixture("basic-rust");
    Command::cargo_bin("code-sanity")
        .unwrap()
        .args([
            "--root",
            repo.path().to_str().unwrap(),
            "install-hooks",
            "--agent",
            "opencode",
        ])
        .assert()
        .success();

    let plugin = repo.path().join(".opencode/plugins/code-sanity.ts");
    let body = fs::read_to_string(&plugin).unwrap();
    assert!(body.contains("project-edit"));
    assert!(body.contains(".code-sanity/mirror"));
    assert!(body.contains("tool.execute.before"));
    assert!(body.contains("tool.execute.after"));
    assert!(body.contains("strict mode"));
    assert!(repo.path().join(".opencode/package.json").exists());

    // doctor reports the plugin as installed.
    Command::cargo_bin("code-sanity")
        .unwrap()
        .args([
            "--root",
            repo.path().to_str().unwrap(),
            "doctor",
            "--agent",
            "opencode",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("installed=true"));

    Command::cargo_bin("code-sanity")
        .unwrap()
        .args(["--root", repo.path().to_str().unwrap(), "mode"])
        .assert()
        .success()
        .stdout(predicate::str::contains("guided"));
}

#[test]
fn opencode_bridge_projects_mirror_edit_to_real() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let mirror_path = repo.path().join(".code-sanity/mirror/src/lib.rs");

    // The plugin redirects read+edit to the mirror, so the agent edits the
    // sanitized mirror file directly. Simulate that in-place edit.
    let mirror = fs::read_to_string(&mirror_path).unwrap();
    let edited = mirror.replace("    1\n", "    3\n");
    assert_ne!(mirror, edited);
    fs::write(&mirror_path, &edited).unwrap();

    // The after-hook back-projects the mirror edit to the real repo.
    code_sanity::project_mirror_edit(
        repo.path(),
        Path::new("src/lib.rs"),
        code_sanity::patch::ApplyOptions::default(),
    )
    .unwrap();

    let real = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    assert!(real.contains("    3"));
    assert!(real.contains("fn dangerous_parser()")); // real name preserved
    let mirror_after = read_sanitized_file(repo.path(), Path::new("src/lib.rs")).unwrap();
    let alias = alias_of(repo.path(), "dangerous");
    assert!(mirror_after.contains(&format!("fn {alias}_parser()")));
    assert!(mirror_after.contains("    3"));
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn opencode_bridge_conflicts_on_replacement_span_edit() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let mirror_path = repo.path().join(".code-sanity/mirror/src/lib.rs");
    let real_before = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();

    // Edit inside a replacement span (rename the alias itself) via a raw mirror
    // edit; the bridge must refuse and leave the real file untouched.
    let alias = alias_of(repo.path(), "dangerous");
    let mirror = fs::read_to_string(&mirror_path).unwrap();
    let edited = mirror.replace(&format!("{alias}_parser"), "pleasant_parser");
    assert_ne!(mirror, edited);
    fs::write(&mirror_path, &edited).unwrap();

    let err = code_sanity::project_mirror_edit(
        repo.path(),
        Path::new("src/lib.rs"),
        code_sanity::patch::ApplyOptions::default(),
    )
    .unwrap_err();
    let chain = format!("{err:#}");
    assert!(chain.contains("replacement span"), "{chain}");
    // The displaced edit is kept as a durable stash and referenced in the error.
    assert!(chain.contains("the edit is kept at"), "{chain}");
    assert_eq!(
        fs::read_to_string(repo.path().join("src/lib.rs")).unwrap(),
        real_before
    );
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn opencode_bridge_creates_real_file_from_new_mirror_file() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let new_mirror_path = repo.path().join(".code-sanity/mirror/src/created.rs");
    fs::write(
        &new_mirror_path,
        "pub fn created_ok() -> usize {\n    2\n}\n",
    )
    .unwrap();

    code_sanity::project_mirror_edit(
        repo.path(),
        Path::new("src/created.rs"),
        code_sanity::patch::ApplyOptions::default(),
    )
    .unwrap();

    let real = fs::read_to_string(repo.path().join("src/created.rs")).unwrap();
    assert_eq!(real, "pub fn created_ok() -> usize {\n    2\n}\n");
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn mcp_server_reads_sanitized_and_applies_patch() {
    use serde_json::{Value, json};
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();

    let alias = alias_of(repo.path(), "dangerous");
    let patch = format!(
        "--- a/src/lib.rs\n\
         +++ b/src/lib.rs\n\
         @@ -2,3 +2,3 @@\n \
         fn {alias}_parser() -> usize {{\n\
         -    1\n\
         +    2\n \
         }}\n"
    );
    let requests = [
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"read_file","arguments":{"path":"src/lib.rs"}}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"apply_patch","arguments":{"patch":patch,"agent":"mcp"}}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"verify","arguments":{}}}),
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"list_files","arguments":{}}}),
    ];
    let input = requests
        .iter()
        .map(|request| request.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let mut out = Vec::new();
    code_sanity::mcp::serve(
        repo.path(),
        std::io::Cursor::new(input.into_bytes()),
        &mut out,
    )
    .unwrap();
    let responses: Vec<Value> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();

    // read_file returns sanitized content only.
    let read_text = responses[0]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(read_text.contains(&format!("fn {alias}_parser()")));
    assert!(!read_text.contains("dangerous_parser"));
    assert_eq!(responses[0]["result"]["isError"], false);

    // apply_patch projects to the real repo through the bridge.
    let apply_text = responses[1]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(apply_text.contains("applied files=src/lib.rs"));
    let real = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    assert!(real.contains("    2"));
    assert!(real.contains("fn dangerous_parser()"));

    assert_eq!(responses[2]["result"]["isError"], false);
    let list_text = responses[3]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(list_text.contains("src/lib.rs"));
}

#[test]
fn cli_serve_once_prints_tool_manifest() {
    let repo = copy_fixture("basic-rust");
    Command::cargo_bin("code-sanity")
        .unwrap()
        .args(["--root", repo.path().to_str().unwrap(), "serve", "--once"])
        .assert()
        .success()
        .stdout(predicate::str::contains("read_file"))
        .stdout(predicate::str::contains("apply_patch"))
        .stdout(predicate::str::contains("inputSchema"));
}

#[test]
fn codex_and_claude_hooks_generate_and_verify() {
    use serde_json::Value;
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let root = repo.path().to_str().unwrap();

    for agent in ["codex", "claude"] {
        Command::cargo_bin("code-sanity")
            .unwrap()
            .args(["--root", root, "install-hooks", "--agent", agent])
            .assert()
            .success();
        Command::cargo_bin("code-sanity")
            .unwrap()
            .args(["--root", root, "doctor", "--agent", agent])
            .assert()
            .success()
            .stdout(predicate::str::contains("installed=true"));
    }

    // Generated configs are valid JSON.
    let codex_hooks = fs::read_to_string(repo.path().join(".codex/hooks.json")).unwrap();
    serde_json::from_str::<Value>(&codex_hooks).unwrap();
    let claude_settings = fs::read_to_string(repo.path().join(".claude/settings.json")).unwrap();
    serde_json::from_str::<Value>(&claude_settings).unwrap();
    assert!(repo.path().join(".claude/hooks/session_start.py").exists());
}

#[test]
fn install_hooks_merges_and_uninstall_preserves_foreign_settings() {
    use serde_json::Value;
    let repo = copy_fixture("basic-rust");
    let root = repo.path().to_str().unwrap();

    // Pre-existing settings.json with foreign keys and a foreign hook.
    let settings_path = repo.path().join(".claude/settings.json");
    fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
    let foreign = serde_json::json!({
        "permissions": { "allow": ["Bash(cargo:*)"] },
        "env": { "FOO": "bar" },
        "hooks": {
            "PreToolUse": [
                { "matcher": "Bash", "hooks": [ { "type": "command", "command": "echo foreign" } ] }
            ]
        }
    });
    fs::write(
        &settings_path,
        serde_json::to_string_pretty(&foreign).unwrap(),
    )
    .unwrap();

    Command::cargo_bin("code-sanity")
        .unwrap()
        .args(["--root", root, "install-hooks", "--agent", "claude"])
        .assert()
        .success();

    let merged: Value = serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
    // Foreign keys and hooks survive the merge.
    assert_eq!(merged["permissions"]["allow"][0], "Bash(cargo:*)");
    assert_eq!(merged["env"]["FOO"], "bar");
    let pre = merged["hooks"]["PreToolUse"].as_array().unwrap();
    assert!(
        pre.iter()
            .any(|entry| entry["hooks"][0]["command"] == "echo foreign")
    );
    assert!(pre.iter().any(|entry| {
        entry["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("pre_tool_use.py")
    }));
    // A backup of the pre-merge file exists.
    assert!(repo.path().join(".claude/settings.json.bak").exists());
    // Idempotent: reinstalling does not duplicate entries.
    Command::cargo_bin("code-sanity")
        .unwrap()
        .args(["--root", root, "install-hooks", "--agent", "claude"])
        .assert()
        .success();
    let again: Value = serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
    assert_eq!(
        again["hooks"]["PreToolUse"].as_array().unwrap().len(),
        pre.len()
    );

    // Uninstall removes our entries and scripts but keeps foreign config.
    Command::cargo_bin("code-sanity")
        .unwrap()
        .args(["--root", root, "uninstall-hooks", "--agent", "claude"])
        .assert()
        .success();
    let stripped: Value =
        serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
    assert_eq!(stripped["permissions"]["allow"][0], "Bash(cargo:*)");
    let pre_after = stripped["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre_after.len(), 1);
    assert_eq!(pre_after[0]["hooks"][0]["command"], "echo foreign");
    assert!(!repo.path().join(".claude/hooks/pre_tool_use.py").exists());
    assert!(!repo.path().join(".claude/hooks/post_tool_use.py").exists());
}

#[test]
fn post_hook_projects_mirror_edit_then_syncs_only_that_path() {
    let Some(py) = python3_bin() else {
        eprintln!("skipping: python3 not available");
        return;
    };
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let root = repo.path().to_str().unwrap();
    Command::cargo_bin("code-sanity")
        .unwrap()
        .args(["--root", root, "install-hooks", "--agent", "claude"])
        .assert()
        .success();

    // The agent edits the mirror file in place (outside a replacement span).
    let mirror_path = repo.path().join(".code-sanity/mirror/src/lib.rs");
    let mirror = fs::read_to_string(&mirror_path).unwrap();
    fs::write(&mirror_path, mirror.replace("    1\n", "    8\n")).unwrap();

    // The PostToolUse hook receives the edited mirror path and must run
    // project-edit first so the real file gets the change.
    let bin = assert_cmd::cargo::cargo_bin("code-sanity");
    let hook = repo.path().join(".claude/hooks/post_tool_use.py");
    let payload = serde_json::json!({
        "tool_name": "Edit",
        "tool_input": { "file_path": ".code-sanity/mirror/src/lib.rs" },
        "cwd": root,
    });
    use std::io::Write as _;
    use std::process::{Command as StdCommand, Stdio};
    let mut child = StdCommand::new(py)
        .arg(&hook)
        .current_dir(repo.path())
        .env("CODE_SANITY_BIN", &bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());

    let real = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    assert!(real.contains("    8"), "real: {real}");
    assert!(real.contains("fn dangerous_parser()"));
    assert!(verify_workspace(repo.path()).is_ok());
    // No swallowed failures: the log stays empty on the happy path.
    let log = repo.path().join(".code-sanity/logs/hooks.log");
    if log.exists() {
        let body = fs::read_to_string(&log).unwrap();
        assert!(body.is_empty(), "unexpected hook errors: {body}");
    }
}

#[test]
fn codex_and_claude_hooks_enforce_strict_mode() {
    let Some(py) = python3_bin() else {
        eprintln!("skipping: python3 not available");
        return;
    };
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let root = repo.path().to_str().unwrap();
    Command::cargo_bin("code-sanity")
        .unwrap()
        .args(["--root", root, "install-hooks", "--agent", "codex"])
        .assert()
        .success();
    Command::cargo_bin("code-sanity")
        .unwrap()
        .args(["--root", root, "install-hooks", "--agent", "claude"])
        .assert()
        .success();
    set_mode(repo.path(), "strict");

    let cwd = serde_json::Value::String(root.to_string());
    let codex_pre = repo.path().join(".codex/hooks/pre_tool_use.py");
    let claude_pre = repo.path().join(".claude/hooks/pre_tool_use.py");

    // Codex: raw real-repo edit is denied in strict.
    let deny = run_hook(
        py,
        &codex_pre,
        repo.path(),
        &serde_json::json!({"tool_name":"Edit","tool_input":{"file_path":"src/lib.rs"},"cwd":cwd})
            .to_string(),
    );
    assert!(deny.contains("\"deny\""), "codex deny: {deny}");

    // Codex: editing the mirror is allowed.
    let allow = run_hook(
        py,
        &codex_pre,
        repo.path(),
        &serde_json::json!({"tool_name":"Edit","tool_input":{"file_path":".code-sanity/mirror/src/lib.rs"},"cwd":cwd})
            .to_string(),
    );
    assert!(allow.contains("\"allow\""), "codex allow: {allow}");
    assert!(
        !allow.contains("deny"),
        "codex mirror edit not denied: {allow}"
    );

    // Codex: obvious shell reads are redirected to the mirror.
    let redirect = run_hook(
        py,
        &codex_pre,
        repo.path(),
        &serde_json::json!({"tool_name":"bash","tool_input":{"command":"cat src/lib.rs"},"cwd":cwd})
            .to_string(),
    );
    assert!(
        redirect.contains("code-sanity read src/lib.rs"),
        "codex redirect: {redirect}"
    );

    // Claude: raw real-repo read is denied in strict.
    let claude_deny = run_hook(
        py,
        &claude_pre,
        repo.path(),
        &serde_json::json!({"tool_name":"Read","tool_input":{"file_path":"src/lib.rs"},"cwd":cwd})
            .to_string(),
    );
    assert!(claude_deny.contains("deny"), "claude deny: {claude_deny}");

    // Claude: reading the mirror is allowed (no deny emitted).
    let claude_allow = run_hook(
        py,
        &claude_pre,
        repo.path(),
        &serde_json::json!({"tool_name":"Read","tool_input":{"file_path":".code-sanity/mirror/src/lib.rs"},"cwd":cwd})
            .to_string(),
    );
    assert!(
        claude_allow.trim().is_empty(),
        "claude mirror read should be allowed: {claude_allow}"
    );
}

#[test]
fn external_model_proposals_validated_queued_and_applied_on_approval() {
    use code_sanity::config::{Config, Layout, ProviderConfig};
    let Some(py) = python3_bin() else {
        eprintln!("skipping: python3 not available");
        return;
    };
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();

    // A fake offline "model": reads the file payload, emits a fixed proposal set
    // covering one valid, one invalid-identifier, and one not-in-file case.
    let script_dir = tempfile::tempdir().unwrap();
    let script = script_dir.path().join("fake_model.py");
    fs::write(
        &script,
        "import json,sys\npayload=json.load(sys.stdin)\nif payload.get(\"request_mode\") == \"path-only\":\n print(json.dumps({\"proposals\":[]}))\nelse:\n print(json.dumps({\"proposals\":[\n  {\"category\":\"identifier\",\"original_text\":\"safe_helper\",\"sanitized_text\":\"assist_helper\",\"confidence\":0.95},\n  {\"category\":\"identifier\",\"original_text\":\"safe_helper\",\"sanitized_text\":\"9invalid\",\"confidence\":0.95},\n  {\"category\":\"identifier\",\"original_text\":\"ghost_term\",\"sanitized_text\":\"foo\",\"confidence\":0.95}\n ]}))\n",
    )
    .unwrap();

    let layout = Layout::new(repo.path());
    let mut config = Config::load_or_default(&layout).unwrap();
    config.sanitizer.provider = ProviderConfig::External {
        command: vec![py.to_string(), script.to_str().unwrap().to_string()],
        timeout_secs: Some(60),
    };
    config.save(&layout).unwrap();

    // Repo-supplied provider commands require explicit confirmation.
    let refused = code_sanity::proposal::propose_sanitize(
        repo.path(),
        Some(Path::new("src/lib.rs")),
        code_sanity::proposal::ProviderAllow::default(),
    )
    .unwrap_err();
    assert!(refused.to_string().contains("--allow-provider-command"));

    let report = code_sanity::proposal::propose_sanitize(
        repo.path(),
        Some(Path::new("src/lib.rs")),
        code_sanity::proposal::ProviderAllow {
            command: true,
            endpoint: false,
        },
    )
    .unwrap();
    assert_eq!(report.proposed, 3);
    assert_eq!(report.queued, 1);
    assert_eq!(report.rejected.len(), 2);

    // The model never wrote the mirror.
    let mirror_before = read_sanitized_file(repo.path(), Path::new("src/lib.rs")).unwrap();
    assert!(mirror_before.contains("safe_helper"));
    assert!(!mirror_before.contains("assist_helper"));

    // Approve the surviving proposal -> symbol-scoped v2 projection applies it.
    let items = code_sanity::proposal::list_review(repo.path(), false).unwrap();
    assert_eq!(items.len(), 1);
    code_sanity::proposal::resolve_review(repo.path(), &items[0].id, true).unwrap();

    let conn = code_sanity::db::connect(&layout).unwrap();
    let projected =
        code_sanity::semantic_store::project_document(&conn, repo.path(), "src/lib.rs").unwrap();
    assert!(projected.content.contains("fn assist_helper()"));
    assert!(!projected.content.contains("fn safe_helper"));
    let mirror_after = read_sanitized_file(repo.path(), Path::new("src/lib.rs")).unwrap();
    assert!(mirror_after.contains("fn assist_helper()"));
    assert!(!mirror_after.contains("fn safe_helper()"));
    let real = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    assert!(real.contains("fn safe_helper")); // real symbol untouched
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn alias_registry_is_applied_deterministically_across_files() {
    use code_sanity::config::{Config, Layout};
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("src/a.rs"),
        "fn widgetname() -> usize {\n    1\n}\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("src/b.rs"),
        "fn use_it() -> usize {\n    widgetname()\n}\n",
    )
    .unwrap();
    index_workspace(repo.path()).unwrap();

    let layout = Layout::new(repo.path());
    let mut config = Config::load_or_default(&layout).unwrap();
    config
        .sanitizer
        .alias_registry
        .insert("widgetname".to_string(), "gadget".to_string());
    config.save(&layout).unwrap();
    index_workspace(repo.path()).unwrap();

    let a = read_sanitized_file(repo.path(), Path::new("src/a.rs")).unwrap();
    let b = read_sanitized_file(repo.path(), Path::new("src/b.rs")).unwrap();
    assert!(a.contains("fn gadget()"));
    assert!(b.contains("gadget()"));
    assert!(!a.contains("widgetname"));
    assert!(!b.contains("widgetname"));
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn heuristic_provider_queues_denylist_terms() {
    use code_sanity::config::{Config, Layout};
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let layout = Layout::new(repo.path());
    let mut config = Config::load_or_default(&layout).unwrap();
    config.sanitizer.denylist = vec!["safe_helper".to_string()];
    config.save(&layout).unwrap();

    let report = code_sanity::proposal::propose_sanitize(
        repo.path(),
        Some(Path::new("src/lib.rs")),
        code_sanity::proposal::ProviderAllow::default(),
    )
    .unwrap();
    assert_eq!(report.queued, 1);
    let items = code_sanity::proposal::list_review(repo.path(), false).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].proposal.original_text, "safe_helper");
    assert!(items[0].flag.contains("confidence"));
}

#[test]
fn review_sanitize_reports_applied_replacements() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    Command::cargo_bin("code-sanity")
        .unwrap()
        .args(["--root", repo.path().to_str().unwrap(), "review-sanitize"])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "dangerous -> {}",
            alias_of(repo.path(), "dangerous")
        )))
        .stdout(predicate::str::contains("static-dictionary"));
}

#[test]
#[cfg(unix)]
fn strict_sh_sanitizes_command_output() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    // The command runs in the real repo, but its output is reverse-mapped so
    // real identifiers never reach the caller.
    Command::cargo_bin("code-sanity")
        .unwrap()
        .args([
            "--root",
            repo.path().to_str().unwrap(),
            "sh",
            "--",
            "/bin/echo",
            "dangerous_parser",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "{}_parser",
            alias_of(repo.path(), "dangerous")
        )))
        .stdout(predicate::str::contains("dangerous").not());
}

#[test]
#[cfg(unix)]
fn strict_run_reads_sanitized_worktree() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    // strict-run reads the file from a sanitized worktree, so even a raw `cat`
    // only ever sees sanitized content.
    Command::cargo_bin("code-sanity")
        .unwrap()
        .args([
            "--root",
            repo.path().to_str().unwrap(),
            "strict-run",
            "--",
            "cat",
            "src/lib.rs",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "fn {}_parser()",
            alias_of(repo.path(), "dangerous")
        )))
        .stdout(predicate::str::contains("dangerous_parser").not());
}

#[test]
#[cfg(unix)]
fn strict_sh_streams_output_before_command_finishes() {
    use std::io::BufRead as _;
    use std::sync::mpsc;
    use std::time::Duration;
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();

    // The child prints one line, then sleeps well past our read deadline. If
    // output were buffered until exit (the old Command::output behavior), the
    // first line would not arrive in time.
    let bin = assert_cmd::cargo::cargo_bin("code-sanity");
    let mut child = std::process::Command::new(bin)
        .args([
            "--root",
            repo.path().to_str().unwrap(),
            "sh",
            "--",
            "/bin/sh",
            "-c",
            "echo dangerous_parser; sleep 5",
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        std::io::BufReader::new(stdout).read_line(&mut line).ok();
        tx.send(line).ok();
    });
    let line = rx
        .recv_timeout(Duration::from_secs(3))
        .expect("no output within 3s; strict sh is not streaming");
    let alias = alias_of(repo.path(), "dangerous");
    assert!(line.contains(&format!("{alias}_parser")), "line: {line}");
    assert!(!line.contains("dangerous"));
    child.kill().ok();
    child.wait().ok();
}

#[test]
fn search_results_are_capped_with_truncation_notice() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    let mut body = String::new();
    for i in 0..40 {
        body.push_str(&format!("fn needle_{i}() -> usize {{ {i} }}\n"));
    }
    fs::write(repo.path().join("src/lib.rs"), body).unwrap();
    index_workspace(repo.path()).unwrap();

    let (hits, truncated) =
        code_sanity::search::search_mirror_limited(repo.path(), "needle_", None, Some(10)).unwrap();
    assert_eq!(hits.len(), 10);
    assert!(truncated);

    Command::cargo_bin("code-sanity")
        .unwrap()
        .args([
            "--root",
            repo.path().to_str().unwrap(),
            "grep",
            "needle_",
            "--max-results",
            "10",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("truncated to 10 results"));
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

    let alias = alias_of(repo.path(), "dangerous");
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
        .stdout(predicate::str::contains(format!("{alias}_parser")));

    Command::cargo_bin("code-sanity")
        .unwrap()
        .args([
            "--root",
            repo.path().to_str().unwrap(),
            "grep",
            &format!("{alias}_parser"),
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

#[test]
fn crlf_file_roundtrips_through_the_patch_bridge() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    // A CRLF file containing a dictionary term (dangerous -> neutral).
    fs::write(
        repo.path().join("src/win.rs"),
        "// dangerous note\r\nfn calc() -> u32 {\r\n    1\r\n}\r\n",
    )
    .unwrap();
    index_workspace(repo.path()).unwrap();
    let alias = alias_of(repo.path(), "dangerous");
    let mirror = read_sanitized_file(repo.path(), Path::new("src/win.rs")).unwrap();
    assert!(
        mirror.contains(&format!("// {alias} note\r\n")),
        "{mirror:?}"
    );

    // Whole-file write through the bridge: edit one line of the mirror.
    let edited = mirror.replace("    1\r\n", "    2\r\n");
    assert_ne!(mirror, edited);
    write_sanitized_content(repo.path(), Path::new("src/win.rs"), &edited).unwrap();

    let real = fs::read_to_string(repo.path().join("src/win.rs")).unwrap();
    assert_eq!(
        real,
        "// dangerous note\r\nfn calc() -> u32 {\r\n    2\r\n}\r\n"
    );
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn diff_u0_insertion_lands_after_the_anchor_line() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("src/a.rs"),
        "fn one() {}\nfn two() {}\nfn three() {}\n",
    )
    .unwrap();
    index_workspace(repo.path()).unwrap();
    // Minimal-context insertion exactly as `diff -U0` emits it: insert AFTER
    // line 1.
    let patch = "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,0 +2 @@\n+fn inserted() {}\n";
    apply_patch_text(repo.path(), patch).unwrap();
    let real = fs::read_to_string(repo.path().join("src/a.rs")).unwrap();
    assert_eq!(
        real,
        "fn one() {}\nfn inserted() {}\nfn two() {}\nfn three() {}\n"
    );
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn added_prose_comment_stays_verbatim_in_real_file() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let alias = alias_of(repo.path(), "dangerous");
    let mirror = read_sanitized_file(repo.path(), Path::new("src/lib.rs")).unwrap();
    // Plain-language words (including the alias STEM "neutral", which under
    // synthetic defaults is not an alias by itself) must land verbatim in the
    // real file — reverse mapping never rewrites prose.
    let edited = mirror.replace(
        &format!("fn {alias}_parser()"),
        &format!("// stay neutral here\nfn {alias}_parser()"),
    );
    assert_ne!(mirror, edited);
    write_sanitized_content(repo.path(), Path::new("src/lib.rs"), &edited).unwrap();
    let real = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    assert!(
        real.contains("// stay neutral here"),
        "comment was reverse-mapped into a real term: {real}"
    );
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn added_prose_containing_the_exact_alias_conflicts() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let alias = alias_of(repo.path(), "dangerous");
    let real_before = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    let mirror = read_sanitized_file(repo.path(), Path::new("src/lib.rs")).unwrap();
    // Prose is never reverse-mapped, so the EXACT alias in a comment would
    // land verbatim in the real file and make the mirror ambiguous. The
    // bridge must refuse with a collision conflict, not write it.
    let edited = mirror.replace(
        &format!("fn {alias}_parser()"),
        &format!("// see {alias} for details\nfn {alias}_parser()"),
    );
    assert_ne!(mirror, edited);
    let err = write_sanitized_content(repo.path(), Path::new("src/lib.rs"), &edited).unwrap_err();
    let chain = format!("{err:#}");
    assert!(chain.contains("ambiguous"), "{chain}");
    assert_eq!(
        fs::read_to_string(repo.path().join("src/lib.rs")).unwrap(),
        real_before,
        "real file must stay untouched on conflict"
    );
    // Recovery: the conflict left the mirror holding the refused edit; reset.
    Command::cargo_bin("code-sanity")
        .unwrap()
        .args(["--root", repo.path().to_str().unwrap(), "sync", "--force"])
        .assert()
        .success();
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn removed_sql_comment_projects_end_to_end() {
    let repo = tempfile::tempdir().unwrap();
    fs::write(
        repo.path().join("q.sql"),
        "select 1;\n-- dangerous audit trail\nselect 2;\n",
    )
    .unwrap();
    index_workspace(repo.path()).unwrap();
    let mirror = read_sanitized_file(repo.path(), Path::new("q.sql")).unwrap();
    let edited = mirror
        .lines()
        .filter(|line| !line.starts_with("--"))
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    write_sanitized_content(repo.path(), Path::new("q.sql"), &edited).unwrap();
    let real = fs::read_to_string(repo.path().join("q.sql")).unwrap();
    assert_eq!(real, "select 1;\nselect 2;\n");
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn disjoint_whole_file_edits_straddling_an_alias_apply_without_conflict() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    // The alias for dangerous_parser sits between the two edited lines.
    fs::write(
        repo.path().join("src/lib.rs"),
        "fn top() -> u32 {\n    1\n}\nfn dangerous_parser() {}\nfn bottom() -> u32 {\n    1\n}\n",
    )
    .unwrap();
    index_workspace(repo.path()).unwrap();
    let alias = alias_of(repo.path(), "dangerous");
    let mirror = read_sanitized_file(repo.path(), Path::new("src/lib.rs")).unwrap();
    assert!(mirror.contains(&format!("{alias}_parser")));
    // Two disjoint edits, one above and one below the alias line.
    let edited = mirror.replacen("    1\n", "    10\n", 1);
    let pos = edited.rfind("    1\n").unwrap();
    let mut edited2 = edited.clone();
    edited2.replace_range(pos..pos + "    1\n".len(), "    20\n");
    assert_ne!(mirror, edited2);
    write_sanitized_content(repo.path(), Path::new("src/lib.rs"), &edited2).unwrap();
    let real = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    assert!(real.contains("    10\n"), "{real}");
    assert!(real.contains("    20\n"), "{real}");
    assert!(real.contains("fn dangerous_parser() {}"), "{real}");
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn added_common_word_is_not_reverse_mapped_under_synthetic_defaults() {
    // THE reproduced audit scenario: repo has `fn acme_handler`, agent adds
    // `let client = 5;`. Under the old defaults (acme -> client) the reverse
    // mapper rewrote it to `let acme = 5;` — silent corruption. Synthetic
    // default aliases (client_xxxx) leave the plain English word alone.
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("src/a.rs"),
        "fn acme_handler() -> usize {\n    1\n}\n",
    )
    .unwrap();
    index_workspace(repo.path()).unwrap();
    let alias = alias_of(repo.path(), "acme");
    let mirror = read_sanitized_file(repo.path(), Path::new("src/a.rs")).unwrap();
    assert!(mirror.contains(&format!("{alias}_handler")), "{mirror}");

    let patch = format!(
        "--- a/src/a.rs\n\
         +++ b/src/a.rs\n\
         @@ -1,3 +1,4 @@\n \
         fn {alias}_handler() -> usize {{\n\
         +    let client = 5;\n     \
         1\n \
         }}\n"
    );
    apply_patch_text(repo.path(), &patch).unwrap();
    let real = fs::read_to_string(repo.path().join("src/a.rs")).unwrap();
    assert!(
        real.contains("let client = 5;"),
        "agent's plain word was rewritten: {real}"
    );
    assert!(!real.contains("let acme = 5;"), "{real}");
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn verify_lists_config_violations_and_content_collisions() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("src/a.rs"),
        "fn acme_handler() {}\nlet gadget = 1;\n",
    )
    .unwrap();
    index_workspace(repo.path()).unwrap();

    // Hand-break the persisted config: a multi-token denylist term AND an
    // alias colliding with real content (config.save would refuse these, so
    // edit the TOML directly like a human would).
    let config_path = repo.path().join(".code-sanity/config.toml");
    let body = fs::read_to_string(&config_path)
        .unwrap()
        .lines()
        .map(|line| {
            if line.starts_with("acme = ") {
                "acme = \"gadget\"".to_string()
            } else if line.starts_with("denylist = ") {
                "denylist = [\"secret.internal.key\"]".to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&config_path, body).unwrap();

    // Mutating commands refuse the broken config outright...
    let err = index_workspace(repo.path()).unwrap_err();
    assert!(
        format!("{err:#}").contains("secret.internal.key"),
        "{err:#}"
    );

    // ...while verify (exit 3) LISTS everything: the unmatchable term and the
    // alias-vs-content collision.
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("code-sanity"))
        .arg("--root")
        .arg(repo.path())
        .arg("verify")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("secret.internal.key"), "stderr: {stderr}");
    assert!(stderr.contains("gadget"), "stderr: {stderr}");
    assert!(stderr.contains("ambiguous"), "stderr: {stderr}");
}

#[test]
fn resolve_review_refuses_alias_that_occurs_elsewhere_in_repo() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(repo.path().join("src/a.rs"), "fn shadowfax_launch() {}\n").unwrap();
    // The proposed alias occurs in ANOTHER file: approval must refuse before
    // the registry is persisted.
    fs::write(repo.path().join("src/b.rs"), "fn gadget() {}\n").unwrap();
    index_workspace(repo.path()).unwrap();

    let layout = code_sanity::Layout::new(repo.path());
    let mut config = code_sanity::Config::load_or_default(&layout).unwrap();
    config.sanitizer.denylist = vec!["shadowfax".to_string()];
    config.save(&layout).unwrap();
    // Reconverge under the new denylist so the mirror is consistent again.
    index_workspace(repo.path()).unwrap();

    // Queue a hand-written proposal (heuristic providers derive sym_ aliases,
    // which never collide; we need a colliding one).
    let item = code_sanity::proposal::ReviewItem {
        id: "2099-01-01T00-00-00.000000000Z-testtest".to_string(),
        file: "src/a.rs".to_string(),
        proposal: code_sanity::proposal::Proposal {
            target: None,
            category: "identifier".to_string(),
            original_text: "shadowfax".to_string(),
            sanitized_text: "gadget".to_string(),
            confidence: 1.0,
            rationale: None,
        },
        status: code_sanity::proposal::ReviewStatus::Pending,
        flag: "clean".to_string(),
        created_at: "2099-01-01T00:00:00Z".to_string(),
    };
    let review_dir = repo.path().join(".code-sanity/review");
    fs::create_dir_all(&review_dir).unwrap();
    fs::write(
        review_dir.join(format!("{}.json", item.id)),
        serde_json::to_string_pretty(&item).unwrap(),
    )
    .unwrap();

    let err = code_sanity::proposal::resolve_review(repo.path(), &item.id, true).unwrap_err();
    let chain = format!("{err:#}");
    assert!(chain.contains("gadget"), "{chain}");
    assert!(chain.contains("src/b.rs"), "{chain}");
    // Registry untouched, workspace still consistent.
    let config = code_sanity::Config::load_or_default(&layout).unwrap();
    assert!(config.sanitizer.alias_registry.is_empty());
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn dry_run_plans_without_writing() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let alias = alias_of(repo.path(), "dangerous");
    let real_before = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    let mirror_before = read_sanitized_file(repo.path(), Path::new("src/lib.rs")).unwrap();
    let patch = format!(
        "--- a/src/lib.rs\n\
         +++ b/src/lib.rs\n\
         @@ -2,3 +2,3 @@\n \
         fn {alias}_parser() -> usize {{\n\
         -    1\n\
         +    2\n \
         }}\n"
    );

    let patch_file = repo.path().join("patch.diff");
    fs::write(&patch_file, &patch).unwrap();
    Command::cargo_bin("code-sanity")
        .unwrap()
        .args([
            "--root",
            repo.path().to_str().unwrap(),
            "apply-patch",
            "--patch",
            patch_file.to_str().unwrap(),
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("dry-run ok"))
        .stdout(predicate::str::contains("src/lib.rs"));

    // Nothing changed, no Success journal entry.
    assert_eq!(
        fs::read_to_string(repo.path().join("src/lib.rs")).unwrap(),
        real_before
    );
    assert_eq!(
        read_sanitized_file(repo.path(), Path::new("src/lib.rs")).unwrap(),
        mirror_before
    );
    let listing =
        code_sanity::journal::list_journal_entries(&code_sanity::Layout::new(repo.path())).unwrap();
    assert!(listing.entries.is_empty(), "dry run journaled an apply");

    // The same patch applies for real afterwards.
    apply_patch_text(repo.path(), &patch).unwrap();
    assert!(
        fs::read_to_string(repo.path().join("src/lib.rs"))
            .unwrap()
            .contains("    2")
    );
}

#[test]
fn dry_run_conflict_still_exits_2() {
    let repo = copy_fixture("basic-rust");
    index_workspace(repo.path()).unwrap();
    let alias = alias_of(repo.path(), "dangerous");
    let real_before = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    // Editing inside a replacement span conflicts, dry run or not.
    let patch = format!(
        "--- a/src/lib.rs\n\
         +++ b/src/lib.rs\n\
         @@ -2,1 +2,1 @@\n\
         -fn {alias}_parser() -> usize {{\n\
         +fn pleasant_parser() -> usize {{\n"
    );
    let patch_file = repo.path().join("patch.diff");
    fs::write(&patch_file, &patch).unwrap();
    Command::cargo_bin("code-sanity")
        .unwrap()
        .args([
            "--root",
            repo.path().to_str().unwrap(),
            "apply-patch",
            "--patch",
            patch_file.to_str().unwrap(),
            "--dry-run",
        ])
        .assert()
        .code(2);
    assert_eq!(
        fs::read_to_string(repo.path().join("src/lib.rs")).unwrap(),
        real_before
    );
}

#[test]
fn mcp_success_output_carries_no_absolute_paths() {
    use serde_json::{Value, json};
    // The workspace root's directory name is exactly the kind of private
    // token the dictionary cannot know about.
    let outer = tempfile::tempdir().unwrap();
    let repo = outer.path().join("megacorp_private_root");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(repo.join("src/a.rs"), "fn plain() -> usize {\n    1\n}\n").unwrap();
    index_workspace(&repo).unwrap();

    let patch = "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,3 +1,3 @@\n fn plain() -> usize {\n-    1\n+    2\n }\n";
    let requests = [
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"apply_patch","arguments":{"patch":patch}}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"apply_patch","arguments":{"patch":patch,"dry_run":true}}}),
    ];
    let input = requests
        .iter()
        .map(|request| request.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let mut out = Vec::new();
    code_sanity::mcp::serve(&repo, std::io::Cursor::new(input.into_bytes()), &mut out).unwrap();
    let responses: Vec<Value> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();

    // Success: workspace-relative journal reference only.
    assert_eq!(responses[0]["result"]["isError"], false);
    let text = responses[0]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains("journal=.code-sanity/journal/"), "{text}");
    assert!(!text.contains("megacorp_private_root"), "leaked: {text}");

    // Dry run against the now-changed file: context mismatch -> tool error;
    // the error must not leak the absolute root either.
    let error_text = responses[1]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(
        !error_text.contains("megacorp_private_root"),
        "leaked in error: {error_text}"
    );
}

#[test]
fn usage_errors_exit_64_and_conflicts_keep_2() {
    // 2 is the documented "patch conflict" contract; a typo in the flags must
    // not read as a conflict. Usage errors take 64 (EX_USAGE).
    Command::cargo_bin("code-sanity")
        .unwrap()
        .arg("definitely-not-a-command")
        .assert()
        .code(64);
    Command::cargo_bin("code-sanity").unwrap().assert().code(64); // bare invocation = usage error too
    Command::cargo_bin("code-sanity")
        .unwrap()
        .arg("--help")
        .assert()
        .code(0)
        .stdout(predicate::str::contains("Apply a unified diff"));
    // (2-for-conflict is pinned by apply_patch_conflict_exits_with_code_2.)
}

#[test]
fn explicit_missing_root_is_a_clear_error() {
    Command::cargo_bin("code-sanity")
        .unwrap()
        .args(["--root", "/definitely/not/here", "verify"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--root"));
}

#[test]
fn cli_glob_matches_paths_and_rejects_invalid_patterns() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src/nested")).unwrap();
    fs::write(repo.path().join("top.rs"), "fn needle_top() {}\n").unwrap();
    fs::write(repo.path().join("src/direct.rs"), "fn needle_direct() {}\n").unwrap();
    fs::write(
        repo.path().join("src/nested/deep.rs"),
        "fn needle_deep() {}\n",
    )
    .unwrap();
    index_workspace(repo.path()).unwrap();

    // Path glob: direct children of src/ only (used to silently match NOTHING).
    let hits = search_mirror(repo.path(), "needle_", Some("src/*.rs")).unwrap();
    let paths: Vec<&str> = hits.iter().map(|hit| hit.rel_path.as_str()).collect();
    assert_eq!(paths, vec!["src/direct.rs"], "{paths:?}");

    // Name glob still matches at any depth.
    let hits = search_mirror(repo.path(), "needle_", Some("*.rs")).unwrap();
    assert_eq!(hits.len(), 3, "{hits:?}");

    // Invalid glob is a loud error.
    Command::cargo_bin("code-sanity")
        .unwrap()
        .args([
            "--root",
            repo.path().to_str().unwrap(),
            "grep",
            "needle_",
            "--glob",
            "[",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("glob"));
}
