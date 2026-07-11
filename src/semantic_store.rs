//! SQLite persistence for the versioned semantic index.

use crate::config::{Layout, normalize_safe_rel_path};
use crate::db;
use crate::map::{load_span_map, sha256_hex};
use crate::semantic::{
    LanguageId, OccurrenceRole, ParsedDocument, SemanticOccurrence, SemanticSymbol, SourceOrigin,
    TextRange,
};
use anyhow::{Context, Result, bail};
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
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceSnapshot {
    pub revision: u64,
    pub documents: usize,
    pub symbols: usize,
    pub occurrences: usize,
    pub unresolved_occurrences: usize,
    pub aliases: usize,
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
    pub name: String,
    pub projected_name: String,
    pub role: OccurrenceRole,
    pub original_range: TextRange,
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

/// Incrementally refresh semantic documents while the caller holds the
/// exclusive workspace lock. Parsing is local CPU work; no LSP/model request
/// is made from this path.
pub(crate) fn index_workspace_locked(root: &Path, layout: &Layout) -> Result<SemanticIndexReport> {
    let mut conn = db::connect(layout)?;
    db::ensure_schema(&conn)?;
    let previous = document_hashes(&conn)?;
    let tracked = db::tracked_files(&conn)?;
    let lexical_hashes = db::all_index_states(&conn)?
        .into_iter()
        .map(|state| (state.rel_path, state.input_sha256))
        .collect::<BTreeMap<_, _>>();
    let tracked_set = tracked.iter().cloned().collect::<BTreeSet<_>>();
    let mut report = SemanticIndexReport::default();
    let mut documents = Vec::new();

    for stored_path in tracked {
        if previous.get(&stored_path) == lexical_hashes.get(&stored_path) {
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
            Ok(document) => {
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
    Ok(report)
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
            Some("where symbol_id is null"),
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

pub fn accept_symbol_alias(
    conn: &mut Connection,
    symbol_id: &str,
    replacement: &str,
    category: &str,
    confidence: f64,
    reason: Option<&str>,
) -> Result<u64> {
    let symbol = load_symbol(conn, symbol_id)?
        .ok_or_else(|| anyhow::anyhow!("proposal target symbol_id does not exist"))?;
    if symbol.origin != SourceOrigin::Owned {
        bail!("proposal target is not owned source code");
    }
    if !symbol_projection_is_complete(conn, symbol_id)? {
        bail!("proposal target has unresolved references; semantic projection would be incomplete");
    }
    let tx = conn
        .transaction()
        .context("begin semantic alias approval transaction")?;
    let base_revision = current_revision(&tx)?;
    let next_revision = base_revision
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("semantic workspace revision overflow"))?;
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
            symbol_id,
            symbol.name,
            replacement,
            category,
            confidence,
            reason,
            next_revision as i64,
        ],
    )
    .context("upsert accepted semantic alias")?;
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
    let unresolved = conn
        .query_row(
            r#"
            select count(*) from semantic_occurrences unresolved
            where unresolved.role = 'unresolved'
              and unresolved.name = (select name from semantic_symbols where symbol_id = ?1)
            "#,
            params![symbol_id],
            |row| row.get::<_, i64>(0),
        )
        .context("check unresolved occurrences for semantic alias")?;
    Ok(unresolved == 0)
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
    let source = fs::read_to_string(root.join(rel))
        .with_context(|| format!("read projected document {rel_path}"))?;
    if sha256_hex(source.as_bytes()) != document.content_hash {
        bail!("{rel_path} changed since semantic index; run code-sanity index");
    }

    let nodes = nodes_for_document(conn, rel_path)?;
    let symbols = symbols_for_document(conn, rel_path)?;
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

    let mut content = String::with_capacity(source.len());
    let mut occurrences = Vec::new();
    let mut cursor = 0usize;
    for row in rows {
        let (occurrence, alias) = row.context("read projected occurrence")?;
        if occurrence.range.start_byte < cursor || occurrence.range.end_byte > source.len() {
            continue;
        }
        content.push_str(&source[cursor..occurrence.range.start_byte]);
        let projected_start_byte = content.len();
        let projected_name = alias
            .as_deref()
            .map(|alias| crate::sanitize::adapt_replacement(&occurrence.name, alias))
            .unwrap_or_else(|| occurrence.name.clone());
        content.push_str(&projected_name);
        let projected_end_byte = content.len();
        cursor = occurrence.range.end_byte;
        occurrences.push(ProjectedOccurrence {
            occurrence_id: occurrence.occurrence_id,
            node_id: occurrence.node_id,
            symbol_id: occurrence.symbol_id,
            name: occurrence.name,
            projected_name,
            role: occurrence.role,
            original_range: occurrence.range,
            projected_start_byte,
            projected_end_byte,
        });
    }
    content.push_str(&source[cursor..]);
    Ok(ProjectedDocument {
        rel_path: rel_path.to_string(),
        revision: current_revision(conn)?,
        language: document.language,
        capabilities: document.capabilities,
        content,
        nodes,
        symbols,
        occurrences,
    })
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
    query: &str,
    limit: usize,
) -> Result<Vec<(String, SemanticSymbol)>> {
    let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
    let mut statement = conn
        .prepare(
            r#"
            select s.rel_path, s.symbol_id, s.node_id, s.name, s.kind, s.qualified_name,
                   s.scope_node_id, s.origin, s.locally_bound,
                   n.start_byte, n.end_byte, n.start_line, n.start_column, n.end_line, n.end_column
            from semantic_symbols s join semantic_nodes n on n.node_id = s.node_id
            where s.name like ?1 escape '\' or s.qualified_name like ?1 escape '\'
            order by s.rel_path, n.start_byte limit ?2
            "#,
        )
        .context("prepare semantic symbol search")?;
    let rows = statement
        .query_map(params![pattern, limit.clamp(1, 1000) as i64], |row| {
            Ok((
                row.get(0)?,
                SemanticSymbol {
                    symbol_id: row.get(1)?,
                    node_id: row.get(2)?,
                    name: row.get(3)?,
                    kind: row.get(4)?,
                    qualified_name: row.get(5)?,
                    scope_node_id: row.get(6)?,
                    origin: parse_origin(&row.get::<_, String>(7)?),
                    locally_bound: row.get::<_, i64>(8)? != 0,
                    range: range_from_row(row, 9)?,
                },
            ))
        })
        .context("query semantic symbols")?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("collect semantic symbol search")
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
        _ => OccurrenceRole::Unresolved,
    }
}

fn parse_language(value: &str) -> LanguageId {
    match value {
        "rust" => LanguageId::Rust,
        "cpp" => LanguageId::Cpp,
        "objective-c" => LanguageId::ObjectiveC,
        "objective-cpp" => LanguageId::ObjectiveCpp,
        "javascript" => LanguageId::JavaScript,
        "typescript" => LanguageId::TypeScript,
        "python" => LanguageId::Python,
        "go" => LanguageId::Go,
        _ => LanguageId::Unknown,
    }
}
