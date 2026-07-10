//! A missing config.toml on an initialized workspace must be a hard error —
//! never a silent regeneration. The config holds the workspace salt and the
//! human-approved alias registry; regenerating defaults would re-render the
//! whole mirror WITHOUT the user's sanitization policy, surfacing previously
//! hidden terms in the agent-facing view with exit 0.

use code_sanity::{apply_patch_text, index_workspace, init_workspace, verify_workspace};
use std::fs;
use std::path::Path;

fn init_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    fs::write(
        repo.path().join("lib.rs"),
        "// dangerous comment\nfn calc() -> usize {\n    1\n}\n",
    )
    .unwrap();
    index_workspace(repo.path()).unwrap();
    repo
}

fn config_path(repo: &Path) -> std::path::PathBuf {
    repo.join(".code-sanity/config.toml")
}

#[test]
fn missing_config_on_initialized_workspace_is_a_hard_error() {
    let repo = init_repo();
    let mirror_path = repo.path().join(".code-sanity/mirror/lib.rs");
    let mirror_before = fs::read_to_string(&mirror_path).unwrap();
    fs::remove_file(config_path(repo.path())).unwrap();

    for (what, err) in [
        ("index", index_workspace(repo.path()).unwrap_err()),
        ("init", init_workspace(repo.path()).unwrap_err()),
        (
            "apply",
            apply_patch_text(repo.path(), "--- a/x\n+++ b/x\n").unwrap_err(),
        ),
    ] {
        let message = format!("{err:#}");
        assert!(
            message.contains("config.toml is missing"),
            "{what}: the error must name the lost file, got: {message}"
        );
        // No .bak exists here (Config::save only writes one when it replaces
        // DIFFERENT content), so the remedy must not point at a missing file.
        assert!(
            !message.contains("config.toml.bak"),
            "{what}: must not name a .bak that does not exist, got: {message}"
        );
        assert!(
            message.contains("version control"),
            "{what}: the remedy must be actionable, got: {message}"
        );
    }

    // Nothing was regenerated and nothing was re-rendered.
    assert!(!config_path(repo.path()).exists(), "config was regenerated");
    assert_eq!(
        fs::read_to_string(&mirror_path).unwrap(),
        mirror_before,
        "mirror was re-rendered without the user's policy"
    );
}

#[test]
fn verify_reports_missing_config_as_failure() {
    let repo = init_repo();
    fs::remove_file(config_path(repo.path())).unwrap();

    let err = verify_workspace(repo.path()).unwrap_err();
    let failed = err
        .downcast_ref::<code_sanity::verify::VerifyFailed>()
        .expect("verify must fail with the typed exit-3 error");
    assert_eq!(failed.report.failures.len(), 1);
    assert!(failed.report.failures[0].contains("config.toml"));
    assert!(failed.report.failures[0].contains("version control"));
}

#[test]
fn restoring_the_backup_recovers_the_workspace() {
    let repo = init_repo();
    let config = config_path(repo.path());
    let salt_line = |body: &str| {
        body.lines()
            .find(|line| line.starts_with("salt"))
            .unwrap()
            .to_string()
    };
    let original_salt = salt_line(&fs::read_to_string(&config).unwrap());

    // Cause a .bak by saving a changed config, then lose the config file.
    let layout = code_sanity::Layout::new(repo.path());
    let mut loaded = code_sanity::Config::load_or_default(&layout).unwrap();
    loaded.sanitizer.allowlist.push("keepme".to_string());
    loaded.save(&layout).unwrap();
    assert!(config.with_file_name("config.toml.bak").exists());
    fs::remove_file(&config).unwrap();
    // With a .bak on disk the remedy names it.
    let err = format!("{:#}", index_workspace(repo.path()).unwrap_err());
    assert!(err.contains("config.toml.bak"), "{err}");

    // The documented remedy: restore from the .bak sibling.
    fs::rename(config.with_file_name("config.toml.bak"), &config).unwrap();
    index_workspace(repo.path()).unwrap();
    assert_eq!(
        salt_line(&fs::read_to_string(&config).unwrap()),
        original_salt,
        "restore must keep the original salt"
    );
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn fresh_directory_still_auto_initializes() {
    let repo = tempfile::tempdir().unwrap();
    fs::write(repo.path().join("lib.rs"), "fn calc() {}\n").unwrap();
    // No .code-sanity at all: index bootstraps a fresh workspace as before.
    index_workspace(repo.path()).unwrap();
    assert!(config_path(repo.path()).exists());
    assert!(verify_workspace(repo.path()).is_ok());
}
