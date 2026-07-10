//! End-to-end regression for the prose-leak: `collect_protected_identifiers`
//! used to apply declaration/import heuristics to raw content, so English
//! prose ("Data from shadowfax is loaded") and markdown bold (`__shadowfax__`)
//! added a denylisted term to the repo-wide protected set. The term then
//! survived verbatim in every mirror file — and `verify` exited 0, because
//! the backstop recomputes that same set.

use code_sanity::config::{Config, Layout};
use code_sanity::{index_workspace, verify_workspace};
use std::fs;

fn repo_with_denylisted_term() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    // Prose that trips both the line rule (`from` in the first two words) and
    // the dunder rule (markdown bold).
    fs::write(
        repo.path().join("README.md"),
        "Data from shadowfax is loaded nightly.\n\
         Teams use shadowfax for rollouts.\n\
         __shadowfax__ is the codename.\n",
    )
    .unwrap();
    // A comment mentioning the term, in a different file: the protected set
    // is repo-wide, so a leak in README used to leak here too.
    fs::write(
        repo.path().join("src/lib.rs"),
        "// shadowfax kill switch lives here\n// migrated from shadowfax_v1\nfn helper() {}\n",
    )
    .unwrap();
    // Unknown extension = code: `export` must still protect its name.
    fs::write(repo.path().join("deploy.sh"), "export DEPLOY_TOKEN=x\n").unwrap();

    index_workspace(repo.path()).unwrap();
    let layout = Layout::new(repo.path());
    let mut config = Config::load_or_default(&layout).unwrap();
    config.sanitizer.denylist = vec!["shadowfax".to_string()];
    config.save(&layout).unwrap();
    index_workspace(repo.path()).unwrap();
    repo
}

#[test]
fn prose_and_comments_never_leak_a_denylisted_term_into_the_mirror() {
    let repo = repo_with_denylisted_term();
    let mirror = repo.path().join(".code-sanity/mirror");

    for rel in ["README.md", "src/lib.rs"] {
        let rendered = fs::read_to_string(mirror.join(rel)).unwrap();
        assert!(
            !rendered.to_lowercase().contains("shadowfax"),
            "{rel} leaked the denylisted term:\n{rendered}"
        );
    }
    // The markdown-bold run must be sanitized in place, keeping its emphasis.
    let readme = fs::read_to_string(mirror.join("README.md")).unwrap();
    assert!(
        readme.contains("__sym_"),
        "markdown bold must sanitize inside the dunder run:\n{readme}"
    );
    // The leak backstop agrees — it no longer sanctions the prose residue.
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn code_declarations_in_unknown_extensions_stay_protected() {
    let repo = repo_with_denylisted_term();
    let deploy =
        fs::read_to_string(repo.path().join(".code-sanity/mirror").join("deploy.sh")).unwrap();
    assert!(
        deploy.contains("DEPLOY_TOKEN"),
        "an `export` name in a shell script must stay real:\n{deploy}"
    );
}
