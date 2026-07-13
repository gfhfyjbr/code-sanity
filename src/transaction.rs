//! Revision-checked structured edit transactions for MCP v2.

use crate::config::{Layout, normalize_safe_rel_path};
use crate::lock::WorkspaceLock;
use crate::map::sha256_hex;
use crate::patch::{RealFileUpdate, commit_real_file_updates_locked};
use crate::path_projection::PathProjection;
use crate::semantic::{LanguageId, SourceOrigin};
use crate::semantic_store::{self, StoredDocument};
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EditIntent {
    EditNode {
        node_id: String,
        replacement: String,
    },
    RenameSymbol {
        symbol_id: String,
        new_name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedEdit {
    pub rel_path: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub replacement: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_replacement: Option<String>,
    pub source: String,
    pub target_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedFile {
    pub rel_path: String,
    pub before_hash: String,
    pub after_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projected_after_hash: Option<String>,
    pub edits: Vec<PlannedEdit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionPreview {
    pub transaction_id: String,
    pub base_revision: u64,
    pub files: Vec<PlannedFile>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitReport {
    pub transaction_id: String,
    pub base_revision: u64,
    pub committed_revision: u64,
    pub files: Vec<String>,
    pub journal: String,
}

struct IntentSnapshot {
    intent: EditIntent,
    rel_path: String,
    document: StoredDocument,
    source: String,
    range: crate::semantic::TextRange,
    minimum_references: usize,
    real_replacement: Option<String>,
}

pub fn preview_transaction(
    root: &Path,
    expected_revision: u64,
    intents: Vec<EditIntent>,
) -> Result<TransactionPreview> {
    if intents.is_empty() {
        bail!("transaction must contain at least one structured intent");
    }
    let layout = Layout::new(root);
    layout.require_initialized()?;

    // Capture IDs, ranges, source hashes, and capabilities under a short read
    // lock. LSP requests below run after this guard is dropped.
    let snapshots = {
        let _lock = WorkspaceLock::acquire_shared(&layout)?;
        let conn = crate::db::connect(&layout)?;
        crate::db::check_schema(&conn)?;
        require_revision(&conn, expected_revision)?;
        let mut snapshots = Vec::with_capacity(intents.len());
        for intent in intents {
            snapshots.push(snapshot_intent(root, &conn, intent)?);
        }
        snapshots
    };

    let mut edits = Vec::<PlannedEdit>::new();
    for snapshot in &snapshots {
        match &snapshot.intent {
            EditIntent::EditNode {
                node_id,
                replacement: _,
            } => edits.push(PlannedEdit {
                rel_path: snapshot.rel_path.clone(),
                start_byte: snapshot.range.start_byte,
                end_byte: snapshot.range.end_byte,
                replacement: snapshot
                    .real_replacement
                    .clone()
                    .ok_or_else(|| anyhow!("edit_node replacement projection disappeared"))?,
                agent_replacement: match &snapshot.intent {
                    EditIntent::EditNode { replacement, .. } => Some(replacement.clone()),
                    EditIntent::RenameSymbol { .. } => None,
                },
                source: "edit_node".to_string(),
                target_id: node_id.clone(),
            }),
            EditIntent::RenameSymbol {
                symbol_id,
                new_name,
            } => {
                validate_identifier(new_name, snapshot.document.language)?;
                let rel = normalize_safe_rel_path(Path::new(&snapshot.rel_path), "symbol path")?;
                let lsp_edits = crate::lsp::rename(
                    root,
                    &rel,
                    &snapshot.source,
                    snapshot.document.language,
                    &snapshot.range,
                    new_name,
                    snapshot.minimum_references,
                )?;
                edits.extend(lsp_edits.into_iter().map(|edit| PlannedEdit {
                    rel_path: edit.rel_path,
                    start_byte: edit.start_byte,
                    end_byte: edit.end_byte,
                    replacement: edit.new_text,
                    agent_replacement: Some(new_name.clone()),
                    source: "rename_symbol".to_string(),
                    target_id: symbol_id.clone(),
                }));
            }
        }
    }

    let files = {
        let _lock = WorkspaceLock::acquire_shared(&layout)?;
        let conn = crate::db::connect(&layout)?;
        crate::db::check_schema(&conn)?;
        require_revision(&conn, expected_revision)?;
        build_planned_files(root, &layout, &edits)?
    };
    let created_at = Utc::now().to_rfc3339();
    let transaction_id = transaction_id(expected_revision, &intents_json(&snapshots)?, &created_at);
    let mut preview = TransactionPreview {
        transaction_id: transaction_id.clone(),
        base_revision: expected_revision,
        files,
        warnings: Vec::new(),
    };

    // CAS after slow LSP work: neither IDs nor byte ranges may have moved.
    let _lock = WorkspaceLock::acquire(&layout)?;
    let conn = crate::db::connect(&layout)?;
    crate::db::check_schema(&conn)?;
    require_revision(&conn, expected_revision)?;
    verify_preview_sources(root, &preview)?;
    let expected = expected_projected_files(&layout, &conn, &preview)?;
    for file in &mut preview.files {
        file.projected_after_hash = expected
            .get(&file.rel_path)
            .map(|(_, content)| sha256_hex(content.as_bytes()));
    }
    semantic_store::insert_preview_transaction(
        &conn,
        &transaction_id,
        expected_revision,
        &intents_json(&snapshots)?,
        &serde_json::to_string(&preview).context("serialize transaction preview")?,
        &created_at,
    )?;
    project_preview_for_agent(&layout, &conn, &preview)
}

pub fn commit_transaction(
    root: &Path,
    transaction_id: &str,
    expected_revision: u64,
    agent: Option<String>,
    session_id: Option<String>,
) -> Result<CommitReport> {
    let layout = Layout::new(root);
    layout.require_initialized()?;
    let _lock = WorkspaceLock::acquire(&layout)?;
    crate::journal::ensure_no_interrupted_apply(&layout)?;
    let conn = crate::db::connect(&layout)?;
    crate::db::check_schema(&conn)?;
    let config = crate::config::Config::load_or_default(&layout)?;
    let path_projection = PathProjection::from_connection(&config, &conn)?;
    let stored = semantic_store::load_transaction(&conn, transaction_id)?
        .ok_or_else(|| anyhow!("unknown semantic transaction {transaction_id}"))?;
    if stored.base_revision != expected_revision {
        bail!(
            "transaction base revision {} does not match expected revision {expected_revision}",
            stored.base_revision
        );
    }
    let preview: TransactionPreview =
        serde_json::from_str(&stored.preview_json).context("parse stored transaction preview")?;
    if stored.status == "committed" {
        let committed_revision = match stored.committed_revision {
            Some(revision) => revision,
            None => semantic_store::current_revision(&conn)?,
        };
        return Ok(CommitReport {
            transaction_id: transaction_id.to_string(),
            base_revision: stored.base_revision,
            committed_revision,
            files: project_file_names(&path_projection, &preview)?,
            journal: "already-committed".to_string(),
        });
    }
    if stored.status != "previewed" {
        bail!("semantic transaction {transaction_id} is {}", stored.status);
    }
    if preview_after_sources_match(root, &preview)? {
        let semantic = semantic_store::index_workspace_locked(root, &layout)?;
        verify_projected_after_hashes(&layout, &conn, &preview)?;
        semantic_store::mark_transaction_committed(
            &conn,
            transaction_id,
            semantic.revision,
            &Utc::now().to_rfc3339(),
        )?;
        return Ok(CommitReport {
            transaction_id: transaction_id.to_string(),
            base_revision: stored.base_revision,
            committed_revision: semantic.revision,
            files: project_file_names(&path_projection, &preview)?,
            journal: "recovered-after-hash".to_string(),
        });
    }
    require_revision(&conn, expected_revision)?;
    verify_preview_sources(root, &preview)?;
    let expected_mirrors = expected_projected_files(&layout, &conn, &preview)?;

    let mut updates = Vec::with_capacity(preview.files.len());
    for file in &preview.files {
        let rel = normalize_safe_rel_path(Path::new(&file.rel_path), "transaction file")?;
        let before = fs::read_to_string(root.join(&rel))
            .with_context(|| format!("read transaction file {}", file.rel_path))?;
        let after = apply_edits(&before, &file.edits)?;
        if sha256_hex(after.as_bytes()) != file.after_hash {
            bail!(
                "stored transaction preview hash is inconsistent for {}",
                file.rel_path
            );
        }
        updates.push(RealFileUpdate { rel, before, after });
    }

    let journal = commit_real_file_updates_locked(
        root,
        &layout,
        &updates,
        &expected_mirrors,
        agent,
        session_id,
    )?;
    let semantic = semantic_store::index_workspace_locked(root, &layout)?;
    semantic_store::mark_transaction_committed(
        &conn,
        transaction_id,
        semantic.revision,
        &Utc::now().to_rfc3339(),
    )?;
    Ok(CommitReport {
        transaction_id: transaction_id.to_string(),
        base_revision: expected_revision,
        committed_revision: semantic.revision,
        files: project_file_names(&path_projection, &preview)?,
        journal: journal
            .strip_prefix(root)
            .unwrap_or(&journal)
            .to_string_lossy()
            .into_owned(),
    })
}

fn project_preview_for_agent(
    layout: &Layout,
    conn: &rusqlite::Connection,
    preview: &TransactionPreview,
) -> Result<TransactionPreview> {
    let config = crate::config::Config::load_or_default(layout)?;
    let projection = PathProjection::from_connection(&config, conn)?;
    let mut projected = preview.clone();
    for file in &mut projected.files {
        let real_rel = normalize_safe_rel_path(Path::new(&file.rel_path), "preview file")?;
        let map = crate::map::load_span_map(&layout.map_path(&real_rel))
            .with_context(|| format!("load preview projection for {}", file.rel_path))?;
        for edit in &mut file.edits {
            let (start, end) =
                semantic_store::project_original_byte_range(&map, edit.start_byte, edit.end_byte)?;
            edit.start_byte = start;
            edit.end_byte = end;
            edit.rel_path = projection.projected_string_for_real(&edit.rel_path)?;
            if let Some(agent_replacement) = edit.agent_replacement.take() {
                edit.replacement = agent_replacement;
            }
        }
        file.rel_path = projection.projected_string_for_real(&file.rel_path)?;
    }
    Ok(projected)
}

fn project_file_names(
    projection: &PathProjection,
    preview: &TransactionPreview,
) -> Result<Vec<String>> {
    preview
        .files
        .iter()
        .map(|file| projection.projected_string_for_real(&file.rel_path))
        .collect()
}

fn expected_projected_files(
    layout: &Layout,
    conn: &rusqlite::Connection,
    preview: &TransactionPreview,
) -> Result<BTreeMap<String, (std::path::PathBuf, String)>> {
    let agent_preview = project_preview_for_agent(layout, conn, preview)?;
    let mut expected = BTreeMap::new();
    for (real_file, projected_file) in preview.files.iter().zip(&agent_preview.files) {
        let projected_rel = normalize_safe_rel_path(
            Path::new(&projected_file.rel_path),
            "projected transaction file",
        )?;
        let before =
            fs::read_to_string(layout.mirror_dir.join(&projected_rel)).with_context(|| {
                format!(
                    "read projected transaction file {}",
                    projected_rel.display()
                )
            })?;
        let after = apply_edits(&before, &projected_file.edits)?;
        expected.insert(real_file.rel_path.clone(), (projected_rel, after));
    }
    Ok(expected)
}

fn verify_projected_after_hashes(
    layout: &Layout,
    conn: &rusqlite::Connection,
    preview: &TransactionPreview,
) -> Result<()> {
    let config = crate::config::Config::load_or_default(layout)?;
    let projection = PathProjection::from_connection(&config, conn)?;
    for file in &preview.files {
        let Some(expected_hash) = file.projected_after_hash.as_ref() else {
            // Compatibility with previews created before projected hashes were
            // persisted. Normal (non-recovery) commits still use exact bytes.
            continue;
        };
        let projected_rel = projection.projected_for_real(Path::new(&file.rel_path))?;
        let actual = fs::read(layout.mirror_dir.join(&projected_rel)).with_context(|| {
            format!(
                "read recovered transaction projection {}",
                projected_rel.display()
            )
        })?;
        if sha256_hex(&actual) != *expected_hash {
            bail!(
                "{}: recovered real source does not reproduce the stored structured preview",
                projected_rel.display()
            );
        }
    }
    Ok(())
}

fn snapshot_intent(
    root: &Path,
    conn: &rusqlite::Connection,
    intent: EditIntent,
) -> Result<IntentSnapshot> {
    match &intent {
        EditIntent::EditNode { node_id, .. } => {
            let node = semantic_store::load_node(conn, node_id)?
                .ok_or_else(|| anyhow!("unknown node_id {node_id}"))?;
            if node.is_declaration
                || semantic_store::range_contains_declaration(
                    conn,
                    &node.rel_path,
                    node.range.start_byte,
                    node.range.end_byte,
                )?
            {
                bail!(
                    "edit_node target {node_id} contains a declaration; use rename_symbol for declaration changes"
                );
            }
            let document = semantic_store::load_document(conn, &node.rel_path)?
                .ok_or_else(|| anyhow!("semantic document disappeared for {node_id}"))?;
            require_owned_editable(&document)?;
            let source = read_snapshot_source(root, &document)?;
            let projected = semantic_store::project_document(conn, root, &node.rel_path)?;
            let projected_range = projected
                .nodes
                .iter()
                .find(|candidate| candidate.node_id == *node_id)
                .map(|candidate| candidate.range.clone())
                .ok_or_else(|| anyhow!("projected node_id {node_id} disappeared"))?;
            let replacement = match &intent {
                EditIntent::EditNode { replacement, .. } => replacement,
                EditIntent::RenameSymbol { .. } => unreachable!(),
            };
            let mut projected_after = projected.content;
            projected_after.replace_range(
                projected_range.start_byte..projected_range.end_byte,
                replacement,
            );
            let replacement_end = projected_range.start_byte + replacement.len();
            let layout = Layout::new(root);
            let config = crate::config::Config::load_or_default(&layout)?;
            let rel = normalize_safe_rel_path(Path::new(&node.rel_path), "edit_node path")?;
            let real_replacement = crate::patch::back_project_agent_fragment(
                conn,
                &config,
                &rel,
                &projected_after,
                projected_range.start_byte,
                replacement_end,
            )?;
            Ok(IntentSnapshot {
                intent,
                rel_path: node.rel_path,
                document,
                source,
                range: node.range,
                minimum_references: 0,
                real_replacement: Some(real_replacement),
            })
        }
        EditIntent::RenameSymbol { symbol_id, .. } => {
            let (rel_path, symbol) = semantic_store::load_symbol_with_path(conn, symbol_id)?
                .ok_or_else(|| anyhow!("unknown symbol_id {symbol_id}"))?;
            let document = semantic_store::load_document(conn, &rel_path)?
                .ok_or_else(|| anyhow!("semantic document disappeared for {symbol_id}"))?;
            require_owned_editable(&document)?;
            if document.capabilities.semantic_provider.is_none() {
                bail!("rename_symbol requires an available compiler/LSP backend");
            }
            let source = read_snapshot_source(root, &document)?;
            let minimum_references = semantic_store::occurrences_for_symbol(conn, symbol_id)?.len();
            Ok(IntentSnapshot {
                intent,
                rel_path,
                document,
                source,
                range: symbol.range,
                minimum_references,
                real_replacement: None,
            })
        }
    }
}

fn read_snapshot_source(root: &Path, document: &StoredDocument) -> Result<String> {
    let rel = normalize_safe_rel_path(Path::new(&document.rel_path), "semantic document")?;
    let source = fs::read_to_string(root.join(rel))
        .with_context(|| format!("read semantic source {}", document.rel_path))?;
    if sha256_hex(source.as_bytes()) != document.content_hash {
        bail!(
            "{} changed since semantic revision; run code-sanity index",
            document.rel_path
        );
    }
    Ok(source)
}

fn require_owned_editable(document: &StoredDocument) -> Result<()> {
    if document.origin != SourceOrigin::Owned {
        bail!(
            "{} is {:?} code and cannot be mutated",
            document.rel_path,
            document.origin
        );
    }
    if !document.capabilities.edit {
        bail!(
            "{} is read-only: {}",
            document.rel_path,
            document
                .capabilities
                .read_only_reason
                .as_deref()
                .unwrap_or("language backend cannot edit")
        );
    }
    Ok(())
}

fn build_planned_files(
    root: &Path,
    layout: &Layout,
    edits: &[PlannedEdit],
) -> Result<Vec<PlannedFile>> {
    if edits.is_empty() {
        bail!("structured intents produced no edits");
    }
    let conn = crate::db::connect(layout)?;
    crate::db::check_schema(&conn)?;
    let mut by_file = BTreeMap::<String, Vec<PlannedEdit>>::new();
    for edit in edits {
        by_file
            .entry(edit.rel_path.clone())
            .or_default()
            .push(edit.clone());
    }
    let mut files = Vec::with_capacity(by_file.len());
    for (rel_path, mut edits) in by_file {
        let document = semantic_store::load_document(&conn, &rel_path)?
            .ok_or_else(|| anyhow!("LSP returned unindexed file {rel_path}"))?;
        require_owned_editable(&document)?;
        let rel = normalize_safe_rel_path(Path::new(&rel_path), "planned edit")?;
        let before = fs::read_to_string(root.join(rel))
            .with_context(|| format!("read planned edit file {rel_path}"))?;
        if sha256_hex(before.as_bytes()) != document.content_hash {
            bail!("{rel_path} changed during transaction preview");
        }
        edits.sort_by_key(|edit| (edit.start_byte, edit.end_byte));
        validate_non_overlapping(&before, &edits)?;
        let after = apply_edits(&before, &edits)?;
        if after == before {
            bail!("structured edits produce no change in {rel_path}");
        }
        let parsed = crate::semantic::parse_document(Path::new(&rel_path), &after)?;
        if parsed.parse_errors > document.parse_errors {
            bail!(
                "structured edits introduce {} new parse error(s) in {rel_path}",
                parsed.parse_errors - document.parse_errors
            );
        }
        files.push(PlannedFile {
            rel_path,
            before_hash: document.content_hash,
            after_hash: sha256_hex(after.as_bytes()),
            projected_after_hash: None,
            edits,
        });
    }
    Ok(files)
}

fn validate_non_overlapping(source: &str, edits: &[PlannedEdit]) -> Result<()> {
    let mut previous_end = 0usize;
    for edit in edits {
        if edit.start_byte > edit.end_byte
            || edit.end_byte > source.len()
            || !source.is_char_boundary(edit.start_byte)
            || !source.is_char_boundary(edit.end_byte)
        {
            bail!(
                "structured edit {} has an invalid UTF-8 range",
                edit.target_id
            );
        }
        if edit.start_byte < previous_end {
            bail!("structured edits overlap at target {}", edit.target_id);
        }
        previous_end = edit.end_byte;
    }
    Ok(())
}

fn apply_edits(source: &str, edits: &[PlannedEdit]) -> Result<String> {
    validate_non_overlapping(source, edits)?;
    let mut result = source.to_string();
    for edit in edits.iter().rev() {
        result.replace_range(edit.start_byte..edit.end_byte, &edit.replacement);
    }
    Ok(result)
}

fn verify_preview_sources(root: &Path, preview: &TransactionPreview) -> Result<()> {
    for file in &preview.files {
        let rel = normalize_safe_rel_path(Path::new(&file.rel_path), "transaction preview")?;
        let current = fs::read_to_string(root.join(rel))
            .with_context(|| format!("read transaction preview file {}", file.rel_path))?;
        if sha256_hex(current.as_bytes()) != file.before_hash {
            bail!(
                "{} changed since transaction preview; discard it and preview again",
                file.rel_path
            );
        }
    }
    Ok(())
}

fn preview_after_sources_match(root: &Path, preview: &TransactionPreview) -> Result<bool> {
    for file in &preview.files {
        let rel = normalize_safe_rel_path(Path::new(&file.rel_path), "transaction recovery")?;
        let current = fs::read_to_string(root.join(rel))
            .with_context(|| format!("read transaction recovery file {}", file.rel_path))?;
        if sha256_hex(current.as_bytes()) != file.after_hash {
            return Ok(false);
        }
    }
    Ok(true)
}

fn require_revision(conn: &rusqlite::Connection, expected: u64) -> Result<()> {
    let actual = semantic_store::current_revision(conn)?;
    if actual != expected {
        bail!("stale semantic revision: expected {expected}, current {actual}");
    }
    Ok(())
}

fn validate_identifier(value: &str, _language: LanguageId) -> Result<()> {
    let mut chars = value.chars();
    if !chars
        .next()
        .is_some_and(|character| character == '_' || character.is_alphabetic())
        || !chars.all(|character| character == '_' || character.is_alphanumeric())
    {
        bail!("rename target {value:?} is not a valid identifier");
    }
    Ok(())
}

fn intents_json(snapshots: &[IntentSnapshot]) -> Result<String> {
    serde_json::to_string(
        &snapshots
            .iter()
            .map(|snapshot| &snapshot.intent)
            .collect::<Vec<_>>(),
    )
    .context("serialize transaction intents")
}

fn transaction_id(revision: u64, intents: &str, created_at: &str) -> String {
    let material = format!("{revision}\0{created_at}\0{intents}");
    format!("tx_{}", &sha256_hex(material.as_bytes())[..24])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_are_applied_from_the_end_without_offset_drift() {
        let edits = vec![
            PlannedEdit {
                rel_path: "a.rs".into(),
                start_byte: 0,
                end_byte: 3,
                replacement: "long".into(),
                agent_replacement: None,
                source: "test".into(),
                target_id: "a".into(),
            },
            PlannedEdit {
                rel_path: "a.rs".into(),
                start_byte: 4,
                end_byte: 7,
                replacement: "x".into(),
                agent_replacement: None,
                source: "test".into(),
                target_id: "b".into(),
            },
        ];
        assert_eq!(apply_edits("one two", &edits).unwrap(), "long x");
    }

    #[test]
    fn overlapping_edits_are_rejected() {
        let edits = vec![
            PlannedEdit {
                rel_path: "a.rs".into(),
                start_byte: 0,
                end_byte: 3,
                replacement: String::new(),
                agent_replacement: None,
                source: "test".into(),
                target_id: "a".into(),
            },
            PlannedEdit {
                rel_path: "a.rs".into(),
                start_byte: 2,
                end_byte: 4,
                replacement: String::new(),
                agent_replacement: None,
                source: "test".into(),
                target_id: "b".into(),
            },
        ];
        assert!(validate_non_overlapping("abcd", &edits).is_err());
    }
}
