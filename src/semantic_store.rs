//! SQLite persistence for the versioned semantic index.

use crate::config::{Layout, normalize_safe_rel_path};
use crate::db;
use crate::map::{
    PendingReplacement, RenderedSanitization, SpanMap, load_span_map, render_with_map, sha256_hex,
};
use crate::semantic::{
    LanguageId, OccurrenceRole, ParsedDocument, SEMANTIC_RESOLVER_VERSION, SemanticOccurrence,
    SemanticSymbol, SourceOrigin, TextRange,
};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize)]
pub struct SemanticIndexReport {
    pub revision: u64,
    pub indexed: usize,
    pub unchanged: usize,
    pub removed: usize,
    pub parse_errors: usize,
    pub read_only: usize,
    /// Legacy declaration/definition anchors converged onto one canonical
    /// accepted alias during this index pass.
    pub reconciled_aliases: usize,
    /// Decisions accepted by older releases but removed because they violate
    /// current completeness or workspace-injectivity guarantees.
    pub quarantined_aliases: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceSnapshot {
    pub revision: u64,
    pub documents: usize,
    pub symbols: usize,
    pub occurrences: usize,
    pub unresolved_occurrences: usize,
    pub external_occurrences: usize,
    pub aliases: usize,
}

#[derive(Debug, Clone)]
struct DocumentState {
    content_hash: String,
    resolver_version: u32,
}

#[derive(Debug, Clone)]
pub struct StoredDocument {
    pub rel_path: String,
    pub language: LanguageId,
    pub content_hash: String,
    pub origin: SourceOrigin,
    pub capabilities: crate::semantic::BackendCapabilities,
    pub parse_errors: usize,
}

#[derive(Debug, Clone)]
pub struct StoredNode {
    pub node_id: String,
    pub rel_path: String,
    pub kind: String,
    pub range: TextRange,
    pub is_declaration: bool,
}

#[derive(Debug, Clone)]
pub struct StoredTransaction {
    pub transaction_id: String,
    pub base_revision: u64,
    pub status: String,
    pub intents_json: String,
    pub preview_json: String,
    pub committed_revision: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectedOccurrence {
    pub occurrence_id: String,
    pub node_id: String,
    pub symbol_id: Option<String>,
    /// Agent-facing spelling. The real spelling never leaves the semantic
    /// projection boundary.
    pub name: String,
    /// Backward-compatible synonym for `name`; both are projected.
    pub projected_name: String,
    pub role: OccurrenceRole,
    /// Range in `ProjectedDocument::content`.
    pub range: TextRange,
    pub projected_start_byte: usize,
    pub projected_end_byte: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectedDocument {
    pub rel_path: String,
    pub revision: u64,
    pub language: LanguageId,
    pub capabilities: crate::semantic::BackendCapabilities,
    pub content: String,
    pub nodes: Vec<crate::semantic::SemanticNode>,
    pub symbols: Vec<SemanticSymbol>,
    pub occurrences: Vec<ProjectedOccurrence>,
}

#[derive(Debug, Clone, PartialEq)]
struct AliasRow {
    original_name: String,
    sanitized_name: String,
    category: String,
    confidence: Option<f64>,
    reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticAliasPair {
    pub symbol_id: String,
    pub original: String,
    pub alias: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QuarantinedSemanticAlias {
    pub symbol_id: String,
    pub original: String,
    pub alias: String,
    pub reason: String,
}

/// Every accepted symbol binding. Safety and queue reconciliation use this
/// non-deduplicated view so one complete member cannot hide an incomplete
/// independent symbol that happens to share the same textual mapping.
pub fn accepted_alias_bindings(conn: &Connection) -> Result<Vec<SemanticAliasPair>> {
    let mut statement = conn
        .prepare(
            r#"
            select symbol_id, original_name, sanitized_name, source
            from semantic_aliases
            where status = 'accepted'
            order by sanitized_name, original_name, symbol_id
            "#,
        )
        .context("prepare accepted semantic alias query")?;
    let rows = statement
        .query_map([], |row| {
            Ok(SemanticAliasPair {
                symbol_id: row.get(0)?,
                original: row.get(1)?,
                alias: row.get(2)?,
                source: row.get(3)?,
            })
        })
        .context("query accepted semantic aliases")?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("collect accepted semantic alias bindings")
}

/// Active semantic aliases, deduplicated by textual mapping. Rendering and
/// output redaction only need one copy of a repeated original/alias pair.
pub fn accepted_alias_pairs(conn: &Connection) -> Result<Vec<SemanticAliasPair>> {
    let mut pairs = accepted_alias_bindings(conn)?;
    pairs.dedup_by(|left, right| left.original == right.original && left.alias == right.alias);
    Ok(pairs)
}

/// Overlay exact, symbol-bound semantic aliases onto a lexical render. Both
/// projections originate from real-source byte ranges, so rebuilding one map
/// keeps mirror reads and back-projection on a single coordinate system.
pub fn merge_semantic_aliases(
    conn: &Connection,
    rel_path: &str,
    original: &str,
    lexical: RenderedSanitization,
) -> Result<RenderedSanitization> {
    validate_lexical_map_against_semantic_aliases(conn, &lexical.span_map)?;
    let Some(document) = load_document(conn, rel_path)? else {
        return Ok(lexical);
    };
    if document.content_hash != sha256_hex(original.as_bytes()) {
        // The semantic index is refreshed after lexical indexing. Never apply
        // stale byte ranges; the post-semantic mirror refresh will converge it.
        return Ok(lexical);
    }

    let mut statement = conn
        .prepare(
            r#"
            select o.occurrence_id, o.symbol_id, o.name, o.start_byte, o.end_byte,
                   a.sanitized_name, a.confidence
            from semantic_occurrences o
            join semantic_aliases a on a.symbol_id = o.symbol_id
            where o.rel_path = ?1 and a.status = 'accepted'
            order by o.start_byte, o.end_byte
            "#,
        )
        .context("prepare semantic mirror overlay")?;
    let semantic = statement
        .query_map([rel_path], |row| {
            let occurrence_id = row.get::<_, String>(0)?;
            let symbol_id = row.get::<_, String>(1)?;
            let original_text = row.get::<_, String>(2)?;
            let start = row.get::<_, i64>(3)? as usize;
            let end = row.get::<_, i64>(4)? as usize;
            let alias = row.get::<_, String>(5)?;
            Ok(PendingReplacement {
                category: "semantic_identifier".to_string(),
                sanitized_text: crate::sanitize::adapt_replacement(&original_text, &alias),
                confidence: row.get::<_, Option<f64>>(6)?.unwrap_or(1.0),
                policy_source: "semantic-alias".to_string(),
                stable_key: format!("semantic:{symbol_id}:{occurrence_id}"),
                original_text,
                original_start: start,
                original_end: end,
            })
        })
        .context("query semantic mirror overlay")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("collect semantic mirror overlay")?;
    if semantic.is_empty() {
        return Ok(lexical);
    }

    let mut replacements = lexical
        .span_map
        .replacements
        .into_iter()
        .filter(|lexical| {
            !semantic.iter().any(|semantic| {
                lexical.original_start < semantic.original_end
                    && lexical.original_end > semantic.original_start
            })
        })
        .map(|replacement| PendingReplacement {
            category: replacement.category,
            original_text: replacement.original_text,
            sanitized_text: replacement.sanitized_text,
            confidence: replacement.confidence,
            policy_source: replacement.policy_source,
            stable_key: replacement.stable_key,
            original_start: replacement.original_start,
            original_end: replacement.original_end,
        })
        .collect::<Vec<_>>();
    replacements.extend(semantic);
    render_with_map(
        rel_path,
        original,
        &lexical.span_map.language,
        replacements,
        Utc::now().to_rfc3339(),
    )
}

/// Lexical and symbol-scoped policy share one agent namespace. A spelling may
/// map the same original through both layers, but it may never denote two
/// different originals: patch/structured-edit back-projection would otherwise
/// have no correct answer.
fn validate_lexical_map_against_semantic_aliases(
    conn: &Connection,
    lexical_map: &SpanMap,
) -> Result<()> {
    let aliases = accepted_alias_pairs(conn)?;
    if aliases.is_empty() {
        return Ok(());
    }
    let mut semantic = BTreeMap::<String, BTreeSet<String>>::new();
    for pair in aliases {
        semantic
            .entry(crate::sanitize::normalize_term(&pair.alias))
            .or_default()
            .insert(crate::sanitize::normalize_term(&pair.original));
    }
    for replacement in &lexical_map.replacements {
        if replacement.policy_source == "semantic-alias" {
            continue;
        }
        let alias = crate::sanitize::normalize_term(&replacement.sanitized_text);
        let Some(originals) = semantic.get(&alias) else {
            continue;
        };
        let original = crate::sanitize::normalize_term(&replacement.original_text);
        if !originals.contains(&original) {
            bail!(
                "alias {:?} maps both lexical term {:?} and a different semantic symbol; choose a workspace-unique alias",
                replacement.sanitized_text,
                replacement.original_text
            );
        }
    }
    Ok(())
}

pub fn semantic_projection_matches_map(
    conn: &Connection,
    rel_path: &str,
    map: &SpanMap,
) -> Result<bool> {
    let mut statement = conn
        .prepare(
            r#"
            select o.occurrence_id, o.symbol_id, o.name, o.start_byte, o.end_byte,
                   a.sanitized_name
            from semantic_occurrences o
            join semantic_aliases a on a.symbol_id = o.symbol_id
            where o.rel_path = ?1 and a.status = 'accepted'
            order by o.start_byte, o.end_byte
            "#,
        )
        .context("prepare semantic projection fingerprint")?;
    let expected = statement
        .query_map([rel_path], |row| {
            let occurrence_id = row.get::<_, String>(0)?;
            let symbol_id = row.get::<_, String>(1)?;
            let original = row.get::<_, String>(2)?;
            let alias = row.get::<_, String>(5)?;
            Ok((
                format!("semantic:{symbol_id}:{occurrence_id}"),
                row.get::<_, i64>(3)? as usize,
                row.get::<_, i64>(4)? as usize,
                crate::sanitize::adapt_replacement(&original, &alias),
            ))
        })
        .context("query semantic projection fingerprint")?
        .collect::<rusqlite::Result<BTreeSet<_>>>()
        .context("collect semantic projection fingerprint")?;
    let actual = map
        .replacements
        .iter()
        .filter(|replacement| replacement.policy_source == "semantic-alias")
        .map(|replacement| {
            (
                replacement.stable_key.clone(),
                replacement.original_start,
                replacement.original_end,
                replacement.sanitized_text.clone(),
            )
        })
        .collect::<BTreeSet<_>>();
    Ok(expected == actual)
}

/// Incrementally refresh semantic documents while the caller holds the
/// exclusive workspace lock. Parsing is local CPU work; no LSP/model request
/// is made from this path.
pub(crate) fn index_workspace_locked(root: &Path, layout: &Layout) -> Result<SemanticIndexReport> {
    let mut conn = db::connect(layout)?;
    db::ensure_schema(&conn)?;
    let previous = document_states(&conn)?;
    let tracked = db::tracked_files(&conn)?;
    let lexical_hashes = db::all_index_states(&conn)?
        .into_iter()
        .map(|state| (state.rel_path, state.input_sha256))
        .collect::<BTreeMap<_, _>>();
    let tracked_set = tracked.iter().cloned().collect::<BTreeSet<_>>();
    let mut report = SemanticIndexReport::default();
    let mut documents = Vec::new();

    for stored_path in tracked {
        if previous.get(&stored_path).is_some_and(|state| {
            Some(&state.content_hash) == lexical_hashes.get(&stored_path)
                && state.resolver_version >= SEMANTIC_RESOLVER_VERSION
        }) {
            report.unchanged += 1;
            continue;
        }
        let rel = match normalize_safe_rel_path(Path::new(&stored_path), "semantic document") {
            Ok(rel) => rel,
            Err(err) => {
                report.errors.push(format!("{stored_path}: {err:#}"));
                continue;
            }
        };
        let source = match fs::read_to_string(root.join(&rel)) {
            Ok(source) => source,
            Err(err) => {
                report.errors.push(format!("{stored_path}: read: {err}"));
                continue;
            }
        };
        let content_hash = sha256_hex(source.as_bytes());
        debug_assert_eq!(lexical_hashes.get(&stored_path), Some(&content_hash));
        match crate::semantic::parse_document(&rel, &source) {
            Ok(mut document) => {
                if previous
                    .get(&stored_path)
                    .is_some_and(|state| state.content_hash == document.content_hash)
                {
                    preserve_existing_symbol_ids(&conn, &mut document)?;
                }
                report.parse_errors += document.parse_errors;
                report.read_only += usize::from(!document.capabilities.edit);
                report.indexed += 1;
                documents.push(document);
            }
            Err(err) => report
                .errors
                .push(format!("{stored_path}: semantic parse: {err:#}")),
        }
    }

    let removed = previous
        .keys()
        .filter(|path| !tracked_set.contains(*path))
        .cloned()
        .collect::<Vec<_>>();
    report.removed = removed.len();
    report.revision = replace_documents(&mut conn, &documents, &removed)?;
    report.revision = sync_aliases_from_maps(&mut conn, layout)?;
    refresh_stale_compiler_aliases_locked(root, &mut conn)?;
    let mut quarantined = quarantine_unrestored_stale_aliases(&mut conn)?;
    report.reconciled_aliases =
        crate::proposal::reconcile_equivalent_semantic_aliases(root, &mut conn)?;
    quarantined.extend(quarantine_legacy_invalid_accepted_aliases(&mut conn)?);
    report.quarantined_aliases = quarantined.len();
    crate::proposal::forget_quarantined_alias_decisions(layout, &quarantined)?;
    report.revision = current_revision(&conn)?;
    drop(conn);
    crate::index::refresh_semantic_mirrors_locked(root, layout)
        .context("refresh unified semantic mirrors")?;
    crate::proposal::reconcile_review_queue_locked(root, layout)
        .context("retire stale semantic review targets")?;
    Ok(report)
}

fn refresh_stale_compiler_aliases_locked(root: &Path, conn: &mut Connection) -> Result<()> {
    let canonical_ids = {
        let mut statement = conn
            .prepare(
                r#"
                select distinct link.canonical_symbol_id
                from semantic_compiler_links link
                join semantic_aliases alias on alias.symbol_id = link.linked_symbol_id
                where alias.status = 'stale'
                order by link.canonical_symbol_id
                "#,
            )
            .context("prepare stale compiler alias refresh")?;
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .context("query stale compiler aliases")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("collect stale compiler aliases")?
    };

    for canonical_id in canonical_ids {
        let result = (|| -> Result<()> {
            let (rel_path, symbol) = load_symbol_with_path(conn, &canonical_id)?
                .ok_or_else(|| anyhow::anyhow!("stale compiler symbol disappeared"))?;
            let document = load_document(conn, &rel_path)?
                .ok_or_else(|| anyhow::anyhow!("stale compiler document disappeared"))?;
            let decision = conn
                .query_row(
                    r#"
                    select alias.sanitized_name, alias.category, alias.confidence, alias.reason
                    from semantic_compiler_links link
                    join semantic_aliases alias on alias.symbol_id = link.linked_symbol_id
                    where link.canonical_symbol_id = ?1 and alias.status = 'stale'
                    order by alias.symbol_id limit 1
                    "#,
                    [&canonical_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<f64>>(2)?.unwrap_or(1.0),
                            row.get::<_, Option<String>>(3)?,
                        ))
                    },
                )
                .optional()
                .context("load stale semantic alias decision")?;
            let Some(decision) = decision else {
                // An earlier canonical anchor in this same refresh pass may
                // already have restored every linked alias in the component.
                return Ok(());
            };
            let rel = normalize_safe_rel_path(Path::new(&rel_path), "compiler alias refresh")?;
            let source = fs::read_to_string(root.join(&rel))
                .with_context(|| format!("read compiler alias refresh source {rel_path}"))?;
            if sha256_hex(source.as_bytes()) != document.content_hash {
                bail!("{rel_path} changed during compiler alias refresh");
            }
            let (provider, locations) = if let Some(locations) =
                translation_unit_local_reference_closure(conn, root, &canonical_id)?
            {
                ("syntax:translation-unit-local".to_string(), locations)
            } else {
                let provider =
                    document
                        .capabilities
                        .semantic_provider
                        .clone()
                        .ok_or_else(|| {
                            anyhow::anyhow!("semantic provider unavailable for {rel_path}")
                        })?;
                let locations = crate::lsp::references(
                    root,
                    &rel,
                    &source,
                    document.language,
                    &symbol.range,
                    0,
                )?;
                (provider, locations)
            };
            admit_compiler_references(conn, root, &canonical_id, &provider, &locations)?;
            accept_symbol_alias(
                conn,
                &canonical_id,
                &decision.0,
                &decision.1,
                decision.2,
                decision.3.as_deref(),
            )?;
            Ok(())
        })();
        if let Err(err) = result {
            log::warn!(
                "compiler alias {canonical_id} remains stale and will be quarantined: {err:#}"
            );
        }
    }
    Ok(())
}

fn remove_compiler_components_for_symbols(
    tx: &Transaction<'_>,
    symbol_ids: impl IntoIterator<Item = String>,
) -> Result<()> {
    let symbol_ids = symbol_ids.into_iter().collect::<BTreeSet<_>>();
    let mut canonical_ids = symbol_ids.clone();
    {
        let mut statement = tx
            .prepare(
                r#"
                select canonical_symbol_id from semantic_compiler_links
                where canonical_symbol_id = ?1 or linked_symbol_id = ?1
                "#,
            )
            .context("prepare quarantined compiler components")?;
        for symbol_id in &symbol_ids {
            let rows = statement
                .query_map([symbol_id], |row| row.get::<_, String>(0))
                .context("query quarantined compiler component")?;
            for row in rows {
                canonical_ids.insert(row.context("read quarantined compiler canonical ID")?);
            }
        }
    }
    for canonical_id in canonical_ids {
        tx.execute(
            "delete from semantic_compiler_bindings where canonical_symbol_id = ?1",
            [&canonical_id],
        )
        .context("remove quarantined compiler bindings")?;
        tx.execute(
            "delete from semantic_compiler_resolutions where canonical_symbol_id = ?1",
            [&canonical_id],
        )
        .context("remove quarantined compiler resolution")?;
        tx.execute(
            "delete from semantic_compiler_links where canonical_symbol_id = ?1",
            [&canonical_id],
        )
        .context("remove quarantined compiler links")?;
    }
    Ok(())
}

pub(crate) fn quarantine_unrestored_stale_aliases(
    conn: &mut Connection,
) -> Result<Vec<QuarantinedSemanticAlias>> {
    let aliases = {
        let mut statement = conn
            .prepare(
                r#"
                select symbol_id, original_name, sanitized_name, source
                from semantic_aliases where status = 'stale' order by symbol_id
                "#,
            )
            .context("prepare unrestored stale semantic aliases")?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .context("query unrestored stale semantic aliases")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("collect unrestored stale semantic aliases")?
    };
    if aliases.is_empty() {
        return Ok(Vec::new());
    }
    if let Some((_, original, alias, _)) = aliases
        .iter()
        .find(|(_, _, _, source)| source != "proposal-v2")
    {
        bail!(
            "policy-derived semantic alias {alias:?} for {original:?} became stale; fix the sanitizer config and reindex"
        );
    }

    let quarantined = aliases
        .iter()
        .map(|(symbol_id, original, alias, _)| QuarantinedSemanticAlias {
            symbol_id: symbol_id.clone(),
            original: original.clone(),
            alias: alias.clone(),
            reason: "compiler reference closure could not be restored".to_string(),
        })
        .collect::<Vec<_>>();
    let tx = conn
        .transaction()
        .context("begin stale semantic alias quarantine")?;
    let base_revision = current_revision(&tx)?;
    remove_compiler_components_for_symbols(
        &tx,
        quarantined.iter().map(|alias| alias.symbol_id.clone()),
    )?;
    for alias in &quarantined {
        tx.execute(
            "delete from semantic_aliases where symbol_id = ?1 and status = 'stale' and source = 'proposal-v2'",
            [&alias.symbol_id],
        )
        .with_context(|| format!("quarantine stale semantic alias for {}", alias.symbol_id))?;
        tx.execute(
            "update semantic_proposals set status = 'stale' where symbol_id = ?1",
            [&alias.symbol_id],
        )
        .context("make stale semantic proposal retryable")?;
    }
    let next_revision = base_revision
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("semantic workspace revision overflow"))?;
    let updated = tx
        .execute(
            "update semantic_workspace set revision = ?2 where singleton = 1 and revision = ?1",
            params![base_revision as i64, next_revision as i64],
        )
        .context("advance stale semantic alias quarantine revision")?;
    if updated != 1 {
        bail!("semantic workspace changed during stale alias quarantine");
    }
    tx.commit()
        .context("commit stale semantic alias quarantine")?;
    log::warn!(
        "quarantined {} semantic alias(es) whose compiler closure could not be restored",
        quarantined.len()
    );
    Ok(quarantined)
}

fn sync_aliases_from_maps(conn: &mut Connection, layout: &Layout) -> Result<u64> {
    let fingerprint = alias_map_fingerprint(conn)?;
    let stored_fingerprint = conn
        .query_row(
            "select alias_fingerprint from semantic_workspace where singleton = 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .context("read semantic alias fingerprint")?;
    if stored_fingerprint == fingerprint {
        return current_revision(conn);
    }
    let mut desired = BTreeMap::<String, AliasRow>::new();
    for rel_path in db::tracked_files(conn)? {
        let rel = match normalize_safe_rel_path(Path::new(&rel_path), "semantic alias path") {
            Ok(rel) => rel,
            Err(_) => continue,
        };
        let map = match load_span_map(&layout.map_path(&rel)) {
            Ok(map) => map,
            Err(_) => continue,
        };
        for replacement in map.replacements {
            let symbol_id = conn
                .query_row(
                    r#"
                    select o.symbol_id
                    from semantic_occurrences o
                    join semantic_symbols s on s.symbol_id = o.symbol_id
                    where o.rel_path = ?1 and o.start_byte = ?2 and o.end_byte = ?3
                      and o.symbol_id is not null and s.origin = 'owned'
                    limit 1
                    "#,
                    params![
                        rel_path,
                        replacement.original_start as i64,
                        replacement.original_end as i64,
                    ],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .context("match lexical replacement to semantic occurrence")?;
            let Some(symbol_id) = symbol_id else {
                continue;
            };
            if !symbol_projection_is_complete(conn, &symbol_id)? {
                continue;
            }
            desired.entry(symbol_id).or_insert_with(|| AliasRow {
                original_name: replacement.original_text,
                sanitized_name: replacement.sanitized_text,
                category: replacement.category,
                confidence: Some(replacement.confidence),
                reason: Some(format!("migrated from v1 {}", replacement.policy_source)),
            });
        }
    }
    {
        let mut statement = conn
            .prepare("select symbol_id from semantic_aliases where source != 'v1'")
            .context("prepare native semantic alias query")?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .context("query native semantic aliases")?;
        for row in rows {
            desired.remove(&row.context("read native semantic alias")?);
        }
    }

    let mut current = BTreeMap::<String, AliasRow>::new();
    {
        let mut statement = conn
            .prepare(
                r#"
                select symbol_id, original_name, sanitized_name, category, confidence, reason
                from semantic_aliases where source = 'v1' order by symbol_id
                "#,
            )
            .context("prepare semantic alias query")?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    AliasRow {
                        original_name: row.get(1)?,
                        sanitized_name: row.get(2)?,
                        category: row.get(3)?,
                        confidence: row.get(4)?,
                        reason: row.get(5)?,
                    },
                ))
            })
            .context("query semantic aliases")?;
        for row in rows {
            let (symbol_id, alias) = row.context("read semantic alias")?;
            current.insert(symbol_id, alias);
        }
    }
    if current == desired {
        conn.execute(
            "update semantic_workspace set alias_fingerprint = ?1 where singleton = 1",
            params![fingerprint],
        )
        .context("refresh semantic alias fingerprint")?;
        return current_revision(conn);
    }

    let tx = conn
        .transaction()
        .context("begin semantic alias transaction")?;
    let base_revision = current_revision(&tx)?;
    let next_revision = base_revision
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("semantic workspace revision overflow"))?;
    tx.execute("delete from semantic_aliases where source = 'v1'", [])
        .context("clear migrated v1 semantic aliases")?;
    for (symbol_id, alias) in desired {
        tx.execute(
            r#"
            insert into semantic_aliases(
              symbol_id, original_name, sanitized_name, category,
              confidence, reason, status, source, created_revision
            ) values(?1, ?2, ?3, ?4, ?5, ?6, 'accepted', 'v1', ?7)
            on conflict(symbol_id) do nothing
            "#,
            params![
                symbol_id,
                alias.original_name,
                alias.sanitized_name,
                alias.category,
                alias.confidence,
                alias.reason,
                next_revision as i64,
            ],
        )
        .context("insert semantic alias")?;
    }
    let updated = tx
        .execute(
            r#"
            update semantic_workspace set revision = ?2, alias_fingerprint = ?3
            where singleton = 1 and revision = ?1
            "#,
            params![base_revision as i64, next_revision as i64, fingerprint],
        )
        .context("advance semantic alias revision")?;
    if updated != 1 {
        bail!("semantic workspace revision changed during alias commit");
    }
    tx.commit().context("commit semantic alias transaction")?;
    Ok(next_revision)
}

fn alias_map_fingerprint(conn: &Connection) -> Result<String> {
    let mut statement = conn
        .prepare("select rel_path, original_hash, sanitized_hash from files order by rel_path")
        .context("prepare alias map fingerprint query")?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .context("query alias map fingerprint")?;
    let mut material = String::new();
    for row in rows {
        let (path, original_hash, sanitized_hash) =
            row.context("read alias map fingerprint row")?;
        material.push_str(&path);
        material.push('\0');
        material.push_str(&original_hash);
        material.push('\0');
        material.push_str(&sanitized_hash);
        material.push('\n');
    }
    Ok(sha256_hex(material.as_bytes()))
}

pub fn current_revision(conn: &Connection) -> Result<u64> {
    let revision = conn
        .query_row(
            "select revision from semantic_workspace where singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .context("read semantic workspace revision")?;
    u64::try_from(revision).context("semantic workspace revision is negative")
}

pub fn document_fingerprint(conn: &Connection) -> Result<String> {
    let mut statement = conn
        .prepare("select rel_path, content_hash from semantic_documents order by rel_path")
        .context("prepare semantic document fingerprint")?;
    let mut material = String::new();
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .context("query semantic document fingerprint")?;
    for row in rows {
        let (path, hash) = row.context("read semantic document fingerprint")?;
        material.push_str(&path);
        material.push('\0');
        material.push_str(&hash);
        material.push('\n');
    }
    Ok(sha256_hex(material.as_bytes()))
}

pub fn document_hashes(conn: &Connection) -> Result<BTreeMap<String, String>> {
    let mut statement = conn
        .prepare("select rel_path, content_hash from semantic_documents order by rel_path")
        .context("prepare semantic document hash query")?;
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .context("query semantic document hashes")?;
    rows.collect::<rusqlite::Result<BTreeMap<_, _>>>()
        .context("collect semantic document hashes")
}

fn document_states(conn: &Connection) -> Result<BTreeMap<String, DocumentState>> {
    let mut statement = conn
        .prepare(
            "select rel_path, content_hash, capabilities_json from semantic_documents order by rel_path",
        )
        .context("prepare semantic document state query")?;
    let rows = statement
        .query_map([], |row| {
            let capabilities_json = row.get::<_, String>(2)?;
            let resolver_version =
                serde_json::from_str::<crate::semantic::BackendCapabilities>(&capabilities_json)
                    .map(|capabilities| capabilities.resolver_version)
                    .unwrap_or_default();
            Ok((
                row.get::<_, String>(0)?,
                DocumentState {
                    content_hash: row.get(1)?,
                    resolver_version,
                },
            ))
        })
        .context("query semantic document states")?;
    rows.collect::<rusqlite::Result<BTreeMap<_, _>>>()
        .context("collect semantic document states")
}

pub(crate) fn semantic_index_is_current(conn: &Connection) -> Result<bool> {
    let documents = document_states(conn)?;
    let lexical = db::all_index_states(conn)?
        .into_iter()
        .map(|state| (state.rel_path, state.input_sha256))
        .collect::<BTreeMap<_, _>>();
    Ok(documents.len() == lexical.len()
        && documents.iter().all(|(rel_path, document)| {
            document.resolver_version >= SEMANTIC_RESOLVER_VERSION
                && lexical.get(rel_path) == Some(&document.content_hash)
        }))
}

/// A resolver upgrade may change qualified names or binding decisions while
/// the source bytes and declaration nodes remain identical. Keep the old
/// symbol IDs in that case so accepted aliases and queued review targets do
/// not become stale merely because code-sanity got smarter. Everything other
/// than identity comes from the new analysis: retaining an old qualified name
/// or an old syntax binding would make resolver fixes impossible to apply.
fn preserve_existing_symbol_ids(conn: &Connection, document: &mut ParsedDocument) -> Result<()> {
    #[derive(Debug)]
    struct ExistingSymbol {
        symbol_id: String,
        node_id: String,
        name: String,
        kind: String,
        start_byte: usize,
        end_byte: usize,
    }

    let mut statement = conn
        .prepare(
            r#"
            select s.symbol_id, s.node_id, s.name, s.kind,
                   n.start_byte, n.end_byte
            from semantic_symbols s
            join semantic_nodes n on n.node_id = s.node_id
            where s.rel_path = ?1
            order by n.start_byte, n.end_byte, s.symbol_id
            "#,
        )
        .context("prepare existing semantic symbol query")?;
    let existing = statement
        .query_map(params![document.rel_path], |row| {
            Ok(ExistingSymbol {
                symbol_id: row.get(0)?,
                node_id: row.get(1)?,
                name: row.get(2)?,
                kind: row.get(3)?,
                start_byte: row.get::<_, i64>(4)? as usize,
                end_byte: row.get::<_, i64>(5)? as usize,
            })
        })
        .context("query existing semantic symbols")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("collect existing semantic symbols")?;

    let mut by_node = BTreeMap::<(String, String, String), Vec<usize>>::new();
    let mut by_range = BTreeMap::<(usize, usize, String, String), Vec<usize>>::new();
    let mut by_loose_range = BTreeMap::<(usize, usize, String), Vec<usize>>::new();
    for (index, symbol) in existing.iter().enumerate() {
        by_node
            .entry((
                symbol.node_id.clone(),
                symbol.name.clone(),
                symbol.kind.clone(),
            ))
            .or_default()
            .push(index);
        by_range
            .entry((
                symbol.start_byte,
                symbol.end_byte,
                symbol.name.clone(),
                symbol.kind.clone(),
            ))
            .or_default()
            .push(index);
        by_loose_range
            .entry((symbol.start_byte, symbol.end_byte, symbol.name.clone()))
            .or_default()
            .push(index);
    }

    let mut used = BTreeSet::<usize>::new();
    let mut preservation_plan = Vec::<(usize, usize)>::new();
    for (symbol_index, symbol) in document.symbols.iter().enumerate() {
        let node_key = (
            symbol.node_id.clone(),
            symbol.name.clone(),
            symbol.kind.clone(),
        );
        let range_key = (
            symbol.range.start_byte,
            symbol.range.end_byte,
            symbol.name.clone(),
            symbol.kind.clone(),
        );
        let old_index = by_node
            .get(&node_key)
            .into_iter()
            .flatten()
            .chain(by_range.get(&range_key).into_iter().flatten())
            .chain(
                by_loose_range
                    .get(&(
                        symbol.range.start_byte,
                        symbol.range.end_byte,
                        symbol.name.clone(),
                    ))
                    .into_iter()
                    .flatten(),
            )
            .copied()
            .find(|index| !used.contains(index));
        let Some(old_index) = old_index else {
            continue;
        };
        used.insert(old_index);
        preservation_plan.push((symbol_index, old_index));
    }
    for (symbol_index, old_index) in preservation_plan {
        let old_id = existing[old_index].symbol_id.clone();
        let generated_id = document.symbols[symbol_index].symbol_id.clone();
        if old_id == generated_id {
            continue;
        }
        if let Some(collision_index) = document
            .symbols
            .iter()
            .enumerate()
            .find(|(index, symbol)| *index != symbol_index && symbol.symbol_id == old_id)
            .map(|(index, _)| index)
        {
            let collision_node = document.symbols[collision_index].node_id.clone();
            let material = format!(
                "resolver-upgrade-collision\0{}\0{}\0{}",
                document.rel_path, old_id, collision_node
            );
            let replacement_id = format!("sym_{}", &sha256_hex(material.as_bytes())[..24]);
            document.symbols[collision_index].symbol_id = replacement_id.clone();
            for occurrence in &mut document.occurrences {
                if occurrence.symbol_id.as_deref() == Some(old_id.as_str()) {
                    occurrence.symbol_id = Some(replacement_id.clone());
                }
            }
        }
        document.symbols[symbol_index].symbol_id = old_id.clone();
        for occurrence in &mut document.occurrences {
            if occurrence.symbol_id.as_deref() == Some(generated_id.as_str()) {
                occurrence.symbol_id = Some(old_id.clone());
            }
        }
    }

    // A newer resolver may coalesce a prototype/definition pair which the
    // old index exposed as two symbols. Retain a declaration anchor for every
    // old ID so pending reviews and accepted aliases remain addressable. New
    // compiler admission links those anchors authoritatively on approval.
    let mut split_symbols = Vec::<SemanticSymbol>::new();
    for occurrence in document
        .occurrences
        .iter_mut()
        .filter(|occurrence| occurrence.role == OccurrenceRole::Declaration)
    {
        let key = (
            occurrence.range.start_byte,
            occurrence.range.end_byte,
            occurrence.name.clone(),
        );
        let Some(old_index) = by_loose_range
            .get(&key)
            .into_iter()
            .flatten()
            .copied()
            .find(|index| !used.contains(index))
        else {
            continue;
        };
        used.insert(old_index);
        let existing_symbol = &existing[old_index];
        if occurrence.symbol_id.as_deref() == Some(existing_symbol.symbol_id.as_str()) {
            continue;
        }
        let Some(template) = occurrence
            .symbol_id
            .as_ref()
            .and_then(|symbol_id| {
                document
                    .symbols
                    .iter()
                    .find(|symbol| &symbol.symbol_id == symbol_id)
            })
            .cloned()
        else {
            continue;
        };
        let mut split = template;
        split.symbol_id = existing_symbol.symbol_id.clone();
        split.node_id = occurrence.node_id.clone();
        split.name = occurrence.name.clone();
        split.range = occurrence.range.clone();
        split.locally_bound = false;
        occurrence.symbol_id = Some(split.symbol_id.clone());
        split_symbols.push(split);
    }
    document.symbols.extend(split_symbols);

    // Do not restore old syntax bindings here. Correct bindings already moved
    // from generated IDs to the preserved IDs above. Any remaining difference
    // is a deliberate result of the new resolver and must win. Authoritative
    // LSP/compiler bindings are persisted separately and reapplied after the
    // document transaction.
    for symbol in &mut document.symbols {
        symbol.locally_bound = document.occurrences.iter().any(|occurrence| {
            occurrence.role == OccurrenceRole::Reference
                && occurrence.symbol_id.as_deref() == Some(symbol.symbol_id.as_str())
        });
    }
    let mut seen_symbol_ids = BTreeMap::<String, (String, usize)>::new();
    for symbol in &document.symbols {
        if let Some((existing_name, existing_line)) = seen_symbol_ids.insert(
            symbol.symbol_id.clone(),
            (symbol.qualified_name.clone(), symbol.range.start_line),
        ) {
            bail!(
                "resolver upgrade produced duplicate symbol ID {} in {}: {} at line {} and {} at line {}",
                symbol.symbol_id,
                document.rel_path,
                existing_name,
                existing_line,
                symbol.qualified_name,
                symbol.range.start_line
            );
        }
    }
    Ok(())
}

pub fn replace_documents(
    conn: &mut Connection,
    documents: &[ParsedDocument],
    removed: &[String],
) -> Result<u64> {
    if documents.is_empty() && removed.is_empty() {
        return current_revision(conn);
    }
    let tx = conn
        .transaction()
        .context("begin semantic index transaction")?;
    let base_revision = current_revision(&tx)?;
    let next_revision = base_revision
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("semantic workspace revision overflow"))?;

    for rel_path in removed {
        tx.execute(
            "delete from semantic_documents where rel_path = ?1",
            params![rel_path],
        )
        .with_context(|| format!("remove semantic document {rel_path}"))?;
    }
    for document in documents {
        replace_document(&tx, document, next_revision)?;
    }
    reapply_compiler_bindings(&tx)?;
    tx.execute(
        "delete from semantic_aliases where symbol_id not in (select symbol_id from semantic_symbols)",
        [],
    )
    .context("remove aliases for symbols deleted or renamed by reindex")?;
    let updated = tx
        .execute(
            "update semantic_workspace set revision = ?2 where singleton = 1 and revision = ?1",
            params![base_revision as i64, next_revision as i64],
        )
        .context("advance semantic workspace revision")?;
    if updated != 1 {
        bail!("semantic workspace revision changed during index commit");
    }
    tx.commit().context("commit semantic index transaction")?;
    Ok(next_revision)
}

fn reapply_compiler_bindings(tx: &Transaction<'_>) -> Result<()> {
    tx.execute(
        r#"
        update semantic_occurrences as occurrence
        set symbol_id = (
              select binding.canonical_symbol_id
              from semantic_compiler_bindings binding
              join semantic_documents document on document.rel_path = binding.rel_path
              where binding.rel_path = occurrence.rel_path
                and binding.start_byte = occurrence.start_byte
                and binding.end_byte = occurrence.end_byte
                and binding.name = occurrence.name
                and binding.content_hash = document.content_hash
                and exists(select 1 from semantic_symbols canonical
                           where canonical.symbol_id = binding.canonical_symbol_id)
              limit 1
            ),
            role = 'reference'
        where occurrence.role != 'declaration'
          and exists(
              select 1 from semantic_compiler_bindings binding
              join semantic_documents document on document.rel_path = binding.rel_path
              where binding.rel_path = occurrence.rel_path
                and binding.start_byte = occurrence.start_byte
                and binding.end_byte = occurrence.end_byte
                and binding.name = occurrence.name
                and binding.content_hash = document.content_hash
                and exists(select 1 from semantic_symbols canonical
                           where canonical.symbol_id = binding.canonical_symbol_id)
          )
        "#,
        [],
    )
    .context("reapply compiler-backed semantic bindings")?;
    tx.execute(
        r#"
        delete from semantic_compiler_links
        where canonical_symbol_id not in (select symbol_id from semantic_symbols)
           or linked_symbol_id not in (select symbol_id from semantic_symbols)
        "#,
        [],
    )
    .context("remove stale compiler symbol links")?;
    tx.execute(
        r#"
        update semantic_aliases
        set status = 'stale'
        where source = 'proposal-v2' and status = 'accepted'
          and symbol_id in (
            select resolution.canonical_symbol_id
            from semantic_compiler_resolutions resolution
            where resolution.canonical_symbol_id not in (select symbol_id from semantic_symbols)
               or exists(
                  select 1 from semantic_compiler_bindings binding
                  left join semantic_documents document on document.rel_path = binding.rel_path
                  where binding.canonical_symbol_id = resolution.canonical_symbol_id
                    and (document.rel_path is null or document.content_hash != binding.content_hash)
               )
            union
            select link.linked_symbol_id
            from semantic_compiler_links link
            join semantic_compiler_resolutions resolution
              on resolution.canonical_symbol_id = link.canonical_symbol_id
            where resolution.canonical_symbol_id not in (select symbol_id from semantic_symbols)
               or exists(
                  select 1 from semantic_compiler_bindings binding
                  left join semantic_documents document on document.rel_path = binding.rel_path
                  where binding.canonical_symbol_id = resolution.canonical_symbol_id
                    and (document.rel_path is null or document.content_hash != binding.content_hash)
               )
          )
        "#,
        [],
    )
    .context("mark aliases stale after compiler-binding drift")?;
    tx.execute(
        r#"
        delete from semantic_compiler_resolutions
        where canonical_symbol_id not in (select symbol_id from semantic_symbols)
           or exists(
              select 1 from semantic_compiler_bindings binding
              left join semantic_documents document on document.rel_path = binding.rel_path
              where binding.canonical_symbol_id = semantic_compiler_resolutions.canonical_symbol_id
                and (document.rel_path is null or document.content_hash != binding.content_hash)
           )
        "#,
        [],
    )
    .context("invalidate stale compiler resolutions")?;
    // A newly-added document can introduce a reference without changing any
    // previously-bound file hash. Conservatively stale the matching compiler
    // alias group so the LSP closure is refreshed before projection resumes.
    tx.execute(
        r#"
        update semantic_aliases as alias
        set status = 'stale'
        where alias.source = 'proposal-v2' and alias.status = 'accepted'
          and exists(
            select 1 from semantic_compiler_links link
            where link.linked_symbol_id = alias.symbol_id
          )
          and exists(
            select 1 from semantic_occurrences occurrence
            where occurrence.name = alias.original_name
              and occurrence.role in ('unresolved', 'external')
          )
        "#,
        [],
    )
    .context("stale compiler aliases with newly unresolved spellings")?;
    tx.execute(
        r#"
        update semantic_aliases as alias
        set status = 'stale'
        where alias.source = 'proposal-v2' and alias.status = 'accepted'
          and exists(
            select 1 from semantic_compiler_links member
            join semantic_compiler_links stale_member
              on stale_member.canonical_symbol_id = member.canonical_symbol_id
            join semantic_aliases stale_alias
              on stale_alias.symbol_id = stale_member.linked_symbol_id
            where member.linked_symbol_id = alias.symbol_id
              and stale_alias.status = 'stale'
          )
        "#,
        [],
    )
    .context("stale all compiler-linked alias anchors")?;
    tx.execute(
        r#"
        delete from semantic_compiler_resolutions
        where exists(
          select 1 from semantic_compiler_links link
          join semantic_aliases alias on alias.symbol_id = link.linked_symbol_id
          where link.canonical_symbol_id = semantic_compiler_resolutions.canonical_symbol_id
            and alias.status = 'stale'
        )
        "#,
        [],
    )
    .context("invalidate compiler resolutions for stale alias groups")?;
    Ok(())
}

fn replace_document(tx: &Transaction<'_>, document: &ParsedDocument, revision: u64) -> Result<()> {
    tx.execute(
        "delete from semantic_documents where rel_path = ?1",
        params![document.rel_path],
    )
    .with_context(|| format!("clear semantic document {}", document.rel_path))?;
    tx.execute(
        r#"
        insert into semantic_documents(
          rel_path, language, content_hash, origin, capabilities_json, parse_errors, indexed_revision
        ) values(?1, ?2, ?3, ?4, ?5, ?6, ?7)
        "#,
        params![
            document.rel_path,
            enum_text(document.language)?,
            document.content_hash,
            enum_text(document.origin)?,
            serde_json::to_string(&document.capabilities).context("serialize capabilities")?,
            document.parse_errors as i64,
            revision as i64,
        ],
    )
    .with_context(|| format!("insert semantic document {}", document.rel_path))?;

    for node in &document.nodes {
        tx.execute(
            r#"
            insert into semantic_nodes(
              node_id, rel_path, parent_node_id, kind, start_byte, end_byte,
              start_line, start_column, end_line, end_column
            ) values(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            "#,
            params![
                node.node_id,
                document.rel_path,
                node.parent_node_id,
                node.kind,
                node.range.start_byte as i64,
                node.range.end_byte as i64,
                node.range.start_line as i64,
                node.range.start_column as i64,
                node.range.end_line as i64,
                node.range.end_column as i64,
            ],
        )
        .with_context(|| format!("insert semantic node {}", node.node_id))?;
    }
    for symbol in &document.symbols {
        insert_symbol(tx, &document.rel_path, symbol)?;
    }
    for occurrence in &document.occurrences {
        insert_occurrence(tx, &document.rel_path, occurrence)?;
    }
    Ok(())
}

fn insert_symbol(tx: &Transaction<'_>, rel_path: &str, symbol: &SemanticSymbol) -> Result<()> {
    tx.execute(
        r#"
        insert into semantic_symbols(
          symbol_id, rel_path, node_id, name, kind, qualified_name,
          scope_node_id, origin, locally_bound
        ) values(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
        "#,
        params![
            symbol.symbol_id,
            rel_path,
            symbol.node_id,
            symbol.name,
            symbol.kind,
            symbol.qualified_name,
            symbol.scope_node_id,
            enum_text(symbol.origin)?,
            i64::from(symbol.locally_bound),
        ],
    )
    .with_context(|| format!("insert semantic symbol {}", symbol.symbol_id))?;
    Ok(())
}

fn insert_occurrence(
    tx: &Transaction<'_>,
    rel_path: &str,
    occurrence: &SemanticOccurrence,
) -> Result<()> {
    tx.execute(
        r#"
        insert into semantic_occurrences(
          occurrence_id, rel_path, node_id, symbol_id, name, role,
          start_byte, end_byte, start_line, start_column, end_line, end_column
        ) values(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
        "#,
        params![
            occurrence.occurrence_id,
            rel_path,
            occurrence.node_id,
            occurrence.symbol_id,
            occurrence.name,
            enum_text(occurrence.role)?,
            occurrence.range.start_byte as i64,
            occurrence.range.end_byte as i64,
            occurrence.range.start_line as i64,
            occurrence.range.start_column as i64,
            occurrence.range.end_line as i64,
            occurrence.range.end_column as i64,
        ],
    )
    .with_context(|| format!("insert semantic occurrence {}", occurrence.occurrence_id))?;
    Ok(())
}

pub fn snapshot(conn: &Connection) -> Result<WorkspaceSnapshot> {
    Ok(WorkspaceSnapshot {
        revision: current_revision(conn)?,
        documents: count(conn, "semantic_documents", None)?,
        symbols: count(conn, "semantic_symbols", None)?,
        occurrences: count(conn, "semantic_occurrences", None)?,
        unresolved_occurrences: count(
            conn,
            "semantic_occurrences",
            Some("where role = 'unresolved'"),
        )?,
        external_occurrences: count(
            conn,
            "semantic_occurrences",
            Some("where role = 'external'"),
        )?,
        aliases: count(conn, "semantic_aliases", None)?,
    })
}

fn count(conn: &Connection, table: &str, suffix: Option<&str>) -> Result<usize> {
    let sql = format!(
        "select count(*) from {table} {}",
        suffix.unwrap_or_default()
    );
    let value = conn
        .query_row(&sql, [], |row| row.get::<_, i64>(0))
        .with_context(|| format!("count {table}"))?;
    usize::try_from(value).with_context(|| format!("negative count for {table}"))
}

pub fn load_symbol(conn: &Connection, symbol_id: &str) -> Result<Option<SemanticSymbol>> {
    conn.query_row(
        r#"
        select s.symbol_id, s.node_id, s.name, s.kind, s.qualified_name, s.scope_node_id,
               s.origin, s.locally_bound,
               n.start_byte, n.end_byte, n.start_line, n.start_column, n.end_line, n.end_column
        from semantic_symbols s
        join semantic_nodes n on n.node_id = s.node_id
        where s.symbol_id = ?1
        "#,
        params![symbol_id],
        |row| {
            Ok(SemanticSymbol {
                symbol_id: row.get(0)?,
                node_id: row.get(1)?,
                name: row.get(2)?,
                kind: row.get(3)?,
                qualified_name: row.get(4)?,
                scope_node_id: row.get(5)?,
                origin: parse_origin(&row.get::<_, String>(6)?),
                locally_bound: row.get::<_, i64>(7)? != 0,
                range: range_from_row(row, 8)?,
            })
        },
    )
    .optional()
    .context("load semantic symbol")
}

pub fn load_symbol_with_path(
    conn: &Connection,
    symbol_id: &str,
) -> Result<Option<(String, SemanticSymbol)>> {
    let rel_path = conn
        .query_row(
            "select rel_path from semantic_symbols where symbol_id = ?1",
            params![symbol_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .context("load semantic symbol path")?;
    match rel_path {
        Some(path) => Ok(load_symbol(conn, symbol_id)?.map(|symbol| (path, symbol))),
        None => Ok(None),
    }
}

pub fn load_document(conn: &Connection, rel_path: &str) -> Result<Option<StoredDocument>> {
    conn.query_row(
        r#"
        select rel_path, language, content_hash, origin, capabilities_json, parse_errors
        from semantic_documents where rel_path = ?1
        "#,
        params![rel_path],
        |row| {
            let capabilities_json: String = row.get(4)?;
            let capabilities = serde_json::from_str(&capabilities_json).map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    capabilities_json.len(),
                    rusqlite::types::Type::Text,
                    Box::new(err),
                )
            })?;
            Ok(StoredDocument {
                rel_path: row.get(0)?,
                language: parse_language(&row.get::<_, String>(1)?),
                content_hash: row.get(2)?,
                origin: parse_origin(&row.get::<_, String>(3)?),
                capabilities,
                parse_errors: row.get::<_, i64>(5)? as usize,
            })
        },
    )
    .optional()
    .context("load semantic document")
}

pub fn load_node(conn: &Connection, node_id: &str) -> Result<Option<StoredNode>> {
    conn.query_row(
        r#"
        select n.node_id, n.rel_path, n.kind,
               n.start_byte, n.end_byte, n.start_line, n.start_column, n.end_line, n.end_column,
               exists(select 1 from semantic_symbols s where s.node_id = n.node_id)
        from semantic_nodes n where n.node_id = ?1
        "#,
        params![node_id],
        |row| {
            Ok(StoredNode {
                node_id: row.get(0)?,
                rel_path: row.get(1)?,
                kind: row.get(2)?,
                range: range_from_row(row, 3)?,
                is_declaration: row.get::<_, i64>(9)? != 0,
            })
        },
    )
    .optional()
    .context("load semantic node")
}

pub fn range_contains_declaration(
    conn: &Connection,
    rel_path: &str,
    start_byte: usize,
    end_byte: usize,
) -> Result<bool> {
    let count = conn
        .query_row(
            r#"
            select count(*)
            from semantic_symbols s
            join semantic_nodes n on n.node_id = s.node_id
            where s.rel_path = ?1 and n.start_byte >= ?2 and n.end_byte <= ?3
            "#,
            params![rel_path, start_byte as i64, end_byte as i64],
            |row| row.get::<_, i64>(0),
        )
        .context("check declarations inside semantic node")?;
    Ok(count != 0)
}

pub fn insert_preview_transaction(
    conn: &Connection,
    transaction_id: &str,
    base_revision: u64,
    intents_json: &str,
    preview_json: &str,
    created_at: &str,
) -> Result<()> {
    conn.execute(
        r#"
        insert into semantic_transactions(
          transaction_id, base_revision, status, intents_json, preview_json,
          committed_revision, created_at, updated_at
        ) values(?1, ?2, 'previewed', ?3, ?4, null, ?5, ?5)
        "#,
        params![
            transaction_id,
            base_revision as i64,
            intents_json,
            preview_json,
            created_at,
        ],
    )
    .context("insert semantic preview transaction")?;
    Ok(())
}

pub fn load_transaction(
    conn: &Connection,
    transaction_id: &str,
) -> Result<Option<StoredTransaction>> {
    conn.query_row(
        r#"
        select transaction_id, base_revision, status, intents_json, preview_json,
               committed_revision
        from semantic_transactions where transaction_id = ?1
        "#,
        params![transaction_id],
        |row| {
            Ok(StoredTransaction {
                transaction_id: row.get(0)?,
                base_revision: row.get::<_, i64>(1)? as u64,
                status: row.get(2)?,
                intents_json: row.get(3)?,
                preview_json: row.get(4)?,
                committed_revision: row.get::<_, Option<i64>>(5)?.map(|value| value as u64),
            })
        },
    )
    .optional()
    .context("load semantic transaction")
}

pub fn mark_transaction_committed(
    conn: &Connection,
    transaction_id: &str,
    committed_revision: u64,
    updated_at: &str,
) -> Result<()> {
    let updated = conn
        .execute(
            r#"
            update semantic_transactions
            set status = 'committed', committed_revision = ?2, updated_at = ?3
            where transaction_id = ?1 and status = 'previewed'
            "#,
            params![transaction_id, committed_revision as i64, updated_at],
        )
        .context("mark semantic transaction committed")?;
    if updated != 1 {
        bail!("semantic transaction is not in previewed state");
    }
    Ok(())
}

pub fn compiler_equivalent_symbol_ids(
    conn: &Connection,
    symbol_id: &str,
) -> Result<BTreeSet<String>> {
    let mut members = BTreeSet::from([symbol_id.to_string()]);
    let mut frontier = vec![symbol_id.to_string()];
    let mut statement = conn
        .prepare(
            r#"
            select canonical_symbol_id, linked_symbol_id
            from semantic_compiler_links
            where canonical_symbol_id = ?1 or linked_symbol_id = ?1
            "#,
        )
        .context("prepare compiler-equivalence traversal")?;
    while let Some(current) = frontier.pop() {
        let edges = statement
            .query_map(params![current], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .context("query compiler-equivalence traversal")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("collect compiler-equivalence traversal")?;
        for (canonical, linked) in edges {
            for member in [canonical, linked] {
                if members.insert(member.clone()) {
                    frontier.push(member);
                }
            }
        }
    }
    Ok(members)
}

/// Validate workspace injectivity before compiler-reference admission mutates
/// the semantic graph. `equivalent_symbol_ids` are syntax-proven supplemental
/// declaration/definition anchors that admission will persist in the same
/// compiler group.
pub fn validate_symbol_alias_candidate(
    conn: &Connection,
    symbol_id: &str,
    replacement: &str,
    equivalent_symbol_ids: &BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    let symbol = load_symbol(conn, symbol_id)?
        .ok_or_else(|| anyhow::anyhow!("proposal target symbol_id does not exist"))?;
    if symbol.origin != SourceOrigin::Owned {
        bail!("proposal target is not owned source code");
    }
    let mut linked_set = compiler_equivalent_symbol_ids(conn, symbol_id)?;
    for equivalent_id in equivalent_symbol_ids {
        let equivalent = load_symbol(conn, equivalent_id)?.ok_or_else(|| {
            anyhow::anyhow!("supplemental compiler-equivalent symbol disappeared")
        })?;
        if equivalent.origin != SourceOrigin::Owned
            || equivalent.name != symbol.name
            || equivalent.kind != symbol.kind
            || equivalent.qualified_name != symbol.qualified_name
        {
            bail!(
                "supplemental compiler-equivalent symbol {} does not match {}",
                equivalent.qualified_name,
                symbol.qualified_name
            );
        }
        linked_set.insert(equivalent_id.clone());
    }

    let mut linked_originals = BTreeSet::new();
    for linked_symbol_id in &linked_set {
        let linked_symbol = load_symbol(conn, linked_symbol_id)?
            .ok_or_else(|| anyhow::anyhow!("compiler-linked proposal target disappeared"))?;
        linked_originals.insert(crate::sanitize::normalize_term(&linked_symbol.name));
        let conflicting = conn
            .query_row(
                r#"
                select sanitized_name from semantic_aliases
                where symbol_id = ?1 and status = 'accepted' and sanitized_name != ?2
                "#,
                params![linked_symbol_id, replacement],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("check compiler-linked alias conflict")?;
        if let Some(conflicting) = conflicting {
            bail!(
                "compiler-linked symbol {} already has incompatible alias {:?}",
                linked_symbol.qualified_name,
                conflicting
            );
        }
    }
    let normalized_replacement = crate::sanitize::normalize_term(replacement);
    {
        let mut statement = conn
            .prepare(
                r#"
                select distinct original_text, sanitized_text
                from replacements
                where policy_source != 'semantic-alias'
                "#,
            )
            .context("prepare lexical/semantic alias injectivity query")?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .context("query lexical/semantic alias injectivity")?;
        for row in rows {
            let (lexical_original, lexical_alias) =
                row.context("read lexical/semantic alias owner")?;
            if crate::sanitize::normalize_term(&lexical_alias) == normalized_replacement
                && !linked_originals.contains(&crate::sanitize::normalize_term(&lexical_original))
            {
                bail!(
                    "semantic alias {replacement:?} is already the lexical alias of {lexical_original:?}; aliases must be workspace-injective"
                );
            }
        }
    }
    {
        let mut statement = conn
            .prepare(
                r#"
                select symbol_id, original_name, sanitized_name from semantic_aliases
                where status in ('accepted', 'stale')
                "#,
            )
            .context("prepare semantic alias injectivity query")?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .context("query semantic alias injectivity")?;
        for row in rows {
            let (existing_symbol, existing_original, existing_alias) =
                row.context("read semantic alias owner")?;
            if crate::sanitize::normalize_term(&existing_alias) == normalized_replacement
                && !linked_set.contains(&existing_symbol)
                && !linked_originals.contains(&crate::sanitize::normalize_term(&existing_original))
            {
                bail!(
                    "semantic alias {replacement:?} maps to a different original on another symbol; aliases must be workspace-injective by original spelling"
                );
            }
        }
    }
    {
        let mut statement = conn
            .prepare("select symbol_id, name from semantic_symbols")
            .context("prepare semantic natural-name collision query")?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .context("query semantic natural-name collisions")?;
        for row in rows {
            let (existing_symbol, existing_name) = row.context("read semantic symbol name")?;
            if crate::sanitize::normalize_term(&existing_name) == normalized_replacement
                && !linked_set.contains(&existing_symbol)
            {
                bail!(
                    "semantic alias {replacement:?} collides with existing symbol {existing_name:?}"
                );
            }
        }
    }
    Ok(linked_set)
}

#[derive(Debug, Clone)]
pub struct SymbolAliasAcceptance {
    pub symbol_id: String,
    pub replacement: String,
    pub category: String,
    pub confidence: f64,
    pub reason: Option<String>,
}

pub fn accept_symbol_alias(
    conn: &mut Connection,
    symbol_id: &str,
    replacement: &str,
    category: &str,
    confidence: f64,
    reason: Option<&str>,
) -> Result<u64> {
    accept_symbol_aliases(
        conn,
        &[SymbolAliasAcceptance {
            symbol_id: symbol_id.to_string(),
            replacement: replacement.to_string(),
            category: category.to_string(),
            confidence,
            reason: reason.map(str::to_string),
        }],
    )
}

/// Atomically accept a validated set of aliases with one workspace revision.
/// The collision indexes are loaded once, making bulk approval linear in the
/// semantic workspace instead of rescanning every symbol for every proposal.
pub fn accept_symbol_aliases(
    conn: &mut Connection,
    aliases: &[SymbolAliasAcceptance],
) -> Result<u64> {
    if aliases.is_empty() {
        return current_revision(conn);
    }

    #[derive(Clone)]
    struct SymbolInfo {
        name: String,
        kind: String,
        qualified_name: String,
        origin: SourceOrigin,
    }
    let mut symbols = BTreeMap::<String, SymbolInfo>::new();
    let mut natural_owners = BTreeMap::<String, BTreeSet<String>>::new();
    {
        let mut statement = conn
            .prepare("select symbol_id, name, kind, qualified_name, origin from semantic_symbols")
            .context("prepare bulk semantic alias symbols")?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .context("query bulk semantic alias symbols")?;
        for row in rows {
            let (symbol_id, name, kind, qualified_name, origin) =
                row.context("read bulk semantic alias symbol")?;
            natural_owners
                .entry(crate::sanitize::normalize_term(&name))
                .or_default()
                .insert(symbol_id.clone());
            symbols.insert(
                symbol_id,
                SymbolInfo {
                    name,
                    kind,
                    qualified_name,
                    origin: parse_origin(&origin),
                },
            );
        }
    }
    let mut graph = BTreeMap::<String, BTreeSet<String>>::new();
    {
        let mut statement = conn
            .prepare("select canonical_symbol_id, linked_symbol_id from semantic_compiler_links")
            .context("prepare bulk compiler alias graph")?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .context("query bulk compiler alias graph")?;
        for row in rows {
            let (left, right) = row.context("read bulk compiler alias edge")?;
            graph.entry(left.clone()).or_default().insert(right.clone());
            graph.entry(right).or_default().insert(left);
        }
    }
    let mut lexical_owners = BTreeMap::<String, BTreeSet<String>>::new();
    {
        let mut statement = conn
            .prepare(
                "select distinct original_text, sanitized_text from replacements where policy_source != 'semantic-alias'",
            )
            .context("prepare bulk lexical alias owners")?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .context("query bulk lexical alias owners")?;
        for row in rows {
            let (original, alias) = row.context("read bulk lexical alias owner")?;
            lexical_owners
                .entry(crate::sanitize::normalize_term(&alias))
                .or_default()
                .insert(crate::sanitize::normalize_term(&original));
        }
    }
    let mut aliases_by_symbol = BTreeMap::<String, (String, String)>::new();
    let mut semantic_owners = BTreeMap::<String, BTreeMap<String, String>>::new();
    {
        let mut statement = conn
            .prepare(
                "select symbol_id, original_name, sanitized_name, status from semantic_aliases",
            )
            .context("prepare bulk semantic alias owners")?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .context("query bulk semantic alias owners")?;
        for row in rows {
            let (symbol_id, original, alias, status) =
                row.context("read bulk semantic alias owner")?;
            if matches!(status.as_str(), "accepted" | "stale") {
                semantic_owners
                    .entry(crate::sanitize::normalize_term(&alias))
                    .or_default()
                    .insert(
                        symbol_id.clone(),
                        crate::sanitize::normalize_term(&original),
                    );
            }
            aliases_by_symbol.insert(symbol_id, (alias, status));
        }
    }

    let component = |start: &str| {
        let mut members = BTreeSet::new();
        let mut frontier = vec![start.to_string()];
        while let Some(current) = frontier.pop() {
            if !members.insert(current.clone()) {
                continue;
            }
            frontier.extend(graph.get(&current).into_iter().flatten().cloned());
        }
        members
    };
    let mut validated = Vec::<(&SymbolAliasAcceptance, BTreeSet<String>)>::new();
    for alias in aliases {
        if !symbol_projection_is_complete(conn, &alias.symbol_id)? {
            bail!(
                "proposal target has unresolved references; semantic projection would be incomplete"
            );
        }
        let target = symbols
            .get(&alias.symbol_id)
            .ok_or_else(|| anyhow::anyhow!("proposal target symbol_id does not exist"))?;
        if target.origin != SourceOrigin::Owned {
            bail!("proposal target is not owned source code");
        }
        let linked = component(&alias.symbol_id);
        let mut linked_originals = BTreeSet::new();
        for linked_symbol_id in &linked {
            let linked_symbol = symbols
                .get(linked_symbol_id)
                .ok_or_else(|| anyhow::anyhow!("compiler-linked proposal target disappeared"))?;
            if linked_symbol.origin != SourceOrigin::Owned
                || linked_symbol.name != target.name
                || linked_symbol.kind != target.kind
                || linked_symbol.qualified_name != target.qualified_name
            {
                bail!(
                    "compiler-linked symbol {} does not match {}",
                    linked_symbol.qualified_name,
                    target.qualified_name
                );
            }
            linked_originals.insert(crate::sanitize::normalize_term(&linked_symbol.name));
            if aliases_by_symbol
                .get(linked_symbol_id)
                .is_some_and(|(existing, status)| {
                    status == "accepted" && existing != &alias.replacement
                })
            {
                let conflicting = &aliases_by_symbol[linked_symbol_id].0;
                bail!(
                    "compiler-linked symbol {} already has incompatible alias {:?}",
                    linked_symbol.qualified_name,
                    conflicting
                );
            }
        }
        let normalized = crate::sanitize::normalize_term(&alias.replacement);
        if lexical_owners.get(&normalized).is_some_and(|owners| {
            owners
                .iter()
                .any(|original| !linked_originals.contains(original))
        }) {
            bail!(
                "semantic alias {:?} is already a lexical alias of a different term; aliases must be workspace-injective",
                alias.replacement
            );
        }
        if semantic_owners.get(&normalized).is_some_and(|owners| {
            owners.iter().any(|(owner, original)| {
                !linked.contains(owner) && !linked_originals.contains(original)
            })
        }) {
            bail!(
                "semantic alias {:?} maps to a different original on another symbol; aliases must be workspace-injective by original spelling",
                alias.replacement
            );
        }
        if natural_owners
            .get(&normalized)
            .is_some_and(|owners| owners.iter().any(|owner| !linked.contains(owner)))
        {
            bail!(
                "semantic alias {:?} collides with an existing symbol name",
                alias.replacement
            );
        }
        for linked_symbol_id in &linked {
            let linked_original = crate::sanitize::normalize_term(
                &symbols
                    .get(linked_symbol_id)
                    .expect("validated compiler-linked symbol exists")
                    .name,
            );
            aliases_by_symbol.insert(
                linked_symbol_id.clone(),
                (alias.replacement.clone(), "accepted".to_string()),
            );
            semantic_owners
                .entry(normalized.clone())
                .or_default()
                .insert(linked_symbol_id.clone(), linked_original);
        }
        validated.push((alias, linked));
    }

    let tx = conn
        .transaction()
        .context("begin bulk semantic alias approval transaction")?;
    let base_revision = current_revision(&tx)?;
    let next_revision = base_revision
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("semantic workspace revision overflow"))?;
    for (alias, linked) in validated {
        for linked_symbol_id in linked {
            let linked_symbol = symbols
                .get(&linked_symbol_id)
                .expect("validated compiler-linked symbol exists");
            tx.execute(
                r#"
            insert into semantic_aliases(
              symbol_id, original_name, sanitized_name, category, confidence,
              reason, status, source, created_revision
            ) values(?1, ?2, ?3, ?4, ?5, ?6, 'accepted', 'proposal-v2', ?7)
            on conflict(symbol_id) do update set
              original_name = excluded.original_name,
              sanitized_name = excluded.sanitized_name,
              category = excluded.category,
              confidence = excluded.confidence,
              reason = excluded.reason,
              status = excluded.status,
              source = excluded.source,
              created_revision = excluded.created_revision
            "#,
                params![
                    linked_symbol_id,
                    linked_symbol.name,
                    alias.replacement,
                    alias.category,
                    alias.confidence,
                    alias.reason,
                    next_revision as i64,
                ],
            )
            .context("upsert accepted compiler-linked semantic alias")?;
        }
    }
    let updated = tx
        .execute(
            "update semantic_workspace set revision = ?2 where singleton = 1 and revision = ?1",
            params![base_revision as i64, next_revision as i64],
        )
        .context("advance semantic alias approval revision")?;
    if updated != 1 {
        bail!("semantic workspace revision changed during alias approval");
    }
    tx.commit().context("commit semantic alias approval")?;
    Ok(next_revision)
}

pub fn symbol_projection_is_complete(conn: &Connection, symbol_id: &str) -> Result<bool> {
    let compiler_resolved = active_compiler_canonical(conn, symbol_id)?.is_some();
    if compiler_resolved {
        return Ok(true);
    }
    let compiler_history = conn
        .query_row(
            r#"
            select exists(
              select 1 from semantic_compiler_bindings where canonical_symbol_id = ?1
              union all
              select 1 from semantic_compiler_links
                where canonical_symbol_id = ?1 or linked_symbol_id = ?1
            )
            "#,
            params![symbol_id],
            |row| row.get::<_, i64>(0),
        )
        .context("check compiler binding history")?
        != 0;
    if compiler_history {
        return Ok(false);
    }
    if !symbol_is_lexically_closed(conn, symbol_id)? {
        // File-local syntax cannot prove that a module/class/global symbol has
        // no references elsewhere. Such symbols require an admitted LSP
        // reference closure before they may be projected.
        return Ok(false);
    }
    lexical_symbol_references_complete(conn, symbol_id)
}

pub(crate) fn active_compiler_canonical(
    conn: &Connection,
    symbol_id: &str,
) -> Result<Option<String>> {
    conn.query_row(
        r#"
            select resolution.canonical_symbol_id
            from semantic_compiler_resolutions resolution
            where (
                  resolution.canonical_symbol_id = ?1
                  or exists(
                    select 1 from semantic_compiler_links member
                    where member.canonical_symbol_id = resolution.canonical_symbol_id
                      and member.linked_symbol_id = ?1
                  )
                )
              and not exists(
                select 1 from semantic_compiler_bindings binding
                left join semantic_documents document on document.rel_path = binding.rel_path
                where binding.canonical_symbol_id = resolution.canonical_symbol_id
                  and (document.rel_path is null
                       or document.content_hash != binding.content_hash)
              )
            order by (resolution.canonical_symbol_id = ?1) desc,
                     resolution.canonical_symbol_id
            limit 1
            "#,
        params![symbol_id],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .context("resolve active compiler canonical symbol")
}

/// One-time repair for aliases accepted by releases that did not enforce the
/// current workspace-wide projection and injectivity invariants. Unsafe
/// `proposal-v2` rows are removed (never guessed or silently remapped), their
/// proposal decisions become retryable, and the caller re-renders the old
/// semantic spans out of the mirror. Non-proposal rows are policy-derived and
/// therefore fail loudly instead of being mutated behind the config's back.
pub(crate) fn quarantine_legacy_invalid_accepted_aliases(
    conn: &mut Connection,
) -> Result<Vec<QuarantinedSemanticAlias>> {
    const MIGRATION_KEY: &str = "semantic-alias-safety-v2";
    conn.execute_batch(
        r#"
        create table if not exists semantic_migrations(
          migration_key text primary key,
          applied_at text not null,
          affected_rows integer not null
        );
        "#,
    )
    .context("ensure semantic migration ledger")?;
    let already_applied = conn
        .query_row(
            "select exists(select 1 from semantic_migrations where migration_key = ?1)",
            [MIGRATION_KEY],
            |row| row.get::<_, i64>(0),
        )
        .context("check semantic alias safety migration")?
        != 0;
    if already_applied {
        return Ok(Vec::new());
    }

    #[derive(Clone)]
    struct Candidate {
        symbol_id: String,
        original: String,
        alias: String,
        source: String,
        confidence: Option<f64>,
        created_revision: u64,
    }

    let aliases = {
        let mut statement = conn
            .prepare(
                r#"
                select symbol_id, original_name, sanitized_name, source,
                       confidence, created_revision
                from semantic_aliases
                where status = 'accepted'
                order by symbol_id
                "#,
            )
            .context("prepare legacy semantic alias safety scan")?;
        statement
            .query_map([], |row| {
                Ok(Candidate {
                    symbol_id: row.get(0)?,
                    original: row.get(1)?,
                    alias: row.get(2)?,
                    source: row.get(3)?,
                    confidence: row.get(4)?,
                    created_revision: row.get::<_, i64>(5)? as u64,
                })
            })
            .context("query legacy semantic aliases")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("collect legacy semantic aliases")?
    };

    let mut symbols = BTreeMap::<String, (String, SourceOrigin)>::new();
    let mut natural_owners = BTreeMap::<String, BTreeSet<String>>::new();
    {
        let mut statement = conn
            .prepare("select symbol_id, name, origin from semantic_symbols")
            .context("prepare legacy semantic alias symbols")?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .context("query legacy semantic alias symbols")?;
        for row in rows {
            let (symbol_id, name, origin) = row.context("read legacy semantic alias symbol")?;
            natural_owners
                .entry(crate::sanitize::normalize_term(&name))
                .or_default()
                .insert(symbol_id.clone());
            symbols.insert(symbol_id, (name, parse_origin(&origin)));
        }
    }

    let mut graph = BTreeMap::<String, BTreeSet<String>>::new();
    {
        let mut statement = conn
            .prepare("select canonical_symbol_id, linked_symbol_id from semantic_compiler_links")
            .context("prepare legacy semantic alias graph")?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .context("query legacy semantic alias graph")?;
        for row in rows {
            let (left, right) = row.context("read legacy semantic alias graph edge")?;
            graph.entry(left.clone()).or_default().insert(right.clone());
            graph.entry(right).or_default().insert(left);
        }
    }
    let component = |start: &str| {
        let mut members = BTreeSet::new();
        let mut frontier = vec![start.to_string()];
        while let Some(current) = frontier.pop() {
            if !members.insert(current.clone()) {
                continue;
            }
            frontier.extend(graph.get(&current).into_iter().flatten().cloned());
        }
        members
    };

    let mut lexical_owners = BTreeMap::<String, BTreeSet<String>>::new();
    {
        let mut statement = conn
            .prepare(
                r#"
                select distinct original_text, sanitized_text
                from replacements where policy_source != 'semantic-alias'
                "#,
            )
            .context("prepare legacy lexical alias owners")?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .context("query legacy lexical alias owners")?;
        for row in rows {
            let (original, alias) = row.context("read legacy lexical alias owner")?;
            lexical_owners
                .entry(crate::sanitize::normalize_term(&alias))
                .or_default()
                .insert(crate::sanitize::normalize_term(&original));
        }
    }

    let mut quarantined = BTreeMap::<String, QuarantinedSemanticAlias>::new();
    for alias in &aliases {
        if alias.source != "proposal-v2" {
            continue;
        }
        let members = component(&alias.symbol_id);
        let linked_originals = members
            .iter()
            .filter_map(|member| symbols.get(member))
            .map(|(name, _)| crate::sanitize::normalize_term(name))
            .collect::<BTreeSet<_>>();
        let normalized_alias = crate::sanitize::normalize_term(&alias.alias);
        let reason = match symbols.get(&alias.symbol_id) {
            None => Some("target symbol no longer exists".to_string()),
            Some((_, origin)) if *origin != SourceOrigin::Owned => {
                Some("target symbol is not repository-owned".to_string())
            }
            Some(_) if !symbol_projection_is_complete(conn, &alias.symbol_id)? => {
                Some("reference projection is incomplete under the current resolver".to_string())
            }
            Some(_)
                if lexical_owners.get(&normalized_alias).is_some_and(|owners| {
                    owners
                        .iter()
                        .any(|original| !linked_originals.contains(original))
                }) =>
            {
                Some("alias is owned by a different lexical term".to_string())
            }
            Some(_)
                if natural_owners
                    .get(&normalized_alias)
                    .is_some_and(|owners| owners.iter().any(|owner| !members.contains(owner))) =>
            {
                Some("alias collides with an existing source symbol name".to_string())
            }
            Some(_) => None,
        };
        let Some(reason) = reason else {
            continue;
        };
        quarantined.insert(
            alias.symbol_id.clone(),
            QuarantinedSemanticAlias {
                symbol_id: alias.symbol_id.clone(),
                original: alias.original.clone(),
                alias: alias.alias.clone(),
                reason,
            },
        );
    }

    let mut by_alias = BTreeMap::<String, Vec<&Candidate>>::new();
    for alias in &aliases {
        if !quarantined.contains_key(&alias.symbol_id) {
            by_alias
                .entry(crate::sanitize::normalize_term(&alias.alias))
                .or_default()
                .push(alias);
        }
    }
    for mappings in by_alias.values_mut() {
        let originals = mappings
            .iter()
            .map(|alias| crate::sanitize::normalize_term(&alias.original))
            .collect::<BTreeSet<_>>();
        if originals.len() < 2 {
            continue;
        }
        mappings.sort_by(|left, right| {
            let left_policy = usize::from(left.source == "proposal-v2");
            let right_policy = usize::from(right.source == "proposal-v2");
            left_policy
                .cmp(&right_policy)
                .then_with(|| {
                    right
                        .confidence
                        .unwrap_or(f64::NEG_INFINITY)
                        .total_cmp(&left.confidence.unwrap_or(f64::NEG_INFINITY))
                })
                .then_with(|| left.created_revision.cmp(&right.created_revision))
                .then_with(|| left.symbol_id.cmp(&right.symbol_id))
        });
        let winner = mappings[0];
        let winning_original = crate::sanitize::normalize_term(&winner.original);
        for alias in mappings.iter().skip(1) {
            if crate::sanitize::normalize_term(&alias.original) == winning_original {
                continue;
            }
            if alias.source != "proposal-v2" {
                bail!(
                    "policy-derived semantic aliases reuse {:?} for incompatible originals {:?} and {:?}; fix the sanitizer config and reindex",
                    alias.alias,
                    winner.original,
                    alias.original
                );
            }
            quarantined.insert(
                alias.symbol_id.clone(),
                QuarantinedSemanticAlias {
                    symbol_id: alias.symbol_id.clone(),
                    original: alias.original.clone(),
                    alias: alias.alias.clone(),
                    reason: format!(
                        "alias is already owned by the higher-priority mapping {:?} -> {:?}",
                        winner.original, winner.alias
                    ),
                },
            );
        }
    }

    // Compiler evidence is admitted and invalidated as one component. If one
    // member's old decision is unsafe, removing the shared closure would make
    // every remaining member incomplete, so quarantine those propagated
    // aliases in the same atomic repair as well.
    let unsafe_members = quarantined.keys().cloned().collect::<Vec<_>>();
    for symbol_id in unsafe_members {
        let members = component(&symbol_id);
        for alias in aliases
            .iter()
            .filter(|alias| members.contains(&alias.symbol_id))
        {
            if quarantined.contains_key(&alias.symbol_id) {
                continue;
            }
            if alias.source != "proposal-v2" {
                bail!(
                    "policy-derived semantic alias {:?} shares an invalid compiler component; fix the sanitizer config and reindex",
                    alias.alias
                );
            }
            quarantined.insert(
                alias.symbol_id.clone(),
                QuarantinedSemanticAlias {
                    symbol_id: alias.symbol_id.clone(),
                    original: alias.original.clone(),
                    alias: alias.alias.clone(),
                    reason: "compiler component contains another quarantined alias".to_string(),
                },
            );
        }
    }

    let tx = conn
        .transaction()
        .context("begin legacy semantic alias quarantine")?;
    let base_revision = current_revision(&tx)?;
    remove_compiler_components_for_symbols(&tx, quarantined.keys().cloned().collect::<Vec<_>>())?;
    let mut affected = 0usize;
    for alias in quarantined.values() {
        affected += tx
            .execute(
                r#"
                delete from semantic_aliases
                where symbol_id = ?1 and status = 'accepted' and source = 'proposal-v2'
                "#,
                [&alias.symbol_id],
            )
            .with_context(|| format!("quarantine semantic alias for {}", alias.symbol_id))?;
        tx.execute(
            "update semantic_proposals set status = 'stale' where symbol_id = ?1 and status = 'approved'",
            [&alias.symbol_id],
        )
        .context("make quarantined semantic proposal retryable")?;
    }
    if affected != 0 {
        let next_revision = base_revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("semantic workspace revision overflow"))?;
        let updated = tx
            .execute(
                "update semantic_workspace set revision = ?2 where singleton = 1 and revision = ?1",
                params![base_revision as i64, next_revision as i64],
            )
            .context("advance legacy semantic alias quarantine revision")?;
        if updated != 1 {
            bail!("semantic workspace changed during legacy alias quarantine");
        }
    }
    tx.execute(
        r#"
        insert into semantic_migrations(migration_key, applied_at, affected_rows)
        values(?1, ?2, ?3)
        "#,
        params![MIGRATION_KEY, Utc::now().to_rfc3339(), affected as i64],
    )
    .context("record semantic alias safety migration")?;
    tx.commit()
        .context("commit legacy semantic alias quarantine")?;

    let quarantined = quarantined.into_values().collect::<Vec<_>>();
    if !quarantined.is_empty() {
        log::warn!(
            "quarantined {} legacy semantic alias(es) that violate current safety invariants",
            quarantined.len()
        );
    }
    Ok(quarantined)
}

/// True only when syntax proves the symbol cannot be referenced from another
/// document. Global, namespace, class and module symbols require a compiler/
/// LSP reference closure even when the local parser bound same-file uses.
pub fn symbol_is_lexically_closed(conn: &Connection, symbol_id: &str) -> Result<bool> {
    Ok(symbol_lexical_closure_node(conn, symbol_id)?.is_some())
}

fn symbol_lexical_closure_node(conn: &Connection, symbol_id: &str) -> Result<Option<String>> {
    let symbol = conn
        .query_row(
            r#"
            select symbol.kind, symbol.scope_node_id, document.language
            from semantic_symbols symbol
            join semantic_documents document on document.rel_path = symbol.rel_path
            where symbol.symbol_id = ?1
            "#,
            [symbol_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .context("load semantic symbol scope")?;
    let Some((symbol_kind, mut node_id, language)) = symbol else {
        return Ok(None);
    };
    let callable_symbol = symbol_kind == "function";
    let non_local_kind = [
        "method",
        "class",
        "struct",
        "union",
        "enum",
        "trait",
        "module",
        "namespace",
        "field",
        "property",
        "type",
        "constructor",
        "destructor",
    ]
    .iter()
    .any(|marker| symbol_kind.contains(marker));
    let nested_callable_language = matches!(
        language.as_str(),
        "rust" | "java-script" | "javascript" | "type-script" | "typescript" | "python"
    );
    if non_local_kind || (callable_symbol && !nested_callable_language) {
        return Ok(None);
    }
    let mut declaration_owner = true;
    while let Some(current) = node_id {
        let node = conn
            .query_row(
                "select kind, parent_node_id from semantic_nodes where node_id = ?1",
                [&current],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()
            .context("walk semantic symbol scope")?;
        let Some((kind, parent)) = node else {
            break;
        };
        if kind.contains("function")
            || kind.contains("method")
            || kind.contains("lambda")
            || kind.contains("closure")
        {
            // A function symbol's scope_node_id is its own declaration owner.
            // Its name is not closed merely because its body is a function;
            // only a *second*, enclosing callable proves that the binding
            // cannot be named from another document. Parameters/locals keep
            // the existing behavior and close over their first callable.
            if !(callable_symbol && declaration_owner) {
                return Ok(Some(current));
            }
        }
        declaration_owner = false;
        node_id = parent;
    }
    Ok(None)
}

/// Local completeness is scope-aware. An unresolved token with the same text
/// in another function must not suppress an otherwise closed local symbol.
pub fn lexical_symbol_references_complete(conn: &Connection, symbol_id: &str) -> Result<bool> {
    let Some(closure_node_id) = symbol_lexical_closure_node(conn, symbol_id)? else {
        return Ok(false);
    };
    let unresolved_in_scope = conn
        .query_row(
            r#"
            with recursive ancestry(node_id, parent_node_id) as (
                select node.node_id, node.parent_node_id
                from semantic_occurrences unresolved
                join semantic_nodes node on node.node_id = unresolved.node_id
                where unresolved.role = 'unresolved'
                  and unresolved.name = (
                      select name from semantic_symbols where symbol_id = ?1
                  )
                  and unresolved.rel_path = (
                      select rel_path from semantic_symbols where symbol_id = ?1
                  )
                union
                select parent.node_id, parent.parent_node_id
                from ancestry child
                join semantic_nodes parent on parent.node_id = child.parent_node_id
            )
            select exists(select 1 from ancestry where node_id = ?2)
            "#,
            params![symbol_id, closure_node_id],
            |row| row.get::<_, i64>(0),
        )
        .context("check unresolved occurrences inside lexical symbol scope")?
        != 0;
    Ok(!unresolved_in_scope)
}

/// Build a complete syntax-backed closure for a C-family symbol whose linkage
/// proves it cannot escape one implementation translation unit. This is
/// intentionally narrower than "file local": headers are excluded because a
/// `static` definition in a header is instantiated independently by every
/// including translation unit.
pub fn translation_unit_local_reference_closure(
    conn: &Connection,
    root: &Path,
    symbol_id: &str,
) -> Result<Option<Vec<crate::lsp::LspLocation>>> {
    Ok(
        translation_unit_local_reference_closures(conn, root, &[symbol_id.to_string()])?
            .remove(symbol_id),
    )
}

/// Batch form of [`translation_unit_local_reference_closure`]. The old
/// one-symbol implementation walked SQLite ancestry, reopened the source, and
/// queried occurrences for every selected proposal. A large C++ approval queue
/// therefore spent seconds classifying symbols before clangd even started.
/// This performs the same fail-closed proof with two set queries and one source
/// read per implementation file.
pub fn translation_unit_local_reference_closures(
    conn: &Connection,
    root: &Path,
    symbol_ids: &[String],
) -> Result<BTreeMap<String, Vec<crate::lsp::LspLocation>>> {
    if symbol_ids.is_empty() {
        return Ok(BTreeMap::new());
    }

    #[derive(Debug)]
    struct Candidate {
        rel_path: String,
        kind: String,
        language: LanguageId,
        content_hash: String,
        declaration: TextRange,
        storage_ranges: Vec<(usize, usize)>,
        unresolved_same_name: bool,
    }

    #[derive(Debug)]
    struct CandidateRow {
        symbol_id: String,
        rel_path: String,
        kind: String,
        language: LanguageId,
        content_hash: String,
        declaration: TextRange,
        storage_start: Option<usize>,
        storage_end: Option<usize>,
        unresolved_same_name: bool,
    }

    let requested = symbol_ids.iter().cloned().collect::<BTreeSet<_>>();
    let requested_json =
        serde_json::to_string(&requested).context("encode translation-unit-local candidates")?;
    let rows = {
        let mut statement = conn
            .prepare(
                r#"
                with recursive requested(symbol_id) as (
                    select distinct cast(value as text) from json_each(?1)
                ), ancestry(symbol_id, node_id, parent_node_id, kind, depth) as (
                    select symbol.symbol_id, node.node_id, node.parent_node_id, node.kind, 0
                    from requested
                    join semantic_symbols symbol using(symbol_id)
                    join semantic_nodes node on node.node_id = symbol.node_id
                    union all
                    select child.symbol_id, parent.node_id, parent.parent_node_id,
                           parent.kind, child.depth + 1
                    from ancestry child
                    join semantic_nodes parent on parent.node_id = child.parent_node_id
                    where child.depth < 12
                )
                select symbol.symbol_id, symbol.rel_path, symbol.kind,
                       declaration.start_byte, declaration.end_byte,
                       declaration.start_line, declaration.start_column,
                       declaration.end_line, declaration.end_column,
                       document.language, document.content_hash,
                       storage.start_byte, storage.end_byte,
                       exists(
                           select 1 from semantic_occurrences unresolved
                           where unresolved.rel_path = symbol.rel_path
                             and unresolved.name = symbol.name
                             and unresolved.role = 'unresolved'
                       )
                from requested
                join semantic_symbols symbol using(symbol_id)
                join semantic_nodes declaration on declaration.node_id = symbol.node_id
                join semantic_documents document on document.rel_path = symbol.rel_path
                join ancestry owner
                  on owner.symbol_id = symbol.symbol_id
                 and owner.kind in ('function_definition', 'declaration')
                 and owner.depth = (
                     select min(candidate.depth) from ancestry candidate
                     where candidate.symbol_id = symbol.symbol_id
                       and candidate.kind in ('function_definition', 'declaration')
                 )
                left join semantic_nodes storage
                  on storage.parent_node_id = owner.node_id
                 and storage.kind = 'storage_class_specifier'
                order by symbol.symbol_id, storage.start_byte
                "#,
            )
            .context("prepare batched translation-unit-local candidates")?;
        statement
            .query_map([requested_json], |row| {
                Ok(CandidateRow {
                    symbol_id: row.get(0)?,
                    rel_path: row.get(1)?,
                    kind: row.get(2)?,
                    declaration: range_from_row(row, 3)?,
                    language: parse_language(&row.get::<_, String>(9)?),
                    content_hash: row.get(10)?,
                    storage_start: row.get::<_, Option<i64>>(11)?.map(|value| value as usize),
                    storage_end: row.get::<_, Option<i64>>(12)?.map(|value| value as usize),
                    unresolved_same_name: row.get::<_, i64>(13)? != 0,
                })
            })
            .context("query batched translation-unit-local candidates")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("collect batched translation-unit-local candidates")?
    };

    let mut candidates = BTreeMap::<String, Candidate>::new();
    for row in rows {
        let candidate = candidates
            .entry(row.symbol_id)
            .or_insert_with(|| Candidate {
                rel_path: row.rel_path,
                kind: row.kind,
                language: row.language,
                content_hash: row.content_hash,
                declaration: row.declaration,
                storage_ranges: Vec::new(),
                unresolved_same_name: row.unresolved_same_name,
            });
        if let (Some(start), Some(end)) = (row.storage_start, row.storage_end) {
            candidate.storage_ranges.push((start, end));
        }
    }

    let mut source_cache = BTreeMap::<String, String>::new();
    let mut eligible = BTreeSet::<String>::new();
    for (symbol_id, candidate) in &candidates {
        if !matches!(candidate.kind.as_str(), "function" | "variable")
            || !matches!(
                candidate.language,
                LanguageId::Cpp | LanguageId::ObjectiveC | LanguageId::ObjectiveCpp
            )
            || candidate.unresolved_same_name
            || candidate.storage_ranges.is_empty()
        {
            continue;
        }
        let extension = Path::new(&candidate.rel_path)
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !matches!(extension.as_str(), "c" | "cc" | "cpp" | "cxx" | "m" | "mm") {
            continue;
        }
        if !source_cache.contains_key(&candidate.rel_path) {
            let rel = normalize_safe_rel_path(
                Path::new(&candidate.rel_path),
                "translation-unit-local symbol",
            )?;
            let source = fs::read_to_string(root.join(&rel)).with_context(|| {
                format!("read translation-unit-local source {}", candidate.rel_path)
            })?;
            if sha256_hex(source.as_bytes()) != candidate.content_hash {
                bail!(
                    "{} changed since semantic indexing; run code-sanity index",
                    candidate.rel_path
                );
            }
            source_cache.insert(candidate.rel_path.clone(), source);
        }
        let source = &source_cache[&candidate.rel_path];
        if candidate.storage_ranges.iter().any(|(start, end)| {
            source
                .get(*start..*end)
                .is_some_and(|value| value == "static")
        }) {
            eligible.insert(symbol_id.clone());
        }
    }
    if eligible.is_empty() {
        return Ok(BTreeMap::new());
    }

    let eligible_json =
        serde_json::to_string(&eligible).context("encode static translation-unit candidates")?;
    let mut occurrences = BTreeMap::<String, Vec<crate::lsp::LspLocation>>::new();
    let mut declarations = BTreeSet::<String>::new();
    {
        let mut statement = conn
            .prepare(
                r#"
                select occurrence.symbol_id, occurrence.rel_path, occurrence.role,
                       occurrence.start_byte, occurrence.end_byte,
                       occurrence.start_line, occurrence.start_column,
                       occurrence.end_line, occurrence.end_column
                from semantic_occurrences occurrence
                where occurrence.symbol_id in (
                    select cast(value as text) from json_each(?1)
                )
                order by occurrence.symbol_id, occurrence.rel_path, occurrence.start_byte
                "#,
            )
            .context("prepare batched translation-unit-local occurrences")?;
        let rows = statement
            .query_map([eligible_json], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    range_from_row(row, 3)?,
                ))
            })
            .context("query batched translation-unit-local occurrences")?;
        for row in rows {
            let (symbol_id, rel_path, role, range) =
                row.context("read batched translation-unit-local occurrence")?;
            if role == "declaration"
                && candidates
                    .get(&symbol_id)
                    .is_some_and(|candidate| candidate.declaration == range)
            {
                declarations.insert(symbol_id.clone());
            }
            occurrences
                .entry(symbol_id)
                .or_default()
                .push(crate::lsp::LspLocation { rel_path, range });
        }
    }

    let mut closures = BTreeMap::new();
    for symbol_id in eligible {
        let Some(candidate) = candidates.get(&symbol_id) else {
            continue;
        };
        let Some(locations) = occurrences.remove(&symbol_id) else {
            continue;
        };
        if !declarations.contains(&symbol_id)
            || locations
                .iter()
                .any(|location| location.rel_path != candidate.rel_path)
        {
            continue;
        }
        closures.insert(symbol_id, locations);
    }
    Ok(closures)
}

#[derive(Debug)]
struct AdmittedCompilerLocation {
    rel_path: String,
    start_byte: usize,
    end_byte: usize,
    name: String,
    content_hash: String,
    role: String,
    existing_symbol_id: Option<String>,
}

#[derive(Debug)]
struct DetachedCompilerOccurrence {
    rel_path: String,
    start_byte: usize,
    end_byte: usize,
    name: String,
}

fn current_syntax_disproves_binding(
    parsed: &ParsedDocument,
    occurrence: &SemanticOccurrence,
    target: &SemanticSymbol,
) -> bool {
    let Some(current) = parsed.occurrences.iter().find(|candidate| {
        candidate.name == occurrence.name
            && candidate.range.start_byte == occurrence.range.start_byte
            && candidate.range.end_byte == occurrence.range.end_byte
    }) else {
        return false;
    };
    match current.role {
        OccurrenceRole::External => true,
        OccurrenceRole::Reference => current
            .symbol_id
            .as_ref()
            .and_then(|symbol_id| {
                parsed
                    .symbols
                    .iter()
                    .find(|symbol| &symbol.symbol_id == symbol_id)
            })
            .is_some_and(|owner| {
                owner.range != target.range
                    && !parsed.occurrences.iter().any(|candidate| {
                        candidate.role == OccurrenceRole::Declaration
                            && candidate.symbol_id.as_deref() == Some(owner.symbol_id.as_str())
                            && candidate.range == target.range
                    })
            }),
        OccurrenceRole::Declaration | OccurrenceRole::Unresolved => false,
    }
}

fn validate_compiler_reference_closure(
    conn: &Connection,
    root: &Path,
    symbol_id: &str,
    locations: &[crate::lsp::LspLocation],
    equivalent_symbol_ids: &BTreeSet<String>,
) -> Result<(
    String,
    SemanticSymbol,
    Vec<AdmittedCompilerLocation>,
    Vec<DetachedCompilerOccurrence>,
)> {
    let (symbol_path, symbol) = load_symbol_with_path(conn, symbol_id)?
        .ok_or_else(|| anyhow::anyhow!("compiler resolution target no longer exists"))?;
    if locations.is_empty() {
        bail!("language server returned no declaration/reference locations");
    }
    for equivalent_id in equivalent_symbol_ids {
        let equivalent = load_symbol(conn, equivalent_id)?.ok_or_else(|| {
            anyhow::anyhow!("supplemental compiler-equivalent symbol disappeared")
        })?;
        if equivalent.origin != SourceOrigin::Owned
            || equivalent.name != symbol.name
            || equivalent.kind != symbol.kind
            || equivalent.qualified_name != symbol.qualified_name
        {
            bail!(
                "supplemental compiler-equivalent symbol {} does not match {}",
                equivalent.qualified_name,
                symbol.qualified_name
            );
        }
    }
    let mut admitted = Vec::<AdmittedCompilerLocation>::new();
    let mut seen = BTreeSet::<(String, usize, usize)>::new();
    for location in locations {
        let key = (
            location.rel_path.clone(),
            location.range.start_byte,
            location.range.end_byte,
        );
        if !seen.insert(key) {
            continue;
        }
        let document = load_document(conn, &location.rel_path)?.ok_or_else(|| {
            anyhow::anyhow!(
                "compiler reference {}:{} is outside the semantic index",
                location.rel_path,
                location.range.start_line
            )
        })?;
        let rel = normalize_safe_rel_path(Path::new(&location.rel_path), "compiler reference")?;
        let source = fs::read_to_string(root.join(&rel))
            .with_context(|| format!("read compiler reference {}", location.rel_path))?;
        let content_hash = sha256_hex(source.as_bytes());
        if content_hash != document.content_hash {
            bail!(
                "{} changed while compiler references were being resolved",
                location.rel_path
            );
        }
        let referenced_name = source
            .get(location.range.start_byte..location.range.end_byte)
            .context("compiler reference is not a valid UTF-8 range")?;
        if referenced_name != symbol.name {
            bail!(
                "compiler reference {}:{} targets {:?}, expected {:?}",
                location.rel_path,
                location.range.start_line,
                referenced_name,
                symbol.name
            );
        }
        let occurrence = conn
            .query_row(
                r#"
                select role, symbol_id from semantic_occurrences
                where rel_path = ?1 and start_byte = ?2 and end_byte = ?3 and name = ?4
                limit 1
                "#,
                params![
                    location.rel_path,
                    location.range.start_byte as i64,
                    location.range.end_byte as i64,
                    symbol.name,
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()
            .context("match compiler reference to semantic occurrence")?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "compiler reference {}:{} has no exact syntax occurrence",
                    location.rel_path,
                    location.range.start_line
                )
            })?;
        admitted.push(AdmittedCompilerLocation {
            rel_path: location.rel_path.clone(),
            start_byte: location.range.start_byte,
            end_byte: location.range.end_byte,
            name: symbol.name.clone(),
            content_hash,
            role: occurrence.0,
            existing_symbol_id: occurrence.1,
        });
    }
    let admitted_keys = admitted
        .iter()
        .map(|location| {
            (
                location.rel_path.clone(),
                location.start_byte,
                location.end_byte,
            )
        })
        .collect::<BTreeSet<_>>();
    let target_document = load_document(conn, &symbol_path)?
        .ok_or_else(|| anyhow::anyhow!("compiler resolution target document disappeared"))?;
    let c_family_target = matches!(
        target_document.language,
        LanguageId::Cpp | LanguageId::ObjectiveC | LanguageId::ObjectiveCpp
    );
    let alternate_owner = if c_family_target {
        conn.query_row(
            r#"
            select exists(
                select 1 from semantic_symbols
                where name = ?1 and symbol_id != ?2 and origin = 'owned'
            )
            "#,
            params![symbol.name, symbol_id],
            |row| row.get::<_, i64>(0),
        )
        .context("check compiler-corrected alternate symbol owner")?
            != 0
    } else {
        false
    };
    let mut current_target_analysis = None::<ParsedDocument>;
    let mut detached = Vec::new();
    for (rel_path, occurrence) in occurrences_for_symbol(conn, symbol_id)? {
        if !admitted_keys.contains(&(
            rel_path.clone(),
            occurrence.range.start_byte,
            occurrence.range.end_byte,
        )) {
            let inside_preprocessor = conn
                .query_row(
                    r#"
                    with recursive ancestry(node_id, parent_node_id, kind, depth) as (
                        select node.node_id, node.parent_node_id, node.kind, 0
                        from semantic_nodes node where node.node_id = ?1
                        union all
                        select parent.node_id, parent.parent_node_id, parent.kind, child.depth + 1
                        from ancestry child
                        join semantic_nodes parent on parent.node_id = child.parent_node_id
                        where child.depth < 24
                    )
                    select exists(select 1 from ancestry where kind like 'preproc_%')
                    "#,
                    [&occurrence.node_id],
                    |row| row.get::<_, i64>(0),
                )
                .context("check omitted compiler occurrence preprocessor scope")?
                != 0;
            let syntax_disproves_binding = if c_family_target
                && alternate_owner
                && rel_path == symbol_path
                && occurrence.role != OccurrenceRole::Declaration
                && !inside_preprocessor
            {
                if current_target_analysis.is_none() {
                    let rel = normalize_safe_rel_path(
                        Path::new(&symbol_path),
                        "compiler-corrected syntax target",
                    )?;
                    let source = fs::read_to_string(root.join(&rel)).with_context(|| {
                        format!("read compiler-corrected syntax target {symbol_path}")
                    })?;
                    current_target_analysis = Some(crate::semantic::parse_document(&rel, &source)?);
                }
                current_syntax_disproves_binding(
                    current_target_analysis.as_ref().expect("initialized above"),
                    &occurrence,
                    &symbol,
                )
            } else {
                false
            };
            if syntax_disproves_binding {
                detached.push(DetachedCompilerOccurrence {
                    rel_path,
                    start_byte: occurrence.range.start_byte,
                    end_byte: occurrence.range.end_byte,
                    name: occurrence.name,
                });
                continue;
            }
            bail!(
                "language server omitted indexed occurrence {}:{}; refusing incomplete compiler binding",
                rel_path,
                occurrence.range.start_line
            );
        }
    }
    if !admitted.iter().any(|location| {
        location.rel_path == symbol_path
            && location.start_byte == symbol.range.start_byte
            && location.end_byte == symbol.range.end_byte
    }) {
        bail!("language server omitted the target declaration");
    }
    Ok((symbol_path, symbol, admitted, detached))
}

pub fn validate_compiler_references_with_equivalents(
    conn: &Connection,
    root: &Path,
    symbol_id: &str,
    locations: &[crate::lsp::LspLocation],
    equivalent_symbol_ids: &BTreeSet<String>,
) -> Result<()> {
    validate_compiler_reference_closure(conn, root, symbol_id, locations, equivalent_symbol_ids)?;
    Ok(())
}

pub fn admit_compiler_references(
    conn: &mut Connection,
    root: &Path,
    symbol_id: &str,
    provider: &str,
    locations: &[crate::lsp::LspLocation],
) -> Result<u64> {
    admit_compiler_references_with_equivalents(
        conn,
        root,
        symbol_id,
        provider,
        locations,
        &BTreeSet::new(),
    )
}

/// Admit an LSP reference closure plus syntax-proven declaration/definition
/// anchors that the server omitted (most commonly when clangd opened a header
/// without its compilation database). Supplemental anchors must describe the
/// exact same owned symbol shape before they can enter the compiler link set.
pub fn admit_compiler_references_with_equivalents(
    conn: &mut Connection,
    root: &Path,
    symbol_id: &str,
    provider: &str,
    locations: &[crate::lsp::LspLocation],
    equivalent_symbol_ids: &BTreeSet<String>,
) -> Result<u64> {
    admit_compiler_reference_batch(
        conn,
        root,
        &[CompilerReferenceAdmission {
            symbol_id: symbol_id.to_string(),
            provider: provider.to_string(),
            locations: locations.to_vec(),
            equivalent_symbol_ids: equivalent_symbol_ids.clone(),
        }],
    )
}

#[derive(Debug, Clone)]
pub(crate) struct CompilerReferenceAdmission {
    pub(crate) symbol_id: String,
    pub(crate) provider: String,
    pub(crate) locations: Vec<crate::lsp::LspLocation>,
    pub(crate) equivalent_symbol_ids: BTreeSet<String>,
}

/// Validate every compiler closure before opening the transaction, then admit
/// the whole batch at one semantic revision. A late DB failure rolls back all
/// closures instead of leaving a prefix of a Select All operation persisted.
pub(crate) fn admit_compiler_reference_batch(
    conn: &mut Connection,
    root: &Path,
    admissions: &[CompilerReferenceAdmission],
) -> Result<u64> {
    if admissions.is_empty() {
        return current_revision(conn);
    }
    struct ValidatedAdmission {
        symbol_id: String,
        provider: String,
        equivalent_symbol_ids: BTreeSet<String>,
        admitted: Vec<AdmittedCompilerLocation>,
        detached: Vec<DetachedCompilerOccurrence>,
        fingerprint: String,
    }

    let mut seen = BTreeSet::new();
    let mut validated = Vec::with_capacity(admissions.len());
    for admission in admissions {
        if !seen.insert(admission.symbol_id.clone()) {
            bail!(
                "compiler reference batch repeats canonical symbol {}",
                admission.symbol_id
            );
        }
        let (_, _symbol, admitted, detached) = validate_compiler_reference_closure(
            conn,
            root,
            &admission.symbol_id,
            &admission.locations,
            &admission.equivalent_symbol_ids,
        )?;

        let mut fingerprint_material = String::new();
        for location in &admitted {
            fingerprint_material.push_str(&location.rel_path);
            fingerprint_material.push('\0');
            fingerprint_material.push_str(&location.start_byte.to_string());
            fingerprint_material.push(':');
            fingerprint_material.push_str(&location.end_byte.to_string());
            fingerprint_material.push('\0');
            fingerprint_material.push_str(&location.content_hash);
            fingerprint_material.push('\n');
        }
        validated.push(ValidatedAdmission {
            symbol_id: admission.symbol_id.clone(),
            provider: admission.provider.clone(),
            equivalent_symbol_ids: admission.equivalent_symbol_ids.clone(),
            admitted,
            detached,
            fingerprint: sha256_hex(fingerprint_material.as_bytes()),
        });
    }

    let tx = conn
        .transaction()
        .context("begin compiler reference batch admission")?;
    let base_revision = current_revision(&tx)?;
    let next_revision = base_revision
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("semantic workspace revision overflow"))?;
    for admission in &validated {
        tx.execute(
            "delete from semantic_compiler_bindings where canonical_symbol_id = ?1",
            params![admission.symbol_id],
        )
        .context("clear old compiler bindings")?;
        tx.execute(
            "delete from semantic_compiler_links where canonical_symbol_id = ?1",
            params![admission.symbol_id],
        )
        .context("clear old compiler links")?;
        let mut linked = admission
            .admitted
            .iter()
            .filter_map(|location| location.existing_symbol_id.clone())
            .collect::<BTreeSet<_>>();
        linked.insert(admission.symbol_id.clone());
        linked.extend(admission.equivalent_symbol_ids.iter().cloned());
        for linked_symbol_id in linked {
            if load_symbol(&tx, &linked_symbol_id)?.is_none() {
                continue;
            }
            tx.execute(
                "insert into semantic_compiler_links(canonical_symbol_id, linked_symbol_id) values(?1, ?2)",
                params![admission.symbol_id, linked_symbol_id],
            )
            .context("insert compiler symbol link")?;
        }
        for location in &admission.admitted {
            if location.role == "declaration" {
                continue;
            }
            tx.execute(
                r#"
                delete from semantic_compiler_bindings
                where rel_path = ?1 and start_byte = ?2 and end_byte = ?3
                "#,
                params![
                    location.rel_path,
                    location.start_byte as i64,
                    location.end_byte as i64,
                ],
            )
            .context("remove conflicting compiler binding")?;
            tx.execute(
                r#"
                insert into semantic_compiler_bindings(
                  canonical_symbol_id, rel_path, start_byte, end_byte, name, content_hash
                ) values(?1, ?2, ?3, ?4, ?5, ?6)
                "#,
                params![
                    admission.symbol_id,
                    location.rel_path,
                    location.start_byte as i64,
                    location.end_byte as i64,
                    location.name,
                    location.content_hash,
                ],
            )
            .context("insert compiler binding")?;
            tx.execute(
                r#"
                update semantic_occurrences set symbol_id = ?1, role = 'reference'
                where rel_path = ?2 and start_byte = ?3 and end_byte = ?4
                  and name = ?5 and role != 'declaration'
                "#,
                params![
                    admission.symbol_id,
                    location.rel_path,
                    location.start_byte as i64,
                    location.end_byte as i64,
                    location.name,
                ],
            )
            .context("apply compiler binding")?;
        }
        for occurrence in &admission.detached {
            tx.execute(
                r#"
                update semantic_occurrences
                set symbol_id = null, role = 'unresolved'
                where rel_path = ?1 and start_byte = ?2 and end_byte = ?3
                  and name = ?4 and symbol_id = ?5 and role != 'declaration'
                "#,
                params![
                    occurrence.rel_path,
                    occurrence.start_byte as i64,
                    occurrence.end_byte as i64,
                    occurrence.name,
                    admission.symbol_id,
                ],
            )
            .context("detach compiler-disproved syntax binding")?;
        }
        tx.execute(
            r#"
            insert into semantic_compiler_resolutions(
              canonical_symbol_id, provider, locations_fingerprint, resolved_revision
            ) values(?1, ?2, ?3, ?4)
            on conflict(canonical_symbol_id) do update set
              provider = excluded.provider,
              locations_fingerprint = excluded.locations_fingerprint,
              resolved_revision = excluded.resolved_revision
            "#,
            params![
                admission.symbol_id,
                admission.provider,
                admission.fingerprint,
                next_revision as i64
            ],
        )
        .context("record compiler resolution")?;
    }
    let updated = tx
        .execute(
            "update semantic_workspace set revision = ?2 where singleton = 1 and revision = ?1",
            params![base_revision as i64, next_revision as i64],
        )
        .context("advance compiler resolution revision")?;
    if updated != 1 {
        bail!("semantic workspace revision changed during compiler reference batch admission");
    }
    tx.commit()
        .context("commit compiler reference batch admission")?;
    Ok(next_revision)
}

#[allow(clippy::too_many_arguments)]
pub fn record_proposal(
    conn: &Connection,
    proposal_id: &str,
    symbol_id: &str,
    occurrence_id: &str,
    replacement: &str,
    category: &str,
    confidence: f64,
    reason: &str,
    status: &str,
    created_at: &str,
) -> Result<()> {
    let revision = current_revision(conn)?;
    conn.execute(
        r#"
        insert into semantic_proposals(
          proposal_id, symbol_id, occurrence_id, replacement, category,
          confidence, reason, status, context_revision, created_at
        ) values(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
        on conflict(proposal_id) do update set status = excluded.status
        "#,
        params![
            proposal_id,
            symbol_id,
            occurrence_id,
            replacement,
            category,
            confidence,
            reason,
            status,
            revision as i64,
            created_at,
        ],
    )
    .context("record semantic proposal")?;
    Ok(())
}

pub fn update_proposal_status(conn: &Connection, proposal_id: &str, status: &str) -> Result<()> {
    conn.execute(
        "update semantic_proposals set status = ?2 where proposal_id = ?1",
        params![proposal_id, status],
    )
    .context("update semantic proposal status")?;
    Ok(())
}

pub fn occurrences_for_symbol(
    conn: &Connection,
    symbol_id: &str,
) -> Result<Vec<(String, SemanticOccurrence)>> {
    let mut statement = conn
        .prepare(
            r#"
            select rel_path, occurrence_id, node_id, symbol_id, name, role,
                   start_byte, end_byte, start_line, start_column, end_line, end_column
            from semantic_occurrences where symbol_id = ?1 order by rel_path, start_byte
            "#,
        )
        .context("prepare semantic occurrence query")?;
    let rows = statement
        .query_map(params![symbol_id], |row| {
            Ok((
                row.get(0)?,
                SemanticOccurrence {
                    occurrence_id: row.get(1)?,
                    node_id: row.get(2)?,
                    symbol_id: row.get(3)?,
                    name: row.get(4)?,
                    role: parse_role(&row.get::<_, String>(5)?),
                    range: range_from_row(row, 6)?,
                },
            ))
        })
        .context("query semantic occurrences")?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("collect semantic occurrences")
}

pub fn project_document(
    conn: &Connection,
    root: &Path,
    rel_path: &str,
) -> Result<ProjectedDocument> {
    let document = load_document(conn, rel_path)?
        .ok_or_else(|| anyhow::anyhow!("unindexed semantic document {rel_path}"))?;
    if !document.capabilities.parse {
        bail!(
            "{rel_path} has no AST projection backend; read_code refuses raw fallback (use the read-only legacy mirror tool if needed)"
        );
    }
    let rel = normalize_safe_rel_path(Path::new(rel_path), "projected document")?;
    let source = fs::read_to_string(root.join(&rel))
        .with_context(|| format!("read projected document {rel_path}"))?;
    if sha256_hex(source.as_bytes()) != document.content_hash {
        bail!("{rel_path} changed since semantic index; run code-sanity index");
    }
    let layout = Layout::new(root);
    let persisted_map = load_span_map(&layout.map_path(&rel)).with_context(|| {
        format!("load projected span map for {rel_path}; run code-sanity index")
    })?;
    if persisted_map.original_hash != document.content_hash {
        bail!("{rel_path} mirror projection is stale; run code-sanity index");
    }
    let projected_rel = if persisted_map.projected_path.is_empty() {
        rel.clone()
    } else {
        normalize_safe_rel_path(
            Path::new(&persisted_map.projected_path),
            "projected document path",
        )?
    };
    let persisted_content = fs::read_to_string(layout.mirror_dir.join(&projected_rel))
        .with_context(|| format!("read projected mirror {}", projected_rel.display()))?;
    if sha256_hex(persisted_content.as_bytes()) != persisted_map.sanitized_hash {
        bail!("{rel_path} projected mirror drifted; run code-sanity sync");
    }
    let current_projection = merge_semantic_aliases(
        conn,
        rel_path,
        &source,
        RenderedSanitization {
            sanitized: persisted_content,
            span_map: persisted_map,
        },
    )?;
    let span_map = current_projection.span_map;
    let content = current_projection.sanitized;

    let mut nodes = nodes_for_document(conn, rel_path)?;
    for node in &mut nodes {
        node.range = project_text_range(&span_map, &content, &node.range)?;
    }
    let mut symbols = symbols_for_document(conn, rel_path)?;
    let mut statement = conn
        .prepare(
            r#"
            select o.occurrence_id, o.node_id, o.symbol_id, o.name, o.role,
                   o.start_byte, o.end_byte, o.start_line, o.start_column, o.end_line, o.end_column,
                   a.sanitized_name
            from semantic_occurrences o
            left join semantic_aliases a on a.symbol_id = o.symbol_id and a.status = 'accepted'
            where o.rel_path = ?1 order by o.start_byte, o.end_byte
            "#,
        )
        .context("prepare projected occurrence query")?;
    let rows = statement
        .query_map(params![rel_path], |row| {
            Ok((
                SemanticOccurrence {
                    occurrence_id: row.get(0)?,
                    node_id: row.get(1)?,
                    symbol_id: row.get(2)?,
                    name: row.get(3)?,
                    role: parse_role(&row.get::<_, String>(4)?),
                    range: range_from_row(row, 5)?,
                },
                row.get::<_, Option<String>>(11)?,
            ))
        })
        .context("query projected occurrences")?;

    let mut occurrences = Vec::new();
    let mut projected_names = BTreeMap::<String, String>::new();
    for row in rows {
        let (occurrence, _alias) = row.context("read projected occurrence")?;
        let projected_range = project_text_range(&span_map, &content, &occurrence.range)?;
        if projected_range.end_byte > content.len()
            || !content.is_char_boundary(projected_range.start_byte)
            || !content.is_char_boundary(projected_range.end_byte)
        {
            bail!("{rel_path}: projected occurrence range is invalid");
        }
        let projected_name =
            content[projected_range.start_byte..projected_range.end_byte].to_string();
        if let Some(symbol_id) = occurrence.symbol_id.as_ref() {
            projected_names
                .entry(symbol_id.clone())
                .or_insert_with(|| projected_name.clone());
        }
        occurrences.push(ProjectedOccurrence {
            occurrence_id: occurrence.occurrence_id,
            node_id: occurrence.node_id,
            symbol_id: occurrence.symbol_id,
            name: projected_name.clone(),
            projected_name,
            role: occurrence.role,
            range: projected_range.clone(),
            projected_start_byte: projected_range.start_byte,
            projected_end_byte: projected_range.end_byte,
        });
    }
    let qualified_projection = qualified_projection_table(conn)?;
    for symbol in &mut symbols {
        let real_name = symbol.name.clone();
        let projected_name = projected_names
            .get(&symbol.symbol_id)
            .cloned()
            .unwrap_or_else(|| real_name.clone());
        symbol.name = projected_name.clone();
        symbol.qualified_name = project_qualified_name(
            &symbol.qualified_name,
            &real_name,
            &projected_name,
            &qualified_projection,
        );
        symbol.range = project_text_range(&span_map, &content, &symbol.range)?;
    }
    Ok(ProjectedDocument {
        rel_path: crate::config::normalize_rel_path(&projected_rel),
        revision: current_revision(conn)?,
        language: document.language,
        capabilities: document.capabilities,
        content,
        nodes,
        symbols,
        occurrences,
    })
}

fn qualified_projection_table(conn: &Connection) -> Result<BTreeMap<String, String>> {
    let mut candidates = BTreeMap::<String, BTreeSet<(String, String)>>::new();
    {
        let mut statement = conn
            .prepare(
                r#"
                select symbol.qualified_name, symbol.name, alias.sanitized_name
                from semantic_symbols symbol
                join semantic_aliases alias on alias.symbol_id = symbol.symbol_id
                where alias.status = 'accepted'
                "#,
            )
            .context("prepare qualified semantic projection")?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .context("query qualified semantic projection")?;
        for row in rows {
            let (qualified, original, alias) = row.context("read qualified semantic alias")?;
            candidates.entry(qualified).or_default().insert((
                original.clone(),
                crate::sanitize::adapt_replacement(&original, &alias),
            ));
        }
    }
    // The persisted map is also authoritative for global lexical replacements
    // that cannot safely become a symbol-scoped alias. Exact declaration spans
    // let qualified metadata use the same spelling as the physical mirror.
    {
        let mut statement = conn
            .prepare(
                r#"
                select symbol.qualified_name, symbol.name, replacement.sanitized_text
                from semantic_symbols symbol
                join semantic_nodes node on node.node_id = symbol.node_id
                join files file on file.rel_path = symbol.rel_path
                join spans span on span.file_id = file.id
                  and span.original_start = node.start_byte
                  and span.original_end = node.end_byte
                join replacements replacement on replacement.id = span.replacement_id
                "#,
            )
            .context("prepare qualified lexical projection")?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .context("query qualified lexical projection")?;
        for row in rows {
            let (qualified, original, projected) = row.context("read qualified lexical alias")?;
            candidates
                .entry(qualified)
                .or_default()
                .insert((original, projected));
        }
    }

    let mut components = candidates
        .into_iter()
        .filter_map(|(qualified, choices)| {
            (choices.len() == 1).then(|| (qualified, choices.into_iter().next().unwrap()))
        })
        .collect::<Vec<_>>();
    components.sort_by(|left, right| left.0.len().cmp(&right.0.len()).then(left.0.cmp(&right.0)));
    let mut projected = BTreeMap::<String, String>::new();
    for (qualified, (original, alias)) in components {
        let value = project_qualified_name(&qualified, &original, &alias, &projected);
        projected.insert(qualified, value);
    }
    Ok(projected)
}

fn project_qualified_name(
    qualified: &str,
    real_name: &str,
    projected_name: &str,
    known: &BTreeMap<String, String>,
) -> String {
    let mut projected = replace_qualified_component(qualified, real_name, projected_name)
        .unwrap_or_else(|| projected_name.to_string());
    if let Some((prefix, replacement)) = known
        .iter()
        .filter(|(prefix, _)| {
            prefix.len() < qualified.len()
                && qualified.starts_with(prefix.as_str())
                && qualified[prefix.len()..]
                    .chars()
                    .next()
                    .is_some_and(|ch| ch != '_' && !ch.is_ascii_alphanumeric())
        })
        .max_by_key(|(prefix, _)| prefix.len())
    {
        projected = format!("{replacement}{}", &projected[prefix.len()..]);
    }
    projected
}

fn replace_qualified_component(
    qualified: &str,
    real_name: &str,
    projected_name: &str,
) -> Option<String> {
    let start = qualified.rfind(real_name)?;
    let end = start + real_name.len();
    let before_ok = qualified[..start]
        .chars()
        .next_back()
        .is_none_or(|ch| ch != '_' && !ch.is_ascii_alphanumeric());
    let after_ok = qualified[end..]
        .chars()
        .next()
        .is_none_or(|ch| ch != '_' && !ch.is_ascii_alphanumeric());
    (before_ok && after_ok).then(|| {
        format!(
            "{}{}{}",
            &qualified[..start],
            projected_name,
            &qualified[end..]
        )
    })
}

pub(crate) fn project_text_range(
    map: &SpanMap,
    projected: &str,
    range: &TextRange,
) -> Result<TextRange> {
    let start = project_byte_offset(map, range.start_byte, false)?;
    let end = project_byte_offset(map, range.end_byte, true)?;
    if start > end || end > projected.len() {
        bail!("projected range {start}..{end} is outside rendered content");
    }
    Ok(text_range_for_bytes(projected, start, end))
}

pub(crate) fn project_original_byte_range(
    map: &SpanMap,
    start: usize,
    end: usize,
) -> Result<(usize, usize)> {
    Ok((
        project_byte_offset(map, start, false)?,
        project_byte_offset(map, end, true)?,
    ))
}

fn project_byte_offset(map: &SpanMap, offset: usize, end_bias: bool) -> Result<usize> {
    if offset > map.original_size {
        bail!("original byte offset {offset} is outside span map");
    }
    for span in &map.spans {
        if offset < span.original_start || offset > span.original_end {
            continue;
        }
        if span.replacement_id.is_none() {
            return Ok(span.sanitized_start + offset.saturating_sub(span.original_start));
        }
        if offset == span.original_start {
            return Ok(span.sanitized_start);
        }
        if offset == span.original_end {
            return Ok(span.sanitized_end);
        }
        return Ok(if end_bias {
            span.sanitized_end
        } else {
            span.sanitized_start
        });
    }
    if offset == map.original_size {
        return Ok(map.sanitized_size);
    }
    bail!("original byte offset {offset} is not covered by span map")
}

fn text_range_for_bytes(content: &str, start: usize, end: usize) -> TextRange {
    fn point(content: &str, offset: usize) -> (usize, usize) {
        let prefix = &content[..offset];
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
        let line_start = prefix.rfind('\n').map_or(0, |index| index + 1);
        (line, offset - line_start + 1)
    }
    let (start_line, start_column) = point(content, start);
    let (end_line, end_column) = point(content, end);
    TextRange {
        start_byte: start,
        end_byte: end,
        start_line,
        start_column,
        end_line,
        end_column,
    }
}

fn nodes_for_document(
    conn: &Connection,
    rel_path: &str,
) -> Result<Vec<crate::semantic::SemanticNode>> {
    let mut statement = conn
        .prepare(
            r#"
            select node_id, parent_node_id, kind,
                   start_byte, end_byte, start_line, start_column, end_line, end_column
            from semantic_nodes where rel_path = ?1 order by start_byte, end_byte desc
            "#,
        )
        .context("prepare document nodes query")?;
    let rows = statement
        .query_map(params![rel_path], |row| {
            Ok(crate::semantic::SemanticNode {
                node_id: row.get(0)?,
                parent_node_id: row.get(1)?,
                kind: row.get(2)?,
                range: range_from_row(row, 3)?,
            })
        })
        .context("query document nodes")?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("collect document nodes")
}

pub fn find_symbols(
    conn: &Connection,
    root: &Path,
    query: &str,
    limit: usize,
) -> Result<Vec<(String, SemanticSymbol)>> {
    let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
    let qualified_projection = qualified_projection_table(conn)?;
    let mut statement = conn
        .prepare(
            r#"
            select s.rel_path, s.symbol_id, s.node_id, s.name, s.kind, s.qualified_name,
                   s.scope_node_id, s.origin, s.locally_bound,
                   n.start_byte, n.end_byte, n.start_line, n.start_column, n.end_line, n.end_column,
                   a.sanitized_name, replacement.sanitized_text
            from semantic_symbols s
            join semantic_nodes n on n.node_id = s.node_id
            left join files file on file.rel_path = s.rel_path
            left join spans span on span.file_id = file.id
              and span.original_start = n.start_byte
              and span.original_end = n.end_byte
            left join replacements replacement on replacement.id = span.replacement_id
            left join semantic_aliases a on a.symbol_id = s.symbol_id and a.status = 'accepted'
            where (a.sanitized_name is not null and a.sanitized_name like ?1 escape '\')
               or (a.sanitized_name is null and replacement.sanitized_text is not null
                   and replacement.sanitized_text like ?1 escape '\')
               or (a.sanitized_name is null and replacement.sanitized_text is null
                   and s.name like ?1 escape '\')
            order by s.rel_path, n.start_byte limit ?2
            "#,
        )
        .context("prepare semantic symbol search")?;
    let rows = statement
        .query_map(params![pattern, limit.clamp(1, 1000) as i64], |row| {
            let real_name = row.get::<_, String>(3)?;
            let projected_name = row
                .get::<_, Option<String>>(15)?
                .map(|alias| crate::sanitize::adapt_replacement(&real_name, &alias))
                .or(row.get::<_, Option<String>>(16)?)
                .unwrap_or_else(|| real_name.clone());
            let qualified = row.get::<_, String>(5)?;
            Ok((
                row.get(0)?,
                real_name.clone(),
                SemanticSymbol {
                    symbol_id: row.get(1)?,
                    node_id: row.get(2)?,
                    name: projected_name.clone(),
                    kind: row.get(4)?,
                    qualified_name: project_qualified_name(
                        &qualified,
                        &real_name,
                        &projected_name,
                        &qualified_projection,
                    ),
                    scope_node_id: row.get(6)?,
                    origin: parse_origin(&row.get::<_, String>(7)?),
                    locally_bound: row.get::<_, i64>(8)? != 0,
                    range: range_from_row(row, 9)?,
                },
            ))
        })
        .context("query semantic symbols")?;
    let layout = Layout::new(root);
    let mut projected = Vec::new();
    for row in rows {
        let (rel_path, _real_name, mut symbol) = row.context("read semantic symbol search")?;
        let rel = normalize_safe_rel_path(Path::new(&rel_path), "semantic symbol path")?;
        let map = load_span_map(&layout.map_path(&rel))
            .with_context(|| format!("load projected range for {rel_path}"))?;
        let projected_rel = if map.projected_path.is_empty() {
            rel.clone()
        } else {
            normalize_safe_rel_path(Path::new(&map.projected_path), "projected symbol path")?
        };
        let content = fs::read_to_string(layout.mirror_dir.join(&projected_rel))?;
        symbol.range = project_text_range(&map, &content, &symbol.range)?;
        projected.push((rel_path, symbol));
    }
    Ok(projected)
}

fn symbols_for_document(conn: &Connection, rel_path: &str) -> Result<Vec<SemanticSymbol>> {
    let mut statement = conn
        .prepare(
            r#"
            select s.symbol_id, s.node_id, s.name, s.kind, s.qualified_name,
                   s.scope_node_id, s.origin, s.locally_bound,
                   n.start_byte, n.end_byte, n.start_line, n.start_column, n.end_line, n.end_column
            from semantic_symbols s join semantic_nodes n on n.node_id = s.node_id
            where s.rel_path = ?1 order by n.start_byte
            "#,
        )
        .context("prepare document symbols query")?;
    let rows = statement
        .query_map(params![rel_path], |row| {
            Ok(SemanticSymbol {
                symbol_id: row.get(0)?,
                node_id: row.get(1)?,
                name: row.get(2)?,
                kind: row.get(3)?,
                qualified_name: row.get(4)?,
                scope_node_id: row.get(5)?,
                origin: parse_origin(&row.get::<_, String>(6)?),
                locally_bound: row.get::<_, i64>(7)? != 0,
                range: range_from_row(row, 8)?,
            })
        })
        .context("query document symbols")?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("collect document symbols")
}

fn range_from_row(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<TextRange> {
    Ok(TextRange {
        start_byte: row.get::<_, i64>(offset)? as usize,
        end_byte: row.get::<_, i64>(offset + 1)? as usize,
        start_line: row.get::<_, i64>(offset + 2)? as usize,
        start_column: row.get::<_, i64>(offset + 3)? as usize,
        end_line: row.get::<_, i64>(offset + 4)? as usize,
        end_column: row.get::<_, i64>(offset + 5)? as usize,
    })
}

fn enum_text(value: impl Serialize) -> Result<String> {
    let value = serde_json::to_value(value).context("serialize enum value")?;
    value
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!("enum did not serialize as a string"))
}

fn parse_origin(value: &str) -> SourceOrigin {
    match value {
        "generated" => SourceOrigin::Generated,
        "vendor" => SourceOrigin::Vendor,
        "dependency" => SourceOrigin::Dependency,
        _ => SourceOrigin::Owned,
    }
}

fn parse_role(value: &str) -> OccurrenceRole {
    match value {
        "declaration" => OccurrenceRole::Declaration,
        "reference" => OccurrenceRole::Reference,
        "external" => OccurrenceRole::External,
        _ => OccurrenceRole::Unresolved,
    }
}

fn parse_language(value: &str) -> LanguageId {
    match value {
        "rust" => LanguageId::Rust,
        "cpp" => LanguageId::Cpp,
        "objective-c" => LanguageId::ObjectiveC,
        "objective-cpp" => LanguageId::ObjectiveCpp,
        "java-script" | "javascript" => LanguageId::JavaScript,
        "type-script" | "typescript" => LanguageId::TypeScript,
        "python" => LanguageId::Python,
        "go" => LanguageId::Go,
        _ => LanguageId::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unresolved_same_name_in_another_function_does_not_open_local_scope() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("src/lib.rs"),
            "fn first() { let scoped_token = 1; let _ = scoped_token; }\n\
             fn second() {}\n",
        )
        .unwrap();
        crate::index_workspace(repo.path()).unwrap();
        let layout = Layout::new(repo.path());
        let conn = db::connect(&layout).unwrap();
        let (second_node, start_byte, end_byte, line, column) = conn
            .query_row(
                r#"
                select symbol.node_id, node.start_byte, node.end_byte,
                       node.start_line, node.start_column
                from semantic_symbols symbol
                join semantic_nodes node on node.node_id = symbol.node_id
                where symbol.name = 'second'
                "#,
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .unwrap();
        conn.execute(
            r#"
            insert into semantic_occurrences(
                occurrence_id, rel_path, node_id, symbol_id, name, role,
                start_byte, end_byte, start_line, start_column, end_line, end_column
            ) values('test-cross-scope-unresolved', 'src/lib.rs', ?1, null,
                     'scoped_token', 'unresolved', ?2, ?3, ?4, ?5, ?4, ?5)
            "#,
            params![second_node, start_byte, end_byte, line, column],
        )
        .unwrap();
        let symbol_id = conn
            .query_row(
                r#"
                select symbol.symbol_id
                from semantic_symbols symbol
                join semantic_nodes node on node.node_id = symbol.node_id
                where symbol.name = 'scoped_token' and node.start_line = 1
                limit 1
                "#,
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        let unresolved: i64 = conn
            .query_row(
                "select count(*) from semantic_occurrences where name = 'scoped_token' and role = 'unresolved'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(unresolved > 0);
        assert!(symbol_is_lexically_closed(&conn, &symbol_id).unwrap());
        assert!(lexical_symbol_references_complete(&conn, &symbol_id).unwrap());
        assert!(symbol_projection_is_complete(&conn, &symbol_id).unwrap());
    }

    #[test]
    fn nested_javascript_function_is_closed_but_top_level_function_is_not() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("src/nested.js"),
            "(() => { function encGetEntropy() { return 7; } use(encGetEntropy()); })();\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("src/global.js"),
            "function globalHelper() { return 7; } use(globalHelper());\n",
        )
        .unwrap();
        crate::index_workspace(repo.path()).unwrap();
        let layout = Layout::new(repo.path());
        let conn = db::connect(&layout).unwrap();
        let symbol = |path: &str, name: &str| {
            conn.query_row(
                "select symbol_id from semantic_symbols where rel_path = ?1 and name = ?2",
                params![path, name],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
        };

        let nested = symbol("src/nested.js", "encGetEntropy");
        assert!(symbol_is_lexically_closed(&conn, &nested).unwrap());
        assert!(lexical_symbol_references_complete(&conn, &nested).unwrap());
        assert!(symbol_projection_is_complete(&conn, &nested).unwrap());

        let top_level = symbol("src/global.js", "globalHelper");
        assert!(!symbol_is_lexically_closed(&conn, &top_level).unwrap());
        assert!(!symbol_projection_is_complete(&conn, &top_level).unwrap());

        let stored = load_document(&conn, "src/nested.js").unwrap().unwrap();
        assert_eq!(stored.language, LanguageId::JavaScript);
    }

    #[test]
    fn compiler_linked_alias_member_uses_the_canonical_resolution() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("src/api.hpp"),
            "int shared_operation(int value);\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("src/api.cpp"),
            "int shared_operation(int value) { return value + 1; }\n",
        )
        .unwrap();
        crate::index_workspace(repo.path()).unwrap();
        let layout = Layout::new(repo.path());
        let conn = db::connect(&layout).unwrap();
        let mut symbols = conn
            .prepare(
                "select symbol_id from semantic_symbols where name = 'shared_operation' order by rel_path",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        symbols.dedup();
        assert_eq!(symbols.len(), 2);
        let canonical = &symbols[0];
        for symbol_id in &symbols {
            conn.execute(
                "insert into semantic_compiler_links(canonical_symbol_id, linked_symbol_id) values(?1, ?2)",
                params![canonical, symbol_id],
            )
            .unwrap();
        }
        conn.execute(
            r#"
            insert into semantic_compiler_resolutions(
              canonical_symbol_id, provider, locations_fingerprint, resolved_revision
            ) values(?1, 'clangd-test', 'fingerprint', 1)
            "#,
            [canonical],
        )
        .unwrap();

        assert!(symbol_projection_is_complete(&conn, canonical).unwrap());
        assert!(symbol_projection_is_complete(&conn, &symbols[1]).unwrap());
    }

    #[test]
    fn legacy_alias_migration_quarantines_the_lower_priority_collision_once() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("src/lib.rs"),
            "fn first() { let private_alpha = 1; let _ = private_alpha; }\n\
             fn second() { let private_beta = 2; let _ = private_beta; }\n",
        )
        .unwrap();
        crate::index_workspace(repo.path()).unwrap();
        let layout = Layout::new(repo.path());
        let mut conn = db::connect(&layout).unwrap();
        let symbol = |conn: &Connection, name: &str| {
            conn.query_row(
                "select symbol_id from semantic_symbols where name = ?1 limit 1",
                [name],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
        };
        let alpha = symbol(&conn, "private_alpha");
        let beta = symbol(&conn, "private_beta");
        for (symbol_id, original, confidence, revision) in [
            (&alpha, "private_alpha", 0.95, 10_i64),
            (&beta, "private_beta", 0.80, 11_i64),
        ] {
            conn.execute(
                r#"
                insert into semantic_aliases(
                  symbol_id, original_name, sanitized_name, category, confidence,
                  reason, status, source, created_revision
                ) values(?1, ?2, 'localValue', 'identifier', ?3, 'legacy test',
                         'accepted', 'proposal-v2', ?4)
                "#,
                params![symbol_id, original, confidence, revision],
            )
            .unwrap();
        }
        conn.execute(
            "delete from semantic_migrations where migration_key = 'semantic-alias-safety-v2'",
            [],
        )
        .unwrap();

        let quarantined = quarantine_legacy_invalid_accepted_aliases(&mut conn).unwrap();
        assert_eq!(quarantined.len(), 1);
        assert_eq!(quarantined[0].symbol_id, beta);
        assert!(quarantined[0].reason.contains("higher-priority"));
        let remaining = conn
            .query_row(
                "select symbol_id from semantic_aliases where status = 'accepted'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert_eq!(remaining, alpha);
        assert!(
            quarantine_legacy_invalid_accepted_aliases(&mut conn)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn stale_alias_quarantine_removes_its_compiler_component() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("src/lib.rs"),
            "fn run() { let private_value = 1; let _ = private_value; }\n",
        )
        .unwrap();
        crate::index_workspace(repo.path()).unwrap();
        let layout = Layout::new(repo.path());
        let mut conn = db::connect(&layout).unwrap();
        let symbol_id = conn
            .query_row(
                "select symbol_id from semantic_symbols where name = 'private_value' limit 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        conn.execute(
            r#"
            insert into semantic_aliases(
              symbol_id, original_name, sanitized_name, category, confidence,
              reason, status, source, created_revision
            ) values(?1, 'private_value', 'localValue', 'identifier', 0.9,
                     'stale test', 'stale', 'proposal-v2', 1)
            "#,
            [&symbol_id],
        )
        .unwrap();
        conn.execute(
            "insert into semantic_compiler_links(canonical_symbol_id, linked_symbol_id) values(?1, ?1)",
            [&symbol_id],
        )
        .unwrap();
        conn.execute(
            r#"
            insert into semantic_compiler_resolutions(
              canonical_symbol_id, provider, locations_fingerprint, resolved_revision
            ) values(?1, 'test', 'fingerprint', 1)
            "#,
            [&symbol_id],
        )
        .unwrap();

        let quarantined = quarantine_unrestored_stale_aliases(&mut conn).unwrap();
        assert_eq!(quarantined.len(), 1);
        for table in [
            "semantic_aliases",
            "semantic_compiler_links",
            "semantic_compiler_resolutions",
        ] {
            let count = conn
                .query_row(&format!("select count(*) from {table}"), [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{table} should be empty");
        }
    }

    #[test]
    fn implementation_static_function_has_a_complete_syntax_closure() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("src/feature.cpp"),
            "#ifdef ENABLE_FEATURE\n\
             static int hidden_feature(int value) { return value + 1; }\n\
             int use_feature() { return hidden_feature(1) + hidden_feature(2); }\n\
             #endif\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("src/header.hpp"),
            "static inline int header_feature(int value) { return value + 1; }\n",
        )
        .unwrap();
        crate::index_workspace(repo.path()).unwrap();
        let layout = Layout::new(repo.path());
        let conn = db::connect(&layout).unwrap();
        let symbol_id = |name: &str| {
            conn.query_row(
                "select symbol_id from semantic_symbols where name = ?1 limit 1",
                [name],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
        };

        let implementation = symbol_id("hidden_feature");
        let locations =
            translation_unit_local_reference_closure(&conn, repo.path(), &implementation)
                .unwrap()
                .unwrap();
        assert_eq!(locations.len(), 3);
        validate_compiler_references_with_equivalents(
            &conn,
            repo.path(),
            &implementation,
            &locations,
            &BTreeSet::new(),
        )
        .unwrap();

        let header = symbol_id("header_feature");
        let batched = translation_unit_local_reference_closures(
            &conn,
            repo.path(),
            &[implementation.clone(), header.clone()],
        )
        .unwrap();
        assert_eq!(batched[&implementation], locations);
        assert!(!batched.contains_key(&header));
        assert!(
            translation_unit_local_reference_closure(&conn, repo.path(), &header)
                .unwrap()
                .is_none(),
            "header-local linkage is per including translation unit, not per physical file"
        );
    }

    #[test]
    fn compiler_closure_detaches_a_legacy_wrong_receiver_binding() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        let source = r#"class Database { public: void delete_agent(int token); };
class ServerState { public: void delete_agent(int token) {} };
ServerState g_state;
void route(Database& db) {
    g_state.delete_agent(1);
    db.delete_agent(1);
}
"#;
        std::fs::write(repo.path().join("src/server.cpp"), source).unwrap();
        crate::index_workspace(repo.path()).unwrap();
        let layout = Layout::new(repo.path());
        let mut conn = db::connect(&layout).unwrap();
        let position = |prefix: &str| source.find(prefix).unwrap() + prefix.len();
        let target_start = position("class ServerState { public: void ");
        let state_call = position("g_state.");
        let database_call = position("db.");
        let target: String = conn
            .query_row(
                "select symbol_id from semantic_symbols where rel_path = 'src/server.cpp' and name = 'delete_agent' and node_id in (select node_id from semantic_nodes where start_byte = ?1)",
                [target_start as i64],
                |row| row.get(0),
            )
            .unwrap();

        // Simulate the legacy resolver bug from an existing workspace: the
        // Database receiver was attached to ServerState solely by spelling.
        conn.execute(
            r#"
            update semantic_occurrences set symbol_id = ?1, role = 'reference'
            where rel_path = 'src/server.cpp' and start_byte = ?2
            "#,
            params![target, database_call as i64],
        )
        .unwrap();
        let location = |start_byte: usize| crate::lsp::LspLocation {
            rel_path: "src/server.cpp".to_string(),
            range: text_range_for_bytes(source, start_byte, start_byte + "delete_agent".len()),
        };
        admit_compiler_references(
            &mut conn,
            repo.path(),
            &target,
            "clangd-test",
            &[location(target_start), location(state_call)],
        )
        .unwrap();

        let (role, owner): (String, Option<String>) = conn
            .query_row(
                "select role, symbol_id from semantic_occurrences where rel_path = 'src/server.cpp' and start_byte = ?1",
                [database_call as i64],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(role, "unresolved");
        assert_eq!(owner, None);
        accept_symbol_alias(&mut conn, &target, "remove_client", "identifier", 1.0, None).unwrap();
        let projected = project_document(&conn, repo.path(), "src/server.cpp").unwrap();
        assert!(projected.content.contains("g_state.remove_client(1)"));
        assert!(projected.content.contains("db.delete_agent(1)"));
    }

    #[test]
    fn compiler_correction_keeps_a_coalesced_redeclaration_owner() {
        let source = "int helper(int value);\nint helper(int value) { return value; }\nint run() { return helper(1); }\n";
        let parsed = crate::semantic::parse_document(Path::new("src/main.cpp"), source).unwrap();
        let owner = parsed
            .symbols
            .iter()
            .find(|symbol| symbol.name == "helper")
            .unwrap();
        let declarations = parsed
            .occurrences
            .iter()
            .filter(|occurrence| {
                occurrence.role == OccurrenceRole::Declaration
                    && occurrence.symbol_id.as_deref() == Some(owner.symbol_id.as_str())
            })
            .collect::<Vec<_>>();
        assert_eq!(declarations.len(), 2);
        let call = parsed
            .occurrences
            .iter()
            .find(|occurrence| {
                occurrence.role == OccurrenceRole::Reference && occurrence.name == "helper"
            })
            .unwrap();
        let mut compatibility_anchor = owner.clone();
        compatibility_anchor.range = declarations[1].range.clone();
        assert!(!current_syntax_disproves_binding(
            &parsed,
            call,
            &compatibility_anchor
        ));
    }

    #[test]
    fn compiler_closure_never_detaches_an_omission_inside_preprocessor_code() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        let source = r#"class Database { public: void delete_agent(int token); };
class ServerState { public: void delete_agent(int token) {} };
ServerState g_state;
void route(Database& db) {
    g_state.delete_agent(1);
#ifdef ALSO_DELETE_DATABASE
    db.delete_agent(1);
#endif
}
"#;
        std::fs::write(repo.path().join("src/server.cpp"), source).unwrap();
        crate::index_workspace(repo.path()).unwrap();
        let layout = Layout::new(repo.path());
        let mut conn = db::connect(&layout).unwrap();
        let position = |prefix: &str| source.find(prefix).unwrap() + prefix.len();
        let target_start = position("class ServerState { public: void ");
        let state_call = position("g_state.");
        let database_call = position("db.");
        let target: String = conn
            .query_row(
                "select symbol_id from semantic_symbols where rel_path = 'src/server.cpp' and node_id in (select node_id from semantic_nodes where start_byte = ?1)",
                [target_start as i64],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "update semantic_occurrences set symbol_id = ?1, role = 'reference' where rel_path = 'src/server.cpp' and start_byte = ?2",
            params![target, database_call as i64],
        )
        .unwrap();
        let location = |start_byte: usize| crate::lsp::LspLocation {
            rel_path: "src/server.cpp".to_string(),
            range: text_range_for_bytes(source, start_byte, start_byte + "delete_agent".len()),
        };
        let error = admit_compiler_references(
            &mut conn,
            repo.path(),
            &target,
            "clangd-test",
            &[location(target_start), location(state_call)],
        )
        .unwrap_err();
        assert!(error.to_string().contains("omitted indexed occurrence"));
    }

    #[test]
    fn compiler_reference_batch_is_all_or_nothing_when_one_closure_is_incomplete() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("src/batch.cpp"),
            "int first_target(int value) { return value; }\n\
             int second_target(int value) { return value; }\n\
             int use_targets() { return first_target(1) + second_target(2); }\n",
        )
        .unwrap();
        crate::index_workspace(repo.path()).unwrap();
        let layout = Layout::new(repo.path());
        let mut conn = db::connect(&layout).unwrap();
        let symbol = |name: &str| {
            conn.query_row(
                "select symbol_id from semantic_symbols where name = ?1 limit 1",
                [name],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
        };
        let first = symbol("first_target");
        let second = symbol("second_target");
        let locations = |symbol_id: &str| {
            occurrences_for_symbol(&conn, symbol_id)
                .unwrap()
                .into_iter()
                .map(|(rel_path, occurrence)| crate::lsp::LspLocation {
                    rel_path,
                    range: occurrence.range,
                })
                .collect::<Vec<_>>()
        };
        let first_locations = locations(&first);
        let second_declaration = locations(&second)
            .into_iter()
            .next()
            .into_iter()
            .collect::<Vec<_>>();
        let error = admit_compiler_reference_batch(
            &mut conn,
            repo.path(),
            &[
                CompilerReferenceAdmission {
                    symbol_id: first,
                    provider: "test-clangd".to_string(),
                    locations: first_locations,
                    equivalent_symbol_ids: BTreeSet::new(),
                },
                CompilerReferenceAdmission {
                    symbol_id: second,
                    provider: "test-clangd".to_string(),
                    locations: second_declaration,
                    equivalent_symbol_ids: BTreeSet::new(),
                },
            ],
        )
        .unwrap_err();
        assert!(error.to_string().contains("omitted indexed occurrence"));
        let resolutions: i64 = conn
            .query_row(
                "select count(*) from semantic_compiler_resolutions",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let links: i64 = conn
            .query_row("select count(*) from semantic_compiler_links", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(resolutions, 0);
        assert_eq!(links, 0);
    }

    #[test]
    fn qualified_projection_rewrites_aliased_parent_and_leaf() {
        let mut known = BTreeMap::new();
        known.insert("RawNamespace".to_string(), "SafeNamespace".to_string());
        known.insert(
            "RawNamespace::RawClass".to_string(),
            "SafeNamespace::SafeClass".to_string(),
        );
        assert_eq!(
            project_qualified_name(
                "RawNamespace::RawClass::raw_method",
                "raw_method",
                "safe_method",
                &known,
            ),
            "SafeNamespace::SafeClass::safe_method"
        );
    }
}
