use code_sanity::config::Layout;
use code_sanity::semantic_store;
use code_sanity::transaction::{self, EditIntent};
use std::fs;

fn indexed_rust_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("src/lib.rs"),
        "fn value() -> u32 {\n    1\n}\n",
    )
    .unwrap();
    code_sanity::index_workspace(repo.path()).unwrap();
    repo
}

fn node_id(repo: &tempfile::TempDir, kind: &str) -> String {
    let layout = Layout::new(repo.path());
    let conn = code_sanity::db::connect(&layout).unwrap();
    conn.query_row(
        "select node_id from semantic_nodes where rel_path = 'src/lib.rs' and kind = ?1 limit 1",
        [kind],
        |row| row.get(0),
    )
    .unwrap()
}

#[test]
fn edit_node_preview_commit_is_revision_checked_and_reindexed() {
    let repo = indexed_rust_repo();
    let layout = Layout::new(repo.path());
    let conn = code_sanity::db::connect(&layout).unwrap();
    let revision = semantic_store::current_revision(&conn).unwrap();
    drop(conn);

    let preview = transaction::preview_transaction(
        repo.path(),
        revision,
        vec![EditIntent::EditNode {
            node_id: node_id(&repo, "integer_literal"),
            replacement: "2".to_string(),
        }],
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(repo.path().join("src/lib.rs")).unwrap(),
        "fn value() -> u32 {\n    1\n}\n",
        "preview must not write real sources"
    );

    let report = transaction::commit_transaction(
        repo.path(),
        &preview.transaction_id,
        revision,
        Some("test".into()),
        Some("semantic-v2".into()),
    )
    .unwrap();
    assert!(report.committed_revision > revision);
    assert_eq!(
        fs::read_to_string(repo.path().join("src/lib.rs")).unwrap(),
        "fn value() -> u32 {\n    2\n}\n"
    );
    let repeated = transaction::commit_transaction(
        repo.path(),
        &preview.transaction_id,
        revision,
        Some("test".into()),
        Some("semantic-v2".into()),
    )
    .unwrap();
    assert_eq!(repeated.journal, "already-committed");
    code_sanity::verify_workspace(repo.path()).unwrap();
}

#[test]
fn preview_rejects_stale_revision_and_declaration_edit() {
    let repo = indexed_rust_repo();
    let layout = Layout::new(repo.path());
    let conn = code_sanity::db::connect(&layout).unwrap();
    let revision = semantic_store::current_revision(&conn).unwrap();
    drop(conn);

    let stale = transaction::preview_transaction(
        repo.path(),
        revision + 1,
        vec![EditIntent::EditNode {
            node_id: node_id(&repo, "integer_literal"),
            replacement: "2".to_string(),
        }],
    )
    .unwrap_err();
    assert!(stale.to_string().contains("stale semantic revision"));

    let declaration = transaction::preview_transaction(
        repo.path(),
        revision,
        vec![EditIntent::EditNode {
            node_id: node_id(&repo, "function_item"),
            replacement: "fn renamed() -> u32 { 1 }".to_string(),
        }],
    )
    .unwrap_err();
    assert!(declaration.to_string().contains("rename_symbol"));
}

#[test]
fn targeted_proposal_projects_only_bound_symbol_occurrences() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    let original = "// shadowfax stays in prose\nfn shadowfax() { shadowfax(); }\n";
    fs::write(repo.path().join("src/lib.rs"), original).unwrap();
    code_sanity::index_workspace(repo.path()).unwrap();
    let layout = Layout::new(repo.path());
    let conn = code_sanity::db::connect(&layout).unwrap();
    let (symbol_id, occurrence_id): (String, String) = conn
        .query_row(
            r#"
            select s.symbol_id, o.occurrence_id
            from semantic_symbols s
            join semantic_occurrences o on o.symbol_id = s.symbol_id
            where s.name = 'shadowfax' and o.role = 'declaration'
            "#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    drop(conn);

    let item = code_sanity::proposal::ReviewItem {
        id: "2099-01-01T00-00-00.000000000Z-semantic".to_string(),
        file: "src/lib.rs".to_string(),
        proposal: code_sanity::proposal::Proposal {
            target: Some(code_sanity::proposal::ProposalTarget {
                symbol_id,
                occurrence_id,
            }),
            category: "identifier".to_string(),
            original_text: "shadowfax".to_string(),
            sanitized_text: "neutral_helper".to_string(),
            confidence: 0.95,
            rationale: Some("repository-owned test symbol".to_string()),
        },
        status: code_sanity::proposal::ReviewStatus::Pending,
        flag: "clean".to_string(),
        created_at: "2099-01-01T00:00:00Z".to_string(),
    };
    fs::create_dir_all(&layout.review_dir).unwrap();
    fs::write(
        layout.review_dir.join(format!("{}.json", item.id)),
        serde_json::to_string_pretty(&item).unwrap(),
    )
    .unwrap();
    code_sanity::proposal::resolve_review(repo.path(), &item.id, true).unwrap();

    let conn = code_sanity::db::connect(&layout).unwrap();
    let projected = semantic_store::project_document(&conn, repo.path(), "src/lib.rs").unwrap();
    assert!(projected.content.starts_with("// shadowfax stays in prose"));
    assert!(projected.content.contains("fn neutral_helper()"));
    assert!(projected.content.contains("neutral_helper();"));
    assert_eq!(
        fs::read_to_string(repo.path().join("src/lib.rs")).unwrap(),
        original
    );
}

#[test]
fn rust_analyzer_regression_combines_semantic_renames_with_ast_edits() {
    if !std::process::Command::new("rust-analyzer")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        eprintln!("rust-analyzer unavailable; skipping live LSP regression");
        return;
    }

    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("Cargo.toml"),
        "[package]\nname = \"semantic-regression\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("src/lib.rs"),
        "fn get_hwid() -> u64 { 1 }\n\npub fn run(some_argument: u64) {\n    let hwid = get_hwid();\n    assert_eq!(hwid, 1);\n}\n",
    )
    .unwrap();
    code_sanity::index_workspace(repo.path()).unwrap();
    let layout = Layout::new(repo.path());
    let conn = code_sanity::db::connect(&layout).unwrap();
    let symbol = |name: &str| -> String {
        conn.query_row(
            "select symbol_id from semantic_symbols where rel_path = 'src/lib.rs' and name = ?1",
            [name],
            |row| row.get(0),
        )
        .unwrap()
    };
    let function_range: (i64, i64) = conn
        .query_row(
            r#"
            select n.start_byte, n.end_byte from semantic_symbols s
            join semantic_nodes n on n.node_id = s.scope_node_id
            where s.rel_path = 'src/lib.rs' and s.name = 'get_hwid'
            "#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let contained_node = |kind: &str| -> String {
        conn.query_row(
            r#"
            select node_id from semantic_nodes
            where rel_path = 'src/lib.rs' and kind = ?1
              and start_byte >= ?2 and end_byte <= ?3
            order by start_byte limit 1
            "#,
            rusqlite::params![kind, function_range.0, function_range.1],
            |row| row.get(0),
        )
        .unwrap()
    };
    let arguments = conn
        .query_row(
            "select node_id from semantic_nodes where rel_path = 'src/lib.rs' and kind = 'arguments' limit 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap();
    let get_hwid = symbol("get_hwid");
    let hwid = symbol("hwid");
    let parameters = contained_node("parameters");
    let literal = contained_node("integer_literal");
    drop(conn);

    let mut conn = code_sanity::db::connect(&layout).unwrap();
    semantic_store::accept_symbol_alias(
        &mut conn,
        &get_hwid,
        "get_device_id",
        "identifier",
        1.0,
        Some("regression setup"),
    )
    .unwrap();
    semantic_store::accept_symbol_alias(
        &mut conn,
        &hwid,
        "device_id",
        "identifier",
        1.0,
        Some("regression setup"),
    )
    .unwrap();
    let revision = semantic_store::current_revision(&conn).unwrap();
    let projected = semantic_store::project_document(&conn, repo.path(), "src/lib.rs").unwrap();
    let get_hwid_symbol = semantic_store::load_symbol(&conn, &get_hwid)
        .unwrap()
        .unwrap();
    assert!(projected.content.contains("fn get_device_id()"));
    assert!(
        projected
            .content
            .contains("let device_id = get_device_id();")
    );
    drop(conn);

    let references = code_sanity::lsp::references(
        repo.path(),
        std::path::Path::new("src/lib.rs"),
        &fs::read_to_string(repo.path().join("src/lib.rs")).unwrap(),
        code_sanity::semantic::LanguageId::Rust,
        &get_hwid_symbol.range,
    )
    .unwrap();
    assert_eq!(references.len(), 2);

    let preview = transaction::preview_transaction(
        repo.path(),
        revision,
        vec![
            EditIntent::RenameSymbol {
                symbol_id: get_hwid,
                new_name: "get_device_id_klitor".to_string(),
            },
            EditIntent::RenameSymbol {
                symbol_id: hwid,
                new_name: "device_id_pizda".to_string(),
            },
            EditIntent::EditNode {
                node_id: parameters,
                replacement: "(some_argument: u64)".to_string(),
            },
            EditIntent::EditNode {
                node_id: literal,
                replacement: "some_argument".to_string(),
            },
            EditIntent::EditNode {
                node_id: arguments,
                replacement: "(some_argument)".to_string(),
            },
        ],
    )
    .unwrap();
    transaction::commit_transaction(
        repo.path(),
        &preview.transaction_id,
        revision,
        Some("regression-test".to_string()),
        None,
    )
    .unwrap();

    let updated = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    assert!(updated.contains("fn get_device_id_klitor(some_argument: u64)"));
    assert!(updated.contains("let device_id_pizda = get_device_id_klitor(some_argument);"));
    code_sanity::verify_workspace(repo.path()).unwrap();
    let status = std::process::Command::new("cargo")
        .arg("check")
        .current_dir(repo.path())
        .status()
        .unwrap();
    assert!(status.success(), "semantic regression must compile");
}

#[test]
fn clangd_renames_objective_cpp_symbol_and_all_references() {
    if !std::process::Command::new("clangd")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        eprintln!("clangd unavailable; skipping live LSP regression");
        return;
    }
    let repo = tempfile::tempdir().unwrap();
    fs::write(
        repo.path().join("compile_flags.txt"),
        "-x\nobjective-c++\n-std=c++20\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("main.mm"),
        "static int get_hwid() { return 1; }\nint run() { return get_hwid(); }\n",
    )
    .unwrap();
    code_sanity::index_workspace(repo.path()).unwrap();
    let layout = Layout::new(repo.path());
    let conn = code_sanity::db::connect(&layout).unwrap();
    let revision = semantic_store::current_revision(&conn).unwrap();
    let symbol_id = conn
        .query_row(
            "select symbol_id from semantic_symbols where rel_path = 'main.mm' and name = 'get_hwid'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap();
    drop(conn);
    let preview = transaction::preview_transaction(
        repo.path(),
        revision,
        vec![EditIntent::RenameSymbol {
            symbol_id,
            new_name: "get_device_id".to_string(),
        }],
    )
    .unwrap();
    transaction::commit_transaction(
        repo.path(),
        &preview.transaction_id,
        revision,
        Some("clangd-regression".to_string()),
        None,
    )
    .unwrap();
    let source = fs::read_to_string(repo.path().join("main.mm")).unwrap();
    assert!(source.contains("int get_device_id()"));
    assert!(source.contains("return get_device_id();"));
    let status = std::process::Command::new("clang++")
        .args([
            "-x",
            "objective-c++",
            "-std=c++20",
            "-fsyntax-only",
            "main.mm",
        ])
        .current_dir(repo.path())
        .status()
        .unwrap();
    assert!(status.success(), "Objective-C++ rename must compile");
}

#[test]
fn semantic_cli_snapshot_find_and_read_smoke() {
    let repo = indexed_rust_repo();
    let binary = assert_cmd::cargo::cargo_bin("code-sanity");
    for (command, extra) in [
        ("workspace-snapshot", Vec::<&str>::new()),
        ("find-code", vec!["value"]),
        ("read-code", vec!["src/lib.rs"]),
    ] {
        let output = std::process::Command::new(&binary)
            .arg("--root")
            .arg(repo.path())
            .arg("--json")
            .arg(command)
            .args(extra)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{command} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["command"], command);
    }
}
