use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn bare_invocation_requires_a_real_terminal_when_redirected() {
    Command::new(assert_cmd::cargo::cargo_bin!("code-sanity"))
        .assert()
        .code(64)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "interactive mode requires a terminal",
        ));
}

#[test]
fn json_without_a_subcommand_returns_an_error_envelope() {
    let assert = Command::new(assert_cmd::cargo::cargo_bin!("code-sanity"))
        .arg("--json")
        .assert()
        .failure();
    let value: serde_json::Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("valid JSON envelope");
    assert_eq!(value["ok"], false);
    assert_eq!(value["command"], "tui");
    assert_eq!(value["error"]["kind"], "error");
}

#[test]
fn help_documents_optional_command_dispatch() {
    Command::new(assert_cmd::cargo::cargo_bin!("code-sanity"))
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("[COMMAND]"));
}
