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
fn denylist_term_that_is_a_public_name_is_refused_not_leaked() {
    // The protected set keeps public symbols real; the denylist keeps a term
    // away from the agent. When they collide, one promise must break silently
    // — so refuse both and let the human decide.
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("src/api.rs"),
        "pub fn shadowfax_client() -> usize {\n    1\n}\n",
    )
    .unwrap();
    index_workspace(repo.path()).unwrap();

    let layout = Layout::new(repo.path());
    let mut config = Config::load_or_default(&layout).unwrap();
    config.sanitizer.denylist = vec!["shadowfax".to_string()];
    config.save(&layout).unwrap();

    let err = index_workspace(repo.path()).unwrap_err().to_string();
    assert!(err.contains("shadowfax"), "{err}");
    assert!(err.contains("shadowfax_client"), "{err}");
    assert!(err.contains("src/api.rs"), "{err}");
    assert!(
        err.contains("allowlist"),
        "remedy must be actionable: {err}"
    );

    // verify reports it too, rather than sanctioning the residue.
    let verify_err = verify_workspace(repo.path()).unwrap_err();
    let failed = verify_err
        .downcast_ref::<code_sanity::verify::VerifyFailed>()
        .expect("must fail with the typed exit-3 error");
    assert!(
        failed
            .report
            .failures
            .iter()
            .any(|failure| failure.contains("shadowfax_client")),
        "{:?}",
        failed.report.failures
    );

    // Allowlisting the term is the documented way out.
    let mut config = Config::load_or_default(&layout).unwrap();
    config.sanitizer.allowlist.push("shadowfax".to_string());
    config.save(&layout).unwrap();
    index_workspace(repo.path()).unwrap();
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn dictionary_terms_in_public_names_stay_sanctioned_residues() {
    // Unlike the denylist, the default dictionary must never brick a repo
    // whose public API happens to contain one of its words.
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("src/api.rs"),
        "pub fn dangerous_parser() -> usize {\n    1\n}\n",
    )
    .unwrap();
    index_workspace(repo.path()).unwrap();
    let mirror = fs::read_to_string(repo.path().join(".code-sanity/mirror/src/api.rs")).unwrap();
    assert!(mirror.contains("dangerous_parser"), "{mirror}");
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
