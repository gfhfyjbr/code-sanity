use crate::config::{Layout, normalize_rel_path};
use crate::map::SpanMap;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

pub fn connect(layout: &Layout) -> Result<Connection> {
    let conn = Connection::open(&layout.db_path)
        .with_context(|| format!("open {}", layout.db_path.display()))?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .context("enable sqlite foreign keys")?;
    Ok(conn)
}

pub fn init_schema(conn: &Connection) -> Result<()> {
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

        create table if not exists patch_journal(
          id integer primary key,
          session_id text,
          agent text,
          sanitized_patch text not null,
          original_patch text not null,
          status text not null,
          created_at text not null
        );
        "#,
    )
    .context("initialize sqlite schema")
}

pub fn upsert_span_map(conn: &mut Connection, span_map: &SpanMap) -> Result<()> {
    let tx = conn.transaction().context("begin span map transaction")?;
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

    tx.commit().context("commit span map transaction")
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

pub fn remove_file(conn: &Connection, rel_path: &str) -> Result<()> {
    conn.execute("delete from files where rel_path = ?1", params![rel_path])
        .with_context(|| format!("remove stale db row for {rel_path}"))?;
    Ok(())
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
