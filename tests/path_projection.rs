use code_sanity::{
    Config, Layout, apply_patch_text, index_workspace, init_workspace, project_mirror_edit,
    read_sanitized_file, search_mirror, verify_workspace,
};
use std::fs;
use std::io::Cursor;
use std::path::Path;

fn configured_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("src/dangerous_worker.mm"),
        "int value = 1;\n",
    )
    .unwrap();
    init_workspace(repo.path()).unwrap();
    let layout = Layout::new(repo.path());
    let mut config = Config::load_or_default(&layout).unwrap();
    config.sanitizer.dictionary.clear();
    config.sanitizer.alias_registry.clear();
    config.sanitizer.denylist.clear();
    config
        .sanitizer
        .dictionary
        .insert("dangerous".into(), "neutral_x1".into());
    config.save(&layout).unwrap();
    repo
}

#[test]
fn projected_filename_reads_patches_verifies_and_migrates() {
    let repo = configured_repo();
    let layout = Layout::new(repo.path());
    index_workspace(repo.path()).unwrap();

    let projected = Path::new("src/neutral_x1_worker.mm");
    assert!(layout.mirror_dir.join(projected).is_file());
    assert!(!layout.mirror_dir.join("src/dangerous_worker.mm").exists());
    assert_eq!(
        read_sanitized_file(repo.path(), projected).unwrap(),
        "int value = 1;\n"
    );
    // Host-side compatibility: an old caller may still pass the real path,
    // but the physical mirror and every returned path remain projected.
    assert_eq!(
        read_sanitized_file(repo.path(), Path::new("src/dangerous_worker.mm")).unwrap(),
        "int value = 1;\n"
    );

    apply_patch_text(
        repo.path(),
        "--- a/src/neutral_x1_worker.mm\n\
         +++ b/src/neutral_x1_worker.mm\n\
         @@ -1,1 +1,1 @@\n\
         -int value = 1;\n\
         +int value = 2;\n",
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(repo.path().join("src/dangerous_worker.mm")).unwrap(),
        "int value = 2;\n"
    );
    verify_workspace(repo.path()).unwrap();

    let mut config = Config::load_or_default(&layout).unwrap();
    config
        .sanitizer
        .dictionary
        .insert("dangerous".into(), "calm_y2".into());
    config.save(&layout).unwrap();
    index_workspace(repo.path()).unwrap();
    assert!(layout.mirror_dir.join("src/calm_y2_worker.mm").is_file());
    assert!(!layout.mirror_dir.join(projected).exists());
    verify_workspace(repo.path()).unwrap();
}

#[test]
fn projected_directory_collision_fails_before_writing() {
    let repo = configured_repo();
    fs::create_dir_all(repo.path().join("neutral_x1")).unwrap();
    fs::write(repo.path().join("neutral_x1/other.mm"), "int other;\n").unwrap();
    fs::create_dir_all(repo.path().join("dangerous")).unwrap();
    fs::write(repo.path().join("dangerous/file.mm"), "int file;\n").unwrap();

    let err = index_workspace(repo.path()).unwrap_err();
    assert!(
        err.to_string().contains("path projection collision"),
        "{err:#}"
    );
}

#[test]
fn projected_directory_create_edit_and_delete_roundtrip() {
    let repo = configured_repo();
    fs::create_dir_all(repo.path().join("dangerous")).unwrap();
    fs::write(repo.path().join("dangerous/seed.mm"), "int seed = 1;\n").unwrap();
    let layout = Layout::new(repo.path());
    index_workspace(repo.path()).unwrap();

    apply_patch_text(
        repo.path(),
        "--- /dev/null\n\
         +++ b/neutral_x1/new_file.mm\n\
         @@ -0,0 +1,1 @@\n\
         +int created = 1;\n",
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(repo.path().join("dangerous/new_file.mm")).unwrap(),
        "int created = 1;\n"
    );

    let mirror = layout.mirror_dir.join("neutral_x1/new_file.mm");
    fs::write(&mirror, "int created = 2;\n").unwrap();
    project_mirror_edit(
        repo.path(),
        Path::new("neutral_x1/new_file.mm"),
        Default::default(),
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(repo.path().join("dangerous/new_file.mm")).unwrap(),
        "int created = 2;\n"
    );

    apply_patch_text(
        repo.path(),
        "--- a/neutral_x1/new_file.mm\n\
         +++ /dev/null\n\
         @@ -1,1 +0,0 @@\n\
         -int created = 2;\n",
    )
    .unwrap();
    assert!(!repo.path().join("dangerous/new_file.mm").exists());
    assert!(!mirror.exists());
    verify_workspace(repo.path()).unwrap();
}

#[test]
fn cli_and_mcp_semantic_reads_expose_only_projected_path() {
    let repo = configured_repo();
    index_workspace(repo.path()).unwrap();

    let output = assert_cmd::Command::cargo_bin("code-sanity")
        .unwrap()
        .args([
            "--root",
            repo.path().to_str().unwrap(),
            "--json",
            "read-code",
            "src/neutral_x1_worker.mm",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["data"]["rel_path"], "src/neutral_x1_worker.mm");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("dangerous_worker"));

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "read_code",
            "arguments": { "path": "src/neutral_x1_worker.mm" }
        }
    });
    let mut response = Vec::new();
    code_sanity::mcp::serve(
        repo.path(),
        Cursor::new(format!("{request}\n").into_bytes()),
        &mut response,
    )
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&response).unwrap();
    assert_eq!(
        value["result"]["structuredContent"]["rel_path"],
        "src/neutral_x1_worker.mm"
    );
    assert!(!String::from_utf8_lossy(&response).contains("dangerous_worker"));
}

#[test]
fn search_list_and_provider_payload_use_projected_path() {
    let repo = configured_repo();
    let layout = Layout::new(repo.path());
    index_workspace(repo.path()).unwrap();

    let hits = search_mirror(repo.path(), "int value", None).unwrap();
    assert_eq!(hits[0].rel_path, "src/neutral_x1_worker.mm");
    let files = code_sanity::search::list_mirror_files(repo.path(), None).unwrap();
    assert!(files.iter().any(|path| path == "src/neutral_x1_worker.mm"));
    assert!(!files.iter().any(|path| path.contains("dangerous_worker")));

    let provider = repo.path().join("provider.sh");
    fs::write(
        &provider,
        "#!/bin/sh\n\
         payload=$(sed -n '1p')\n\
         case \"$payload\" in *src/neutral_x1_worker.mm*) ;; *) exit 9 ;; esac\n\
         case \"$payload\" in *dangerous_worker*) exit 10 ;; esac\n\
         printf '%s' '{\"proposals\":[]}'\n",
    )
    .unwrap();
    let mut config = Config::load_or_default(&layout).unwrap();
    config.sanitizer.provider = code_sanity::config::ProviderConfig::External {
        command: vec!["sh".into(), provider.display().to_string()],
        timeout_secs: Some(10),
    };
    config.save(&layout).unwrap();
    let report = code_sanity::proposal::propose_sanitize(
        repo.path(),
        Some(Path::new("src")),
        code_sanity::proposal::ProviderAllow {
            command: true,
            endpoint: false,
        },
    )
    .unwrap();
    assert_eq!(report.errors, Vec::<String>::new());
}

#[test]
fn policy_change_hides_stale_path_and_preserves_pending_edit_until_force() {
    let repo = configured_repo();
    let layout = Layout::new(repo.path());
    index_workspace(repo.path()).unwrap();

    let old_mirror = layout.mirror_dir.join("src/neutral_x1_worker.mm");
    fs::write(&old_mirror, "int pending = 9;\n").unwrap();
    let mut config = Config::load_or_default(&layout).unwrap();
    config
        .sanitizer
        .dictionary
        .insert("dangerous".into(), "calm_y2".into());
    config.save(&layout).unwrap();

    let ordinary = index_workspace(repo.path()).unwrap();
    assert_eq!(ordinary.pending, 1);
    assert!(old_mirror.is_file());
    assert!(!layout.mirror_dir.join("src/calm_y2_worker.mm").exists());
    let listed = code_sanity::search::list_mirror_files(repo.path(), None).unwrap();
    assert!(!listed.iter().any(|path| path.contains("dangerous_worker")));
    assert!(!listed.iter().any(|path| path.contains("neutral_x1_worker")));
    assert!(read_sanitized_file(repo.path(), Path::new("src/neutral_x1_worker.mm")).is_err());

    let forced = code_sanity::index::index_workspace_force(repo.path()).unwrap();
    assert_eq!(forced.stashed.len(), 1);
    assert_eq!(
        fs::read_to_string(&forced.stashed[0]).unwrap(),
        "int pending = 9;\n"
    );
    assert!(!old_mirror.exists());
    assert_eq!(
        read_sanitized_file(repo.path(), Path::new("src/calm_y2_worker.mm")).unwrap(),
        "int value = 1;\n"
    );
    verify_workspace(repo.path()).unwrap();
}

#[test]
fn review_and_strict_worktree_expose_projected_path_only() {
    let repo = configured_repo();
    let layout = Layout::new(repo.path());
    index_workspace(repo.path()).unwrap();

    fs::create_dir_all(&layout.review_dir).unwrap();
    fs::write(
        layout.review_dir.join("manual.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "id": "manual",
            "file": "src/dangerous_worker.mm",
            "proposal": {
                "category": "identifier",
                "original_text": "value",
                "sanitized_text": "item",
                "confidence": 0.9
            },
            "status": "pending",
            "flag": "clean",
            "created_at": "2026-07-12T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
    let reviews = code_sanity::proposal::list_review(repo.path(), false).unwrap();
    assert_eq!(reviews[0].file, "src/neutral_x1_worker.mm");

    let output = assert_cmd::Command::cargo_bin("code-sanity")
        .unwrap()
        .args([
            "--root",
            repo.path().to_str().unwrap(),
            "strict-run",
            "--",
            "sh",
            "-c",
            "find . -type f -print | sort",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("./src/neutral_x1_worker.mm"), "{stdout}");
    assert!(!stdout.contains("dangerous_worker"), "{stdout}");
}

#[test]
fn provider_can_propose_and_approve_a_path_only_alias() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("src/weaponized_loader.mm"),
        "int weaponized = 1;\n",
    )
    .unwrap();
    init_workspace(repo.path()).unwrap();
    let layout = Layout::new(repo.path());
    let mut config = Config::load_or_default(&layout).unwrap();
    config.sanitizer.dictionary.clear();
    config.sanitizer.alias_registry.clear();
    config.sanitizer.path_alias_registry.clear();
    config.sanitizer.denylist.clear();
    config.save(&layout).unwrap();
    index_workspace(repo.path()).unwrap();

    let provider = repo.path().join("provider.sh");
    fs::write(
        &provider,
        "#!/bin/sh\n\
         payload=$(sed -n '1p')\n\
         case \"$payload\" in *'\"request_mode\":\"path-only\"'*) ;; *) printf '%s' '{\"proposals\":[]}' ; exit 0 ;; esac\n\
         case \"$payload\" in *'\"value\":\"weaponized_loader\"'*) ;; *) exit 11 ;; esac\n\
         path_id=$(printf '%s' \"$payload\" | sed 's/\"path_id\":\"/\\\n/g' | sed -n 's/^\\([^\"]*\\)\".*/\\1/p' | tail -n 1)\n\
         test -n \"$path_id\" || exit 11\n\
         printf '{\"proposals\":[{\"target\":{\"path_id\":\"%s\"},\"category\":\"file_path\",\"original_text\":\"weaponized\",\"sanitized_text\":\"neutral\",\"confidence\":0.97,\"rationale\":\"risk-loaded filename term\"}]}' \"$path_id\"\n",
    )
    .unwrap();
    config.sanitizer.provider = code_sanity::config::ProviderConfig::External {
        command: vec!["sh".into(), provider.display().to_string()],
        timeout_secs: Some(10),
    };
    config.save(&layout).unwrap();

    let report = code_sanity::proposal::propose_sanitize(
        repo.path(),
        Some(Path::new("src/weaponized_loader.mm")),
        code_sanity::proposal::ProviderAllow {
            command: true,
            endpoint: false,
        },
    )
    .unwrap();
    assert_eq!(report.queued, 1, "{report:?}");
    assert!(report.rejected.is_empty(), "{:?}", report.rejected);

    let pending = code_sanity::proposal::list_review(repo.path(), false).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].proposal.category, "file_path");
    assert!(matches!(
        pending[0].proposal.target.as_ref(),
        Some(code_sanity::proposal::ProposalTarget::FilePath(_))
    ));
    assert_eq!(
        code_sanity::proposal::preview_file_path_change(&pending[0]).unwrap(),
        (
            "src/weaponized_loader.mm".to_string(),
            "src/neutral_loader.mm".to_string()
        )
    );

    let approved =
        code_sanity::proposal::resolve_review(repo.path(), &pending[0].id, true).unwrap();
    assert_eq!(approved.file, "src/neutral_loader.mm");
    let config = Config::load_or_default(&layout).unwrap();
    assert_eq!(
        config
            .sanitizer
            .path_alias_registry
            .get("weaponized")
            .map(String::as_str),
        Some("neutral")
    );
    assert!(repo.path().join("src/weaponized_loader.mm").is_file());
    assert!(!repo.path().join("src/neutral_loader.mm").exists());
    assert!(layout.mirror_dir.join("src/neutral_loader.mm").is_file());
    assert!(!layout.mirror_dir.join("src/weaponized_loader.mm").exists());
    assert_eq!(
        read_sanitized_file(repo.path(), Path::new("src/neutral_loader.mm")).unwrap(),
        "int weaponized = 1;\n",
        "path-only approval must not rewrite source content"
    );
    verify_workspace(repo.path()).unwrap();
}

#[test]
fn first_proposal_scan_indexes_before_resolving_its_directory_scope() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src/nested")).unwrap();
    fs::write(
        repo.path().join("src/nested/main.rs"),
        "fn ordinary_name() {}\n",
    )
    .unwrap();
    init_workspace(repo.path()).unwrap();
    let layout = Layout::new(repo.path());
    let provider = repo.path().join("provider.sh");
    fs::write(
        &provider,
        "#!/bin/sh\n\
         payload=$(sed -n '1p')\n\
         case \"$payload\" in *'\"request_mode\":\"path-only\"'*) printf '%s' '{\"proposals\":[]}' ; exit 0 ;; esac\n\
         case \"$payload\" in *'\"rel\":\"src/nested/main.rs\"'*) ;; *) exit 12 ;; esac\n\
         printf '%s' '{\"proposals\":[]}'\n",
    )
    .unwrap();
    let mut config = Config::load_or_default(&layout).unwrap();
    config.sanitizer.provider = code_sanity::config::ProviderConfig::External {
        command: vec!["sh".into(), provider.display().to_string()],
        timeout_secs: Some(10),
    };
    config.save(&layout).unwrap();
    let conn = code_sanity::db::connect(&layout).unwrap();
    assert!(code_sanity::db::tracked_files(&conn).unwrap().is_empty());
    drop(conn);

    let report = code_sanity::proposal::propose_sanitize(
        repo.path(),
        Some(Path::new("src/nested")),
        code_sanity::proposal::ProviderAllow {
            command: true,
            endpoint: false,
        },
    )
    .unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    let conn = code_sanity::db::connect(&layout).unwrap();
    assert!(
        code_sanity::db::tracked_files(&conn)
            .unwrap()
            .iter()
            .any(|file| file == "src/nested/main.rs")
    );
}
