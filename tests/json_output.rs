//! The `--json` machine-readable output contract: exactly one compact JSON
//! document on stdout per invocation (success or failure), stderr free for
//! human diagnostics, exit codes unchanged.

use assert_cmd::Command;
use serde_json::Value;
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
        } else {
            fs::copy(entry.path(), &next_dest)?;
        }
    }
    Ok(())
}

fn cli(repo: &Path) -> Command {
    let mut cmd = Command::cargo_bin("code-sanity").unwrap();
    cmd.args(["--root", repo.to_str().unwrap(), "--json"]);
    cmd
}

/// Parse stdout as the envelope, asserting it is exactly one line.
fn envelope(stdout: &[u8]) -> Value {
    let text = std::str::from_utf8(stdout).expect("stdout is UTF-8");
    let mut lines = text.lines();
    let line = lines.next().expect("stdout has one JSON line");
    assert!(
        lines.next().is_none(),
        "stdout must be exactly one line, got: {text:?}"
    );
    serde_json::from_str(line).expect("stdout parses as JSON")
}

fn index(repo: &Path) {
    Command::cargo_bin("code-sanity")
        .unwrap()
        .args(["--root", repo.to_str().unwrap(), "index"])
        .assert()
        .success();
}

#[test]
fn index_emits_one_parseable_envelope() {
    let repo = copy_fixture("basic-rust");
    let assert = cli(repo.path()).arg("index").assert().success();
    let value = envelope(&assert.get_output().stdout);
    assert_eq!(value["ok"], true);
    assert_eq!(value["command"], "index");
    assert!(value["elapsed_ms"].is_u64());
    let data = &value["data"];
    assert!(data["indexed"].as_u64().unwrap() > 0);
    assert!(data["errors"].is_array());
    assert!(data["stashed"].is_array());
}

#[test]
fn apply_patch_dry_run_and_real_apply_shapes() {
    let repo = copy_fixture("basic-rust");
    index(repo.path());
    let patch =
        "--- /dev/null\n+++ b/src/neutral_new.rs\n@@ -0,0 +1,1 @@\n+pub fn neutral_added() {}\n";

    let assert = cli(repo.path())
        .args(["apply-patch", "--dry-run"])
        .write_stdin(patch)
        .assert()
        .success();
    let value = envelope(&assert.get_output().stdout);
    assert_eq!(value["command"], "apply-patch");
    assert_eq!(value["data"]["dry_run"], true);
    assert_eq!(value["data"]["journal_path"], Value::Null);
    assert_eq!(value["data"]["files"][0], "src/neutral_new.rs");
    assert!(!repo.path().join("src/neutral_new.rs").exists());

    let assert = cli(repo.path())
        .arg("apply-patch")
        .write_stdin(patch)
        .assert()
        .success();
    let value = envelope(&assert.get_output().stdout);
    assert_eq!(value["data"]["dry_run"], false);
    assert!(value["data"]["journal_path"].is_string());
    assert!(repo.path().join("src/neutral_new.rs").exists());
}

#[test]
fn conflict_exits_2_with_error_envelope_and_journal() {
    let repo = copy_fixture("basic-rust");
    index(repo.path());
    // Creating a file that already exists is a conflict (exit 2).
    let patch = "--- /dev/null\n+++ b/src/lib.rs\n@@ -0,0 +1,1 @@\n+pub fn neutral_added() {}\n";
    let assert = cli(repo.path())
        .arg("apply-patch")
        .write_stdin(patch)
        .assert()
        .code(2);
    let output = assert.get_output();
    let value = envelope(&output.stdout);
    assert_eq!(value["ok"], false);
    assert_eq!(value["command"], "apply-patch");
    assert_eq!(value["error"]["kind"], "conflict");
    assert!(value["error"]["journal_path"].is_string());
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("already exists")
    );
    // The human rendering still lands on stderr.
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("conflict journal"),
        "stderr keeps the human message"
    );
}

#[test]
fn verify_failure_exits_3_with_failures_array() {
    let repo = copy_fixture("basic-rust");
    index(repo.path());
    fs::write(
        repo.path().join(".code-sanity/mirror/planted.rs"),
        "fn planted() {}\n",
    )
    .unwrap();
    let assert = cli(repo.path()).arg("verify").assert().code(3);
    let value = envelope(&assert.get_output().stdout);
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["kind"], "verify_failed");
    assert!(value["error"]["checked"].is_u64());
    let failures = value["error"]["failures"].as_array().unwrap();
    assert!(!failures.is_empty());
}

#[test]
fn generic_error_exits_1_with_error_envelope() {
    let repo = copy_fixture("basic-rust");
    index(repo.path());
    let assert = cli(repo.path())
        .args(["read", "missing.rs"])
        .assert()
        .code(1);
    let value = envelope(&assert.get_output().stdout);
    assert_eq!(value["ok"], false);
    assert_eq!(value["command"], "read");
    assert_eq!(value["error"]["kind"], "error");
    assert!(!value["error"]["message"].as_str().unwrap().is_empty());
}

#[test]
fn read_roundtrips_mirror_content_exactly() {
    let repo = copy_fixture("basic-rust");
    index(repo.path());
    let mirror = fs::read_to_string(repo.path().join(".code-sanity/mirror/src/lib.rs")).unwrap();
    let assert = cli(repo.path())
        .args(["read", "src/lib.rs"])
        .assert()
        .success();
    let value = envelope(&assert.get_output().stdout);
    assert_eq!(value["data"]["path"], "src/lib.rs");
    assert_eq!(
        value["data"]["content"].as_str().unwrap(),
        mirror,
        "content must match the mirror byte-for-byte (incl. trailing newline)"
    );
}

#[test]
fn search_reports_hits_and_truncation_with_pure_stdout() {
    let repo = copy_fixture("basic-rust");
    index(repo.path());
    let assert = cli(repo.path())
        .args(["search", "fn", "--max-results", "1"])
        .assert()
        .success();
    let output = assert.get_output();
    let value = envelope(&output.stdout);
    assert_eq!(value["data"]["truncated"], true);
    let hit = &value["data"]["hits"][0];
    assert!(hit["rel_path"].is_string());
    assert!(hit["line"].is_u64());
    assert!(hit["column"].is_u64());
    assert!(hit["line_text"].is_string());
    // The truncation note stays on stderr; stdout stays a single JSON line
    // (asserted by `envelope`).
    assert!(String::from_utf8_lossy(&output.stderr).contains("truncated"));
}

#[test]
fn mode_doctor_and_review_list_shapes() {
    let repo = copy_fixture("basic-rust");
    index(repo.path());

    let assert = cli(repo.path()).arg("mode").assert().success();
    assert_eq!(
        envelope(&assert.get_output().stdout)["data"]["mode"],
        "guided"
    );

    let assert = cli(repo.path()).arg("doctor").assert().success();
    let value = envelope(&assert.get_output().stdout);
    assert_eq!(value["command"], "doctor");
    assert_eq!(value["data"]["state_dir"]["exists"], true);
    assert_eq!(value["data"]["agent"], Value::Null);

    let assert = cli(repo.path()).arg("review").assert().success();
    let value = envelope(&assert.get_output().stdout);
    assert!(value["data"]["items"].as_array().unwrap().is_empty());
}

#[test]
fn corrupt_db_error_names_the_remedy() {
    let repo = copy_fixture("basic-rust");
    index(repo.path());
    fs::write(
        repo.path().join(".code-sanity/db.sqlite"),
        "this is not a sqlite database at all",
    )
    .unwrap();
    // Remove WAL sidecars so sqlite reads the corrupt main file.
    for sidecar in ["db.sqlite-wal", "db.sqlite-shm"] {
        let _ = fs::remove_file(repo.path().join(".code-sanity").join(sidecar));
    }
    let assert = cli(repo.path()).arg("verify").assert().code(1);
    let output = assert.get_output();
    let value = envelope(&output.stdout);
    assert_eq!(value["error"]["kind"], "error");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("delete .code-sanity/db.sqlite"),
        "corruption must name the remedy, got: {message}"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("db.sqlite"));
}

#[test]
fn sh_and_strict_run_reject_json_as_usage_error() {
    let repo = copy_fixture("basic-rust");
    for command in ["sh", "strict-run"] {
        let assert = cli(repo.path())
            .args([command, "--", "echo", "hi"])
            .assert()
            .code(64);
        let output = assert.get_output();
        assert!(
            output.stdout.is_empty(),
            "{command}: stdout must stay empty (child stream is not wrapped)"
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("--json is not supported"));
    }
}

#[test]
fn serve_rejects_json_as_usage_error() {
    // serve's stdout is the MCP stream (or the --once manifest), never the
    // envelope; the doc comment on --json promises the refusal.
    let repo = copy_fixture("basic-rust");
    for args in [vec!["serve"], vec!["serve", "--once"]] {
        let assert = cli(repo.path()).args(&args).assert().code(64);
        let output = assert.get_output();
        assert!(
            output.stdout.is_empty(),
            "{args:?}: stdout must stay empty (protocol stream is not wrapped)"
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("--json is not supported"));
    }
}

#[test]
fn bad_explicit_root_still_emits_the_error_envelope() {
    // Pre-dispatch failures are still post-clap: the machine contract holds.
    let assert = Command::cargo_bin("code-sanity")
        .unwrap()
        .args(["--root", "/nonexistent-code-sanity-root", "--json", "index"])
        .assert()
        .code(1);
    let value = envelope(&assert.get_output().stdout);
    assert_eq!(value["ok"], false);
    assert_eq!(value["command"], "index");
    assert_eq!(value["error"]["kind"], "error");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--root")
    );
}

#[test]
fn human_output_is_unchanged_without_the_flag() {
    let repo = copy_fixture("basic-rust");
    let assert = Command::cargo_bin("code-sanity")
        .unwrap()
        .args(["--root", repo.path().to_str().unwrap(), "index"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.starts_with("indexed="),
        "human key=value format is the default: {stdout:?}"
    );
}
