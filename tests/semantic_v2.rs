use code_sanity::config::Layout;
use code_sanity::lsp::LspLocation;
use code_sanity::semantic::TextRange;
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

fn lsp_location(rel_path: &str, source: &str, needle: &str) -> LspLocation {
    let start_byte = source.find(needle).unwrap();
    let end_byte = start_byte + needle.len();
    let before = &source[..start_byte];
    let start_line = before.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    let start_column = start_byte - line_start + 1;
    LspLocation {
        rel_path: rel_path.to_string(),
        range: TextRange {
            start_byte,
            end_byte,
            start_line,
            start_column,
            end_line: start_line,
            end_column: start_column + needle.len(),
        },
    }
}

#[test]
fn compiler_bindings_link_header_definition_and_call_projection() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("include")).unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    let header = "int helper(int value);\n";
    let definition = "int helper(int value) { return value; }\n";
    let use_site = "int run() { return helper(1); }\n";
    fs::write(repo.path().join("include/api.hpp"), header).unwrap();
    fs::write(repo.path().join("src/api.cpp"), definition).unwrap();
    fs::write(repo.path().join("src/use.cpp"), use_site).unwrap();
    code_sanity::index_workspace(repo.path()).unwrap();
    let layout = Layout::new(repo.path());
    let mut conn = code_sanity::db::connect(&layout).unwrap();
    let canonical: String = conn
        .query_row(
            "select symbol_id from semantic_symbols where rel_path = 'include/api.hpp' and name = 'helper'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let locations = vec![
        lsp_location("include/api.hpp", header, "helper"),
        lsp_location("src/api.cpp", definition, "helper"),
        lsp_location("src/use.cpp", use_site, "helper"),
    ];
    semantic_store::admit_compiler_references(
        &mut conn,
        repo.path(),
        &canonical,
        "clangd-test",
        &locations,
    )
    .unwrap();
    assert!(semantic_store::symbol_projection_is_complete(&conn, &canonical).unwrap());
    semantic_store::accept_symbol_alias(
        &mut conn,
        &canonical,
        "neutral_helper",
        "identifier",
        1.0,
        Some("compiler-linked test"),
    )
    .unwrap();
    for (path, expected) in [
        ("include/api.hpp", "int neutral_helper(int value);"),
        ("src/api.cpp", "int neutral_helper(int value)"),
        ("src/use.cpp", "return neutral_helper(1)"),
    ] {
        let projected = semantic_store::project_document(&conn, repo.path(), path).unwrap();
        assert!(
            projected.content.contains(expected),
            "{path}: {}",
            projected.content
        );
    }
    drop(conn);

    fs::write(
        repo.path().join("src/use.cpp"),
        "\nint run() { return helper(1); }\n",
    )
    .unwrap();
    code_sanity::index_workspace(repo.path()).unwrap();
    let conn = code_sanity::db::connect(&layout).unwrap();
    assert!(
        semantic_store::symbol_projection_is_complete(&conn, &canonical).unwrap(),
        "content drift must trigger and complete a fresh compiler reference closure"
    );
    let aliases: i64 = conn
        .query_row(
            "select count(*) from semantic_aliases where sanitized_name = 'neutral_helper'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(aliases >= 1, "approved alias decision must survive refresh");
}

#[test]
fn resolver_version_upgrade_reindexes_unchanged_source_without_changing_symbol_ids() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("src/main.cpp"),
        "int helper(int value) { return value; }\n",
    )
    .unwrap();
    code_sanity::index_workspace(repo.path()).unwrap();
    let layout = Layout::new(repo.path());
    let conn = code_sanity::db::connect(&layout).unwrap();
    let generated_id: String = conn
        .query_row(
            "select symbol_id from semantic_symbols where name = 'helper'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let legacy_id = "sym_legacy_stable_review_target";
    conn.execute(
        r#"
        insert into semantic_symbols(
          symbol_id, rel_path, node_id, name, kind, qualified_name,
          scope_node_id, origin, locally_bound
        )
        select ?2, rel_path, node_id, name, kind, qualified_name,
               scope_node_id, origin, locally_bound
        from semantic_symbols where symbol_id = ?1
        "#,
        rusqlite::params![generated_id, legacy_id],
    )
    .unwrap();
    conn.execute(
        "update semantic_occurrences set symbol_id = ?2 where symbol_id = ?1",
        rusqlite::params![generated_id, legacy_id],
    )
    .unwrap();
    conn.execute(
        "delete from semantic_symbols where symbol_id = ?1",
        [&generated_id],
    )
    .unwrap();
    let capabilities: String = conn
        .query_row(
            "select capabilities_json from semantic_documents where rel_path = 'src/main.cpp'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let mut capabilities: serde_json::Value = serde_json::from_str(&capabilities).unwrap();
    capabilities["resolver_version"] = serde_json::json!(0);
    conn.execute(
        "update semantic_documents set capabilities_json = ?1 where rel_path = 'src/main.cpp'",
        [serde_json::to_string(&capabilities).unwrap()],
    )
    .unwrap();
    drop(conn);

    let report = code_sanity::index_workspace(repo.path()).unwrap();
    assert_eq!(report.indexed, 0, "lexical source is unchanged");
    assert_eq!(report.semantic.indexed, 1, "semantic resolver must upgrade");
    let conn = code_sanity::db::connect(&layout).unwrap();
    let after: String = conn
        .query_row(
            "select symbol_id from semantic_symbols where name = 'helper'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(after, legacy_id);
    let stored = semantic_store::load_document(&conn, "src/main.cpp")
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.capabilities.resolver_version,
        code_sanity::semantic::SEMANTIC_RESOLVER_VERSION
    );
}

#[test]
fn resolver_upgrade_preserves_identity_but_replaces_stale_binding_decisions() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    let source = r#"class Database { public: void delete_agent(int token); };
class ServerState { public: void delete_agent(int token) {} };
ServerState g_state;
void route(Database& db) {
    g_state.delete_agent(1);
    db.delete_agent(1);
}
"#;
    fs::write(repo.path().join("src/server.cpp"), source).unwrap();
    code_sanity::index_workspace(repo.path()).unwrap();
    let layout = Layout::new(repo.path());
    let conn = code_sanity::db::connect(&layout).unwrap();
    let target: String = conn
        .query_row(
            "select symbol_id from semantic_symbols where qualified_name = 'ServerState::delete_agent'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let database_call = source.find("db.delete_agent").unwrap() + "db.".len();
    conn.execute(
        "update semantic_occurrences set symbol_id = ?1, role = 'reference' where rel_path = 'src/server.cpp' and start_byte = ?2",
        rusqlite::params![target, database_call as i64],
    )
    .unwrap();
    conn.execute(
        "update semantic_symbols set qualified_name = 'delete_agent' where symbol_id = ?1",
        [&target],
    )
    .unwrap();
    let capabilities: String = conn
        .query_row(
            "select capabilities_json from semantic_documents where rel_path = 'src/server.cpp'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let mut capabilities: serde_json::Value = serde_json::from_str(&capabilities).unwrap();
    capabilities["resolver_version"] = serde_json::json!(0);
    conn.execute(
        "update semantic_documents set capabilities_json = ?1 where rel_path = 'src/server.cpp'",
        [serde_json::to_string(&capabilities).unwrap()],
    )
    .unwrap();
    drop(conn);

    let report = code_sanity::index_workspace(repo.path()).unwrap();
    assert_eq!(report.indexed, 0);
    assert_eq!(report.semantic.indexed, 1);
    let conn = code_sanity::db::connect(&layout).unwrap();
    let (after_id, qualified): (String, String) = conn
        .query_row(
            "select symbol_id, qualified_name from semantic_symbols where rel_path = 'src/server.cpp' and name = 'delete_agent' and qualified_name like 'ServerState::%'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        after_id, target,
        "queued review identity must remain stable"
    );
    assert_eq!(qualified, "ServerState::delete_agent");
    let (role, owner): (String, Option<String>) = conn
        .query_row(
            "select role, symbol_id from semantic_occurrences where rel_path = 'src/server.cpp' and start_byte = ?1",
            [database_call as i64],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(role, "reference");
    assert_ne!(owner.as_deref(), Some(target.as_str()));
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
    let original =
        "// shadowfax stays in prose\nfn run() { let shadowfax = 1; let _ = shadowfax; }\n";
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

    let target_symbol_id = symbol_id.clone();
    let item = code_sanity::proposal::ReviewItem {
        id: "2099-01-01T00-00-00.000000000Z-semantic".to_string(),
        file: "src/lib.rs".to_string(),
        proposal: code_sanity::proposal::Proposal {
            target: Some(code_sanity::proposal::ProposalTarget::Semantic(
                code_sanity::proposal::SemanticProposalTarget {
                    symbol_id,
                    occurrence_id,
                },
            )),
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
    assert!(projected.content.contains("let neutral_helper = 1"));
    assert!(projected.content.contains("let _ = neutral_helper"));
    assert_eq!(
        fs::read_to_string(repo.path().join("src/lib.rs")).unwrap(),
        original
    );
    let mirror =
        code_sanity::read_sanitized_file(repo.path(), std::path::Path::new("src/lib.rs")).unwrap();
    assert_eq!(mirror, projected.content);
    let projected_symbol = projected
        .symbols
        .iter()
        .find(|symbol| symbol.symbol_id == target_symbol_id)
        .unwrap();
    assert_eq!(projected_symbol.name, "neutral_helper");
    assert_eq!(
        &projected.content[projected_symbol.range.start_byte..projected_symbol.range.end_byte],
        "neutral_helper"
    );
    assert!(
        projected
            .occurrences
            .iter()
            .filter(|occurrence| occurrence.symbol_id.as_deref() == Some(&target_symbol_id))
            .all(|occurrence| occurrence.name == "neutral_helper")
    );
    let found = semantic_store::find_symbols(&conn, repo.path(), "neutral_helper", 10).unwrap();
    assert!(
        found
            .iter()
            .any(|(_, symbol)| symbol.symbol_id == target_symbol_id)
    );
    assert!(
        semantic_store::find_symbols(&conn, repo.path(), "shadowfax", 10)
            .unwrap()
            .is_empty(),
        "real spelling must not remain searchable after projection"
    );
}

#[test]
fn edit_node_back_projects_semantic_alias_references() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("src/lib.rs"),
        "fn run() -> u32 { let shadowfax = 7; let output = 0; output }\n",
    )
    .unwrap();
    code_sanity::index_workspace(repo.path()).unwrap();
    let layout = Layout::new(repo.path());
    let mut conn = code_sanity::db::connect(&layout).unwrap();
    let symbol_id: String = conn
        .query_row(
            "select symbol_id from semantic_symbols where name = 'shadowfax'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    semantic_store::accept_symbol_alias(
        &mut conn,
        &symbol_id,
        "neutral_helper",
        "identifier",
        1.0,
        Some("structured back-projection regression"),
    )
    .unwrap();
    drop(conn);
    code_sanity::index_workspace(repo.path()).unwrap();

    let conn = code_sanity::db::connect(&layout).unwrap();
    let revision = semantic_store::current_revision(&conn).unwrap();
    let literal: String = conn
        .query_row(
            "select node_id from semantic_nodes where kind = 'integer_literal' and start_byte = (select max(start_byte) from semantic_nodes where kind = 'integer_literal')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(conn);
    let preview = transaction::preview_transaction(
        repo.path(),
        revision,
        vec![EditIntent::EditNode {
            node_id: literal,
            replacement: "neutral_helper".to_string(),
        }],
    )
    .unwrap();
    assert_eq!(preview.files[0].edits[0].replacement, "neutral_helper");
    transaction::commit_transaction(
        repo.path(),
        &preview.transaction_id,
        revision,
        Some("test".into()),
        Some("alias-back-projection".into()),
    )
    .unwrap();
    let real = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    assert!(real.contains("let output = shadowfax;"), "{real}");
    assert!(!real.contains("let output = neutral_helper;"));
    let mirror =
        code_sanity::read_sanitized_file(repo.path(), std::path::Path::new("src/lib.rs")).unwrap();
    assert!(mirror.contains("let output = neutral_helper;"), "{mirror}");
    code_sanity::verify_workspace(repo.path()).unwrap();
}

#[test]
fn semantic_aliases_are_workspace_injective() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("src/lib.rs"),
        "fn one() { let first_private = 1; let _ = first_private; }\n\
         fn two() { let second_private = 2; let _ = second_private; }\n",
    )
    .unwrap();
    code_sanity::index_workspace(repo.path()).unwrap();
    let layout = Layout::new(repo.path());
    let mut conn = code_sanity::db::connect(&layout).unwrap();
    let symbol = |name: &str| {
        conn.query_row(
            "select symbol_id from semantic_symbols where name = ?1",
            [name],
            |row| row.get::<_, String>(0),
        )
        .unwrap()
    };
    let first = symbol("first_private");
    let second = symbol("second_private");
    semantic_store::accept_symbol_alias(
        &mut conn,
        &first,
        "neutral_value",
        "identifier",
        1.0,
        None,
    )
    .unwrap();
    let error = semantic_store::accept_symbol_alias(
        &mut conn,
        &second,
        "neutral_value",
        "identifier",
        1.0,
        None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("workspace-injective"));
}

#[test]
fn semantic_alias_mapping_can_repeat_for_the_same_original_spelling() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("src/lib.rs"),
        "fn one() { let shared_private = 1; let _ = shared_private; }\n\
         fn two() { let shared_private = 2; let _ = shared_private; }\n",
    )
    .unwrap();
    code_sanity::index_workspace(repo.path()).unwrap();
    let layout = Layout::new(repo.path());
    let mut conn = code_sanity::db::connect(&layout).unwrap();
    let symbols = conn
        .prepare("select symbol_id from semantic_symbols where name = 'shared_private'")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(symbols.len(), 2);
    for symbol in &symbols {
        semantic_store::accept_symbol_alias(
            &mut conn,
            symbol,
            "shared_value",
            "identifier",
            1.0,
            None,
        )
        .unwrap();
    }
    let accepted: i64 = conn
        .query_row(
            "select count(*) from semantic_aliases where sanitized_name = 'shared_value' and status = 'accepted'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(accepted, 2);
    let projected = semantic_store::project_document(&conn, repo.path(), "src/lib.rs").unwrap();
    assert_eq!(projected.content.matches("shared_value").count(), 4);
}

#[test]
fn semantic_alias_cannot_reuse_a_lexical_alias() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("src/lib.rs"),
        "// legacy_private\nfn one() { let first_private = 1; let _ = first_private; }\n",
    )
    .unwrap();
    code_sanity::index_workspace(repo.path()).unwrap();
    let layout = Layout::new(repo.path());
    let mut config = code_sanity::config::Config::load_or_default(&layout).unwrap();
    config
        .sanitizer
        .alias_registry
        .insert("legacy_private".to_string(), "neutral_value".to_string());
    config.save(&layout).unwrap();
    code_sanity::index_workspace(repo.path()).unwrap();

    let mut conn = code_sanity::db::connect(&layout).unwrap();
    let symbol: String = conn
        .query_row(
            "select symbol_id from semantic_symbols where name = 'first_private'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let error = semantic_store::accept_symbol_alias(
        &mut conn,
        &symbol,
        "neutral_value",
        "identifier",
        1.0,
        None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("lexical alias"), "{error:#}");
}

#[test]
fn find_symbols_uses_lexical_projection_without_exposing_the_raw_name() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("src/lib.rs"),
        "fn shadowfax() -> u32 { 1 }\n",
    )
    .unwrap();
    code_sanity::index_workspace(repo.path()).unwrap();
    let layout = Layout::new(repo.path());
    let mut config = code_sanity::config::Config::load_or_default(&layout).unwrap();
    config
        .sanitizer
        .alias_registry
        .insert("shadowfax".to_string(), "neutral_helper".to_string());
    config.save(&layout).unwrap();
    code_sanity::index_workspace(repo.path()).unwrap();

    let conn = code_sanity::db::connect(&layout).unwrap();
    let found = semantic_store::find_symbols(&conn, repo.path(), "neutral_helper", 10).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].1.name, "neutral_helper");
    assert!(
        semantic_store::find_symbols(&conn, repo.path(), "shadowfax", 10)
            .unwrap()
            .is_empty(),
        "find_code exposed a spelling hidden by the shared mirror"
    );
}

#[test]
fn new_file_references_are_back_projected_and_join_compiler_closure() {
    if !std::process::Command::new("rust-analyzer")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        eprintln!("rust-analyzer unavailable; skipping new-file semantic regression");
        return;
    }
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("Cargo.toml"),
        "[package]\nname = \"new-file-alias\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    let source = "pub(crate) fn shadowfax() -> u32 { 7 }\n";
    fs::write(repo.path().join("src/lib.rs"), source).unwrap();
    code_sanity::index_workspace(repo.path()).unwrap();
    let layout = Layout::new(repo.path());
    let mut conn = code_sanity::db::connect(&layout).unwrap();
    let symbol = semantic_store::find_symbols(&conn, repo.path(), "shadowfax", 10)
        .unwrap()
        .into_iter()
        .map(|(_, symbol)| symbol)
        .next()
        .unwrap();
    let references = code_sanity::lsp::references(
        repo.path(),
        std::path::Path::new("src/lib.rs"),
        source,
        code_sanity::semantic::LanguageId::Rust,
        &symbol.range,
        1,
    )
    .unwrap();
    semantic_store::admit_compiler_references(
        &mut conn,
        repo.path(),
        &symbol.symbol_id,
        "rust-analyzer-test",
        &references,
    )
    .unwrap();
    semantic_store::accept_symbol_alias(
        &mut conn,
        &symbol.symbol_id,
        "neutral_helper",
        "identifier",
        1.0,
        None,
    )
    .unwrap();
    drop(conn);
    code_sanity::index_workspace(repo.path()).unwrap();

    let patch = concat!(
        "--- a/src/lib.rs\n",
        "+++ b/src/lib.rs\n",
        "@@ -1,1 +1,2 @@\n",
        "+mod use_site;\n",
        " pub(crate) fn neutral_helper() -> u32 { 7 }\n",
        "--- /dev/null\n",
        "+++ b/src/use_site.rs\n",
        "@@ -0,0 +1,1 @@\n",
        "+pub(crate) fn call() -> u32 { super::neutral_helper() }\n",
    );
    code_sanity::apply_patch_text(repo.path(), patch).unwrap();
    let real = fs::read_to_string(repo.path().join("src/use_site.rs")).unwrap();
    assert!(real.contains("super::shadowfax()"), "{real}");
    assert!(!real.contains("neutral_helper"));
    let mirror =
        code_sanity::read_sanitized_file(repo.path(), std::path::Path::new("src/use_site.rs"))
            .unwrap();
    assert!(mirror.contains("super::neutral_helper()"), "{mirror}");
    let status = std::process::Command::new("cargo")
        .arg("check")
        .current_dir(repo.path())
        .status()
        .unwrap();
    assert!(status.success());
    code_sanity::verify_workspace(repo.path()).unwrap();
}

#[test]
fn unresolved_names_only_block_symbols_in_the_same_file() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(repo.path().join("src/owned.rs"), "fn shadowfax() {}\n").unwrap();
    fs::write(
        repo.path().join("src/unrelated.rs"),
        "fn run() { shadowfax(); }\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("src/ambiguous.cpp"),
        "struct A {}; struct B {}; struct Convertible {};\n\
         int parse(A value) { return 1; }\n\
         int parse(B value) { return 2; }\n\
         int run(Convertible value) { return parse(value); }\n",
    )
    .unwrap();
    code_sanity::index_workspace(repo.path()).unwrap();

    let layout = Layout::new(repo.path());
    let conn = code_sanity::db::connect(&layout).unwrap();
    let owned: String = conn
        .query_row(
            "select symbol_id from semantic_symbols where rel_path = 'src/owned.rs' and name = 'shadowfax'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        !semantic_store::symbol_projection_is_complete(&conn, &owned).unwrap(),
        "a non-local symbol requires an admitted compiler/LSP reference closure"
    );

    let overloads = conn
        .prepare(
            "select symbol_id from semantic_symbols where rel_path = 'src/ambiguous.cpp' and name = 'parse' order by symbol_id",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(overloads.len(), 2);
    assert!(
        overloads.iter().all(|symbol_id| {
            !semantic_store::symbol_projection_is_complete(&conn, symbol_id).unwrap()
        }),
        "same-file overload ambiguity must remain fail-closed"
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
    let get_hwid_symbol = semantic_store::load_symbol(&conn, &get_hwid)
        .unwrap()
        .unwrap();
    let references = code_sanity::lsp::references(
        repo.path(),
        std::path::Path::new("src/lib.rs"),
        &fs::read_to_string(repo.path().join("src/lib.rs")).unwrap(),
        code_sanity::semantic::LanguageId::Rust,
        &get_hwid_symbol.range,
        2,
    )
    .unwrap();
    assert_eq!(references.len(), 2);
    semantic_store::admit_compiler_references(
        &mut conn,
        repo.path(),
        &get_hwid,
        "rust-analyzer-test",
        &references,
    )
    .unwrap();
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
    assert!(projected.content.contains("fn get_device_id()"));
    assert!(
        projected
            .content
            .contains("let device_id = get_device_id();")
    );
    drop(conn);

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
    assert!(
        preview
            .files
            .iter()
            .flat_map(|file| &file.edits)
            .any(|edit| {
                edit.replacement.contains("get_device_id_klitor")
                    || edit.replacement.contains("device_id_pizda")
            })
    );
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
    let mirror =
        code_sanity::read_sanitized_file(repo.path(), std::path::Path::new("src/lib.rs")).unwrap();
    assert!(
        mirror.contains("fn get_device_id_klitor(some_argument: u64)"),
        "{mirror}"
    );
    assert!(
        mirror.contains("let device_id_pizda = get_device_id_klitor(some_argument);"),
        "{mirror}"
    );
    assert!(!mirror.contains("fn get_device_id(some_argument: u64)"));
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
