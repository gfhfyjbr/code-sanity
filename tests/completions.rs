use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn zsh_completions_are_generated_from_the_cli_tree() {
    Command::new(assert_cmd::cargo::cargo_bin!("code-sanity"))
        .args(["completions", "zsh"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::starts_with("#compdef code-sanity\n"))
        .stdout(predicate::str::contains("_code-sanity()"))
        .stdout(predicate::str::contains("propose-sanitize:"))
        .stdout(predicate::str::contains("rename-symbol:"));
}

#[test]
fn completion_generation_does_not_read_the_workspace_or_dotenv() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(".env"), "BROKEN='secret\n").unwrap();

    Command::new(assert_cmd::cargo::cargo_bin!("code-sanity"))
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "completions",
            "zsh",
        ])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("#compdef code-sanity\n"));
}

#[test]
fn completions_reject_json_before_writing_a_script() {
    Command::new(assert_cmd::cargo::cargo_bin!("code-sanity"))
        .args(["--json", "completions", "zsh"])
        .assert()
        .code(64)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "--json is not supported for completions",
        ));
}
