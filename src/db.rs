use crate::config::{Layout, normalize_rel_path};
use crate::map::SpanMap;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeSet;
use std::path::Path;

/// Current schema version (PRAGMA user_version). The database is fully derived
/// state (rebuilt by `index` from the real files and config), so migration is
/// drop-and-recreate for the derived tables; only `patch_journal` history is
/// preserved.
const SCHEMA_VERSION: i64 = 2;

pub fn connect(layout: &Layout) -> Result<Connection> {
    let conn = Connection::open(&layout.db_path)
        .with_context(|| format!("open {}", layout.db_path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(10))
        .context("set sqlite busy timeout")?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .context("enable sqlite WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .context("set sqlite synchronous")?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .context("enable sqlite foreign keys")?;
    Ok(conn)
}

/// Create or migrate the schema. The caller MUST hold the exclusive workspace
/// lock: migration drops derived tables, and even the create-if-missing batch
/// races a concurrent migrator. A current version returns immediately without
/// touching the header page, so misuse on a read path degrades to a no-op
/// rather than a write.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    let version = read_user_version(conn)?;
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    refuse_newer_schema(version)?;
    // Read-check-drop-create-stamp must be atomic: a crash or error mid-way
    // must leave either the old schema (old version stamp) or the new one.
    // DDL is transactional in SQLite, so one transaction covers all of it.
    let tx = conn
        .unchecked_transaction()
        .context("begin schema transaction")?;
    if version != 0 {
        // NOTE for the next SCHEMA_VERSION bump: every derived table that has
        // existed in ANY released version must be in this drop list —
        // embedding_state/embedding_chunks (added in v2) are absent only
        // because no pre-v2 database can contain them.
        tx.execute_batch(
            r#"
            drop table if exists spans;
            drop table if exists replacements;
            drop table if exists files;
            drop table if exists index_state;
            "#,
        )
        .context("drop outdated derived tables")?;
    }
    create_tables(&tx)?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)
        .context("set sqlite user_version")?;
    tx.commit().context("commit schema transaction")
}

/// Read-path guard: verify the schema is current without writing anything.
/// An outdated (or fresh, version 0) database is an error directing the user
/// to `index`, never an in-place migration — read paths hold only the shared
/// lock, and migration under a shared lock can drop tables under a writer.
pub fn check_schema(conn: &Connection) -> Result<()> {
    let version = read_user_version(conn)?;
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    refuse_newer_schema(version)?;
    bail!(
        "db.sqlite has schema version {version}, older than this build's \
         {SCHEMA_VERSION}; run `code-sanity index` to migrate (the database \
         is derived state)"
    );
}

fn read_user_version(conn: &Connection) -> Result<i64> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("read sqlite user_version")
}

/// True when `err`'s chain bottoms out in SQLite reporting a corrupt or
/// not-a-database file. The database is fully derived state, so the right
/// remedy is delete-and-reindex — the CLI catches this to say so instead of
/// surfacing a raw "database disk image is malformed".
pub fn is_corruption_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(ffi, _)) if matches!(
                ffi.code,
                rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
            )
        )
    })
}

fn refuse_newer_schema(version: i64) -> Result<()> {
    if version > SCHEMA_VERSION {
        // Never silently downgrade: `create table if not exists` would no-op
        // against unknown newer shapes and then stamp the old version over it.
        bail!(
            "db.sqlite has schema version {version}, newer than this build's \
             {SCHEMA_VERSION}; upgrade code-sanity or delete .code-sanity/db.sqlite \
             (the database is derived state; `code-sanity sync --force` rebuilds it \
             and stashes pending mirror edits under .code-sanity/journal/discarded/)"
        );
    }
    Ok(())
}

fn create_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        create table if not exists files(
          id integer primary key,
          rel_path text unique not null,
          original_hash text not null,
          sanitized_hash text not null,
          original_size integer not null,
          sanitized_size integer not null,
          language text,
          updated_at text not null
        );

        create table if not exists replacements(
          id integer primary key,
          file_id integer not null references files(id) on delete cascade,
          category text not null,
          original_text text not null,
          sanitized_text text not null,
          confidence real,
          policy_source text not null,
          stable_key text not null
        );
        create index if not exists replacements_file_id on replacements(file_id);

        create table if not exists spans(
          id integer primary key,
          file_id integer not null references files(id) on delete cascade,
          replacement_id integer,
          original_start integer not null,
          original_end integer not null,
          sanitized_start integer not null,
          sanitized_end integer not null,
          original_line_start integer not null,
          sanitized_line_start integer not null
        );
        create index if not exists spans_file_id on spans(file_id);

        create table if not exists index_state(
          rel_path text primary key,
          input_sha256 text not null,
          mtime_ns integer not null,
          size integer not null,
          logic_fingerprint text not null,
          protected_json text not null
        );

        create table if not exists patch_journal(
          id integer primary key,
          session_id text,
          agent text,
          sanitized_patch text not null,
          original_patch text not null,
          status text not null,
          created_at text not null
        );

        create table if not exists embedding_state(
          rel_path text primary key,
          file_sha256 text not null,
          fingerprint text not null
        );

        create table if not exists embedding_chunks(
          id integer primary key,
          rel_path text not null,
          chunk_index integer not null,
          start_line integer not null,
          end_line integer not null,
          text text not null,
          vector blob not null
        );
        create index if not exists embedding_chunks_rel on embedding_chunks(rel_path);
        "#,
    )
    .context("initialize sqlite schema")
}

/// Per-file incremental index state: input fingerprint (content hash plus the
/// mtime/size pre-check) and the logic fingerprint the file was last rendered
/// with, plus this file's protected identifier set (JSON array).
#[derive(Debug, Clone)]
pub struct IndexState {
    pub rel_path: String,
    pub input_sha256: String,
    pub mtime_ns: i64,
    pub size: i64,
    pub logic_fingerprint: String,
    pub protected_json: String,
}

impl IndexState {
    pub fn protected(&self) -> BTreeSet<String> {
        serde_json::from_str::<Vec<String>>(&self.protected_json)
            .unwrap_or_default()
            .into_iter()
            .collect()
    }
}

pub fn protected_to_json(protected: &BTreeSet<String>) -> String {
    serde_json::to_string(&protected.iter().collect::<Vec<_>>()).unwrap_or_else(|_| "[]".into())
}

pub fn all_index_states(conn: &Connection) -> Result<Vec<IndexState>> {
    let mut stmt = conn
        .prepare(
            "select rel_path, input_sha256, mtime_ns, size, logic_fingerprint, protected_json
             from index_state order by rel_path",
        )
        .context("prepare index_state query")?;
    let rows = stmt
        .query_map([], |row| {
            Ok(IndexState {
                rel_path: row.get(0)?,
                input_sha256: row.get(1)?,
                mtime_ns: row.get(2)?,
                size: row.get(3)?,
                logic_fingerprint: row.get(4)?,
                protected_json: row.get(5)?,
            })
        })
        .context("query index_state")?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("collect index_state rows")
}

/// Write mirror metadata + span rows + index_state in one transaction so a
/// file is indexed atomically (idempotent upserts keyed by rel_path).
pub fn upsert_indexed_file(
    conn: &mut Connection,
    span_map: &SpanMap,
    state: &IndexState,
) -> Result<()> {
    let tx = conn.transaction().context("begin index transaction")?;
    upsert_span_map_tx(&tx, span_map)?;
    tx.execute(
        r#"
        insert into index_state(rel_path, input_sha256, mtime_ns, size, logic_fingerprint, protected_json)
        values(?1, ?2, ?3, ?4, ?5, ?6)
        on conflict(rel_path) do update set
          input_sha256=excluded.input_sha256,
          mtime_ns=excluded.mtime_ns,
          size=excluded.size,
          logic_fingerprint=excluded.logic_fingerprint,
          protected_json=excluded.protected_json
        "#,
        params![
            state.rel_path,
            state.input_sha256,
            state.mtime_ns,
            state.size,
            state.logic_fingerprint,
            state.protected_json,
        ],
    )
    .context("upsert index_state row")?;
    tx.commit().context("commit index transaction")
}

/// Refresh only the input pre-check columns (content proved unchanged by hash,
/// but mtime/size moved).
pub fn touch_index_state(
    conn: &Connection,
    rel_path: &str,
    mtime_ns: i64,
    size: i64,
) -> Result<()> {
    conn.execute(
        "update index_state set mtime_ns = ?2, size = ?3 where rel_path = ?1",
        params![rel_path, mtime_ns, size],
    )
    .context("touch index_state row")?;
    Ok(())
}

fn upsert_span_map_tx(tx: &rusqlite::Transaction<'_>, span_map: &SpanMap) -> Result<()> {
    let rel_path = normalize_rel_path(Path::new(&span_map.rel_path));
    tx.execute(
        r#"
        insert into files(
          rel_path, original_hash, sanitized_hash, original_size, sanitized_size, language, updated_at
        )
        values(?1, ?2, ?3, ?4, ?5, ?6, ?7)
        on conflict(rel_path) do update set
          original_hash=excluded.original_hash,
          sanitized_hash=excluded.sanitized_hash,
          original_size=excluded.original_size,
          sanitized_size=excluded.sanitized_size,
          language=excluded.language,
          updated_at=excluded.updated_at
        "#,
        params![
            rel_path,
            span_map.original_hash,
            span_map.sanitized_hash,
            span_map.original_size as i64,
            span_map.sanitized_size as i64,
            span_map.language,
            span_map.updated_at,
        ],
    )
    .context("upsert files row")?;

    let file_id: i64 = tx
        .query_row(
            "select id from files where rel_path = ?1",
            params![rel_path],
            |row| row.get(0),
        )
        .context("load file id")?;

    tx.execute(
        "delete from replacements where file_id = ?1",
        params![file_id],
    )
    .context("clear replacements")?;
    tx.execute("delete from spans where file_id = ?1", params![file_id])
        .context("clear spans")?;

    for replacement in &span_map.replacements {
        tx.execute(
            r#"
            insert into replacements(
              file_id, category, original_text, sanitized_text, confidence, policy_source, stable_key
            )
            values(?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
            params![
                file_id,
                replacement.category,
                replacement.original_text,
                replacement.sanitized_text,
                replacement.confidence,
                replacement.policy_source,
                replacement.stable_key,
            ],
        )
        .context("insert replacement")?;
    }

    for span in &span_map.spans {
        tx.execute(
            r#"
            insert into spans(
              file_id, replacement_id, original_start, original_end, sanitized_start, sanitized_end,
              original_line_start, sanitized_line_start
            )
            values(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
            params![
                file_id,
                span.replacement_id.map(|id| id as i64),
                span.original_start as i64,
                span.original_end as i64,
                span.sanitized_start as i64,
                span.sanitized_end as i64,
                span.original_line_start as i64,
                span.sanitized_line_start as i64,
            ],
        )
        .context("insert span")?;
    }

    Ok(())
}

pub fn tracked_files(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("select rel_path from files order by rel_path")
        .context("prepare tracked files query")?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .context("query tracked files")?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("collect tracked files")
}

pub fn file_hashes(conn: &Connection, rel_path: &str) -> Result<Option<(String, String)>> {
    conn.query_row(
        "select original_hash, sanitized_hash from files where rel_path = ?1",
        params![rel_path],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .context("load file hashes")
}

/// Drop every row a file owns (files cascades to replacements/spans, plus
/// index_state and embeddings) in one transaction — a crash mid-removal must
/// not leave a files-less index_state row for `verify` to trip over.
pub fn remove_file(conn: &Connection, rel_path: &str) -> Result<()> {
    let tx = conn
        .unchecked_transaction()
        .context("begin remove transaction")?;
    tx.execute("delete from files where rel_path = ?1", params![rel_path])
        .with_context(|| format!("remove stale db row for {rel_path}"))?;
    tx.execute(
        "delete from index_state where rel_path = ?1",
        params![rel_path],
    )
    .with_context(|| format!("remove stale index_state row for {rel_path}"))?;
    remove_embeddings(&tx, rel_path)?;
    tx.commit().context("commit remove transaction")
}

/// The last embedded state of one mirror file: (mirror content sha256, embed
/// fingerprint — model + chunker version + chunk params).
pub fn embedding_state(conn: &Connection, rel_path: &str) -> Result<Option<(String, String)>> {
    conn.query_row(
        "select file_sha256, fingerprint from embedding_state where rel_path = ?1",
        params![rel_path],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .context("load embedding state")
}

/// Distinct embed fingerprints currently stored (empty when nothing is
/// embedded). Semantic search compares these against the current config
/// fingerprint before paying for a query embedding.
pub fn embedding_fingerprints(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("select distinct fingerprint from embedding_state order by fingerprint")
        .context("prepare embedding fingerprints query")?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .context("query embedding fingerprints")?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("collect embedding fingerprints")
}

pub fn embedded_files(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("select rel_path from embedding_state order by rel_path")
        .context("prepare embedded files query")?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .context("query embedded files")?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("collect embedded files")
}

/// Replace one file's chunks + state atomically (delete-then-insert keyed by
/// rel_path, mirroring how span rows are refreshed).
pub fn replace_embeddings(
    conn: &mut Connection,
    rel_path: &str,
    file_sha256: &str,
    fingerprint: &str,
    chunks: &[(usize, usize, &str, Vec<u8>)],
) -> Result<()> {
    let tx = conn.transaction().context("begin embeddings transaction")?;
    tx.execute(
        "delete from embedding_chunks where rel_path = ?1",
        params![rel_path],
    )
    .context("clear embedding chunks")?;
    for (index, (start_line, end_line, text, vector)) in chunks.iter().enumerate() {
        tx.execute(
            r#"
            insert into embedding_chunks(rel_path, chunk_index, start_line, end_line, text, vector)
            values(?1, ?2, ?3, ?4, ?5, ?6)
            "#,
            params![
                rel_path,
                index as i64,
                *start_line as i64,
                *end_line as i64,
                text,
                vector,
            ],
        )
        .context("insert embedding chunk")?;
    }
    tx.execute(
        r#"
        insert into embedding_state(rel_path, file_sha256, fingerprint)
        values(?1, ?2, ?3)
        on conflict(rel_path) do update set
          file_sha256=excluded.file_sha256,
          fingerprint=excluded.fingerprint
        "#,
        params![rel_path, file_sha256, fingerprint],
    )
    .context("upsert embedding state")?;
    tx.commit().context("commit embeddings transaction")
}

pub fn remove_embeddings(conn: &Connection, rel_path: &str) -> Result<()> {
    conn.execute(
        "delete from embedding_chunks where rel_path = ?1",
        params![rel_path],
    )
    .with_context(|| format!("remove embedding chunks for {rel_path}"))?;
    conn.execute(
        "delete from embedding_state where rel_path = ?1",
        params![rel_path],
    )
    .with_context(|| format!("remove embedding state for {rel_path}"))?;
    Ok(())
}

/// One stored chunk: (rel_path, start_line, end_line, text, vector blob).
pub type EmbeddedChunk = (String, usize, usize, String, Vec<u8>);

pub fn all_embedding_chunks(conn: &Connection) -> Result<Vec<EmbeddedChunk>> {
    let mut stmt = conn
        .prepare(
            "select rel_path, start_line, end_line, text, vector
             from embedding_chunks order by rel_path, chunk_index",
        )
        .context("prepare embedding chunks query")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? as usize,
                row.get::<_, i64>(2)? as usize,
                row.get::<_, String>(3)?,
                row.get::<_, Vec<u8>>(4)?,
            ))
        })
        .context("query embedding chunks")?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("collect embedding chunks")
}

/// Stream `(rowid, vector)` for every stored chunk without materializing
/// chunk texts; semantic search's top-k selection scores rows as they pass,
/// so memory stays O(1) rows instead of O(index).
pub fn for_each_embedding_vector(
    conn: &Connection,
    mut visit: impl FnMut(i64, &[u8]),
) -> Result<()> {
    let mut stmt = conn
        .prepare("select rowid, vector from embedding_chunks")
        .context("prepare embedding vectors query")?;
    let mut rows = stmt.query([]).context("query embedding vectors")?;
    while let Some(row) = rows.next().context("read embedding vector row")? {
        let rowid: i64 = row.get(0).context("read embedding rowid")?;
        let value = row.get_ref(1).context("read embedding vector")?;
        let vector = value.as_blob().context("embedding vector is not a blob")?;
        visit(rowid, vector);
    }
    Ok(())
}

/// Fetch the display fields for chosen chunks, in the given rowid order.
pub fn embedding_chunks_by_rowid(
    conn: &Connection,
    rowids: &[i64],
) -> Result<Vec<(String, usize, usize, String)>> {
    let mut stmt = conn
        .prepare(
            "select rel_path, start_line, end_line, text
             from embedding_chunks where rowid = ?1",
        )
        .context("prepare embedding chunk lookup")?;
    let mut out = Vec::with_capacity(rowids.len());
    for rowid in rowids {
        let row = stmt
            .query_row(params![rowid], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)? as usize,
                    row.get::<_, i64>(2)? as usize,
                    row.get::<_, String>(3)?,
                ))
            })
            .with_context(|| format!("load embedding chunk rowid {rowid}"))?;
        out.push(row);
    }
    Ok(out)
}

pub fn insert_journal_row(
    conn: &Connection,
    session_id: Option<&str>,
    agent: Option<&str>,
    sanitized_patch: &str,
    original_patch: &str,
    status: &str,
    created_at: &str,
) -> Result<()> {
    conn.execute(
        r#"
        insert into patch_journal(
          session_id, agent, sanitized_patch, original_patch, status, created_at
        )
        values(?1, ?2, ?3, ?4, ?5, ?6)
        "#,
        params![
            session_id,
            agent,
            sanitized_patch,
            original_patch,
            status,
            created_at
        ],
    )
    .context("insert patch journal row")?;
    Ok(())
}

/// Delete the oldest `patch_journal` rows beyond `keep` (0 = unlimited);
/// the history table stores full patch texts and grows without bound
/// otherwise. Returns how many rows were removed.
pub fn prune_journal_rows(conn: &Connection, keep: u64) -> Result<usize> {
    if keep == 0 {
        return Ok(0);
    }
    let removed = conn
        .execute(
            r#"
            delete from patch_journal
            where id not in (select id from patch_journal order by id desc limit ?1)
            "#,
            params![keep],
        )
        .context("prune patch journal rows")?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_journal_rows_keeps_newest() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        layout.ensure_dirs().unwrap();
        let conn = connect(&layout).unwrap();
        ensure_schema(&conn).unwrap();
        for i in 0..5 {
            insert_journal_row(&conn, None, None, &format!("patch-{i}"), "", "success", "t")
                .unwrap();
        }
        assert_eq!(prune_journal_rows(&conn, 2).unwrap(), 3);
        let kept: Vec<String> = conn
            .prepare("select sanitized_patch from patch_journal order by id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(kept, vec!["patch-3".to_string(), "patch-4".to_string()]);
        // 0 disables pruning.
        assert_eq!(prune_journal_rows(&conn, 0).unwrap(), 0);
    }
}
