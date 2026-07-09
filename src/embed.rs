//! Semantic index over the sanitized mirror.
//!
//! Follows the same incremental component model as the file index: every
//! mirror file owns its chunk/vector rows, and it is re-embedded only when its
//! **mirror content hash** or the **embed fingerprint** (model + chunker
//! version + chunk parameters) changes. A file that disappears takes its
//! vectors with it. Vectors are always computed from the sanitized mirror —
//! the same text agents already read — so the embedding endpoint never sees
//! real names.
//!
//! Locking: the removal sweep and the tracked-file snapshot run under one
//! exclusive workspace lock up front; each mirror file is then read under a
//! short-lived shared lock and embedding requests run unlocked so a slow
//! endpoint never starves writers. Every file's chunk rows commit in one
//! SQLite transaction under a brief exclusive lock that re-verifies the
//! mirror still matches the snapshot the vectors came from — a mirror that
//! changed mid-run is skipped (reported as stale) and reconciled by the next
//! `embed-index`.

use crate::config::{Config, Layout};
use crate::db;
use crate::llm::OpenAiClient;
use crate::lock::WorkspaceLock;
use crate::map::sha256_hex;
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::path::Path;

/// Bump when chunking behavior changes; forces re-embedding via the fingerprint.
pub const CHUNKER_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// 1-based, inclusive.
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
}

/// Deterministic sliding-window line chunker. Windows of `chunk_lines` lines
/// advance by `chunk_lines - overlap`; the final window is truncated at EOF.
pub fn chunk_text(content: &str, chunk_lines: usize, overlap: usize) -> Vec<Chunk> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let chunk_lines = chunk_lines.max(1);
    let step = chunk_lines - overlap.min(chunk_lines - 1);
    let mut chunks = Vec::new();
    let mut start = 0usize;
    loop {
        let end = (start + chunk_lines).min(lines.len());
        chunks.push(Chunk {
            start_line: start + 1,
            end_line: end,
            text: lines[start..end].join("\n"),
        });
        if end == lines.len() {
            break;
        }
        start += step;
    }
    chunks
}

/// Everything that invalidates stored vectors besides the content itself.
fn embed_fingerprint(config: &Config) -> String {
    sha256_hex(
        format!(
            "chunker-v{CHUNKER_VERSION}:model={}:chunk_lines={}:overlap={}",
            config.embeddings.model, config.embeddings.chunk_lines, config.embeddings.chunk_overlap
        )
        .as_bytes(),
    )
}

fn ensure_enabled(config: &Config) -> Result<()> {
    if !config.embeddings.enabled {
        bail!(
            "embeddings are disabled; set `embeddings.enabled = true` in \
             .code-sanity/config.toml (vectors are computed from the sanitized \
             mirror only)"
        );
    }
    Ok(())
}

fn client_for(config: &Config) -> Result<OpenAiClient> {
    OpenAiClient::new(
        &config.embeddings.base_url,
        &config.embeddings.api_key_env,
        config.embeddings.timeout_secs,
    )
}

#[derive(Debug, Clone, Default)]
pub struct EmbedReport {
    pub embedded: usize,
    pub unchanged: usize,
    pub removed: usize,
    /// Files whose mirror changed between snapshot and commit; their freshly
    /// computed vectors were discarded and the next run reconciles them.
    pub stale: usize,
    pub chunks: usize,
}

/// Bring the vector index up to date with the sanitized mirror. Incremental:
/// unchanged (content hash, fingerprint) pairs are skipped without an HTTP
/// request; untracked files lose their vectors.
pub fn embed_index(root: &Path) -> Result<EmbedReport> {
    let layout = Layout::new(root);
    layout.require_initialized()?;
    let config = Config::load_or_default(&layout)?;
    ensure_enabled(&config)?;
    let client = client_for(&config)?;
    let fingerprint = embed_fingerprint(&config);
    let mut conn = db::connect(&layout)?;

    let mut report = EmbedReport::default();

    // Snapshot the tracked set and drop vectors of files nothing tracks.
    // Sweep and snapshot share one exclusive lock (all DB writers hold it),
    // so the sweep can never delete rows a concurrent index just produced —
    // and schema init (a write) belongs under the same lock.
    let tracked: Vec<String> = {
        let _lock = WorkspaceLock::acquire(&layout)?;
        db::ensure_schema(&conn)?;
        crate::journal::ensure_no_interrupted_apply(&layout)?;
        let tracked = db::tracked_files(&conn)?;
        let tracked_set: BTreeSet<&String> = tracked.iter().collect();
        for rel in db::embedded_files(&conn)? {
            if !tracked_set.contains(&rel) {
                db::remove_embeddings(&conn, &rel)?;
                report.removed += 1;
            }
        }
        tracked
    };

    for rel in &tracked {
        // Short-lived shared lock: read a consistent (mirror content, prior
        // state) pair, then embed without blocking writers.
        let (content, prior) = {
            let _lock = WorkspaceLock::acquire_shared(&layout)?;
            let content = match std::fs::read_to_string(layout.mirror_dir.join(rel)) {
                Ok(content) => content,
                // Deleted mid-run: the deleter swept its rows under its own
                // exclusive lock. Anything else is a real error.
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    return Err(err).with_context(|| format!("read mirror file {rel}"));
                }
            };
            (content, db::embedding_state(&conn, rel)?)
        };
        let file_sha = sha256_hex(content.as_bytes());
        if prior.is_some_and(|(sha, fp)| sha == file_sha && fp == fingerprint) {
            report.unchanged += 1;
            continue;
        }

        let chunks: Vec<Chunk> = chunk_text(
            &content,
            config.embeddings.chunk_lines,
            config.embeddings.chunk_overlap,
        )
        .into_iter()
        .filter(|chunk| !chunk.text.trim().is_empty())
        .collect();

        let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(chunks.len());
        for batch in chunks.chunks(config.embeddings.batch_size.max(1)) {
            let inputs: Vec<String> = batch.iter().map(|chunk| chunk.text.clone()).collect();
            vectors.extend(
                client
                    .embed(&config.embeddings.model, &inputs)
                    .with_context(|| format!("embed {rel}"))?,
            );
        }

        let rows: Vec<(usize, usize, &str, Vec<u8>)> = chunks
            .iter()
            .zip(&vectors)
            .map(|(chunk, vector)| {
                (
                    chunk.start_line,
                    chunk.end_line,
                    chunk.text.as_str(),
                    vector_to_blob(vector),
                )
            })
            .collect();
        if commit_file_embeddings(&layout, &mut conn, rel, &file_sha, &fingerprint, &rows)? {
            report.embedded += 1;
            report.chunks += rows.len();
        } else {
            report.stale += 1;
        }
    }
    Ok(report)
}

/// Commit one file's freshly computed vectors under the exclusive workspace
/// lock, iff the mirror still matches the snapshot the vectors came from.
/// Returns false when the file changed or vanished mid-embed: stale vectors
/// must not overwrite rows describing newer content, and the next
/// `embed-index` reconciles the file.
fn commit_file_embeddings(
    layout: &Layout,
    conn: &mut rusqlite::Connection,
    rel: &str,
    file_sha: &str,
    fingerprint: &str,
    rows: &[(usize, usize, &str, Vec<u8>)],
) -> Result<bool> {
    let _lock = WorkspaceLock::acquire(layout)?;
    match std::fs::read_to_string(layout.mirror_dir.join(rel)) {
        Ok(current) if sha256_hex(current.as_bytes()) == file_sha => {
            db::replace_embeddings(conn, rel, file_sha, fingerprint, rows)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

#[derive(Debug, Clone)]
pub struct SemanticMatch {
    pub rel_path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub score: f32,
    /// First non-empty line of the chunk (sanitized), for display.
    pub preview: String,
}

/// Embed the query and return the top-k chunks by cosine similarity. Results
/// are sanitized mirror content only.
pub fn semantic_search(root: &Path, query: &str, k: usize) -> Result<Vec<SemanticMatch>> {
    if query.trim().is_empty() {
        bail!("semantic search query must not be empty");
    }
    let layout = Layout::new(root);
    layout.require_initialized()?;
    let config = Config::load_or_default(&layout)?;
    ensure_enabled(&config)?;
    let client = client_for(&config)?;
    let query_vector = client
        .embed(&config.embeddings.model, &[query.to_string()])?
        .pop()
        .context("embeddings endpoint returned no vector for the query")?;

    let _lock = WorkspaceLock::acquire_shared(&layout)?;
    let conn = db::connect(&layout)?;
    db::check_schema(&conn)?;

    // Top-k selection over a streamed scan: every vector is scored, but only
    // k (score, rowid) pairs are ever held; chunk texts are fetched afterwards
    // for the winners only, so memory stays O(k) instead of O(index).
    let k = k.max(1);
    let mut top: Vec<(f32, i64)> = Vec::with_capacity(k + 1);
    let mut scanned = 0usize;
    db::for_each_embedding_vector(&conn, |rowid, blob| {
        scanned += 1;
        let score = cosine_with_blob(&query_vector, blob);
        if top.len() == k && top.last().is_some_and(|(floor, _)| score <= *floor) {
            return;
        }
        let position = top
            .iter()
            .position(|(existing, _)| score > *existing)
            .unwrap_or(top.len());
        top.insert(position, (score, rowid));
        top.truncate(k);
    })?;
    if scanned == 0 {
        bail!("vector index is empty; run `code-sanity embed-index` first");
    }

    let rowids: Vec<i64> = top.iter().map(|(_, rowid)| *rowid).collect();
    let details = db::embedding_chunks_by_rowid(&conn, &rowids)?;
    Ok(top
        .iter()
        .zip(details)
        .map(|((score, _), (rel_path, start_line, end_line, text))| {
            let preview = text
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("")
                .chars()
                .take(160)
                .collect();
            SemanticMatch {
                rel_path,
                start_line,
                end_line,
                score: *score,
                preview,
            }
        })
        .collect())
}

pub fn vector_to_blob(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|v| v.to_le_bytes()).collect()
}

pub fn blob_to_vector(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        .collect()
}

/// Cosine similarity of a query vector against a stored little-endian f32
/// blob, decoded on the fly (no per-row Vec allocation during the scan).
fn cosine_with_blob(query: &[f32], blob: &[u8]) -> f32 {
    if query.is_empty() || blob.len() != query.len() * 4 {
        return 0.0;
    }
    let (mut dot, mut norm_q, mut norm_b) = (0.0f32, 0.0f32, 0.0f32);
    for (x, bytes) in query.iter().zip(blob.chunks_exact(4)) {
        let y = f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        dot += x * y;
        norm_q += x * x;
        norm_b += y * y;
    }
    if norm_q == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_q.sqrt() * norm_b.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunker_is_deterministic_and_overlapping() {
        let content = (1..=10).map(|i| format!("line{i}")).collect::<Vec<_>>();
        let content = content.join("\n");
        let chunks = chunk_text(&content, 4, 1);
        assert_eq!(chunks, chunk_text(&content, 4, 1));
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 4);
        // Step is 3: next window starts on line 4 (1 line of overlap).
        assert_eq!(chunks[1].start_line, 4);
        assert_eq!(chunks.last().unwrap().end_line, 10);
        // Every line is covered.
        for line in 1..=10usize {
            assert!(
                chunks
                    .iter()
                    .any(|c| c.start_line <= line && line <= c.end_line)
            );
        }
    }

    #[test]
    fn chunker_handles_small_and_empty_files() {
        assert!(chunk_text("", 60, 10).is_empty());
        let one = chunk_text("only line", 60, 10);
        assert_eq!(one.len(), 1);
        assert_eq!((one[0].start_line, one[0].end_line), (1, 1));
    }

    #[test]
    fn chunker_survives_overlap_ge_chunk_lines() {
        // Degenerate config must still terminate (overlap clamped).
        let content = "a\nb\nc\nd";
        let chunks = chunk_text(content, 2, 5);
        assert!(!chunks.is_empty());
        assert_eq!(chunks.last().unwrap().end_line, 4);
    }

    #[test]
    fn commit_refuses_stale_vectors_and_lands_fresh_ones() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        layout.ensure_dirs().unwrap();
        let mut conn = db::connect(&layout).unwrap();
        db::ensure_schema(&conn).unwrap();
        let snapshot = "snapshot content";
        let file_sha = sha256_hex(snapshot.as_bytes());
        let rows = vec![(1usize, 1usize, snapshot, vector_to_blob(&[1.0, 0.0]))];
        // The mirror moved on after the vectors were computed: refuse.
        std::fs::write(layout.mirror_dir.join("a.txt"), "newer content").unwrap();
        assert!(
            !commit_file_embeddings(&layout, &mut conn, "a.txt", &file_sha, "fp", &rows).unwrap()
        );
        assert!(db::embedding_state(&conn, "a.txt").unwrap().is_none());
        // Mirror matches the snapshot: commit lands.
        std::fs::write(layout.mirror_dir.join("a.txt"), snapshot).unwrap();
        assert!(
            commit_file_embeddings(&layout, &mut conn, "a.txt", &file_sha, "fp", &rows).unwrap()
        );
        assert_eq!(
            db::embedding_state(&conn, "a.txt").unwrap(),
            Some((file_sha, "fp".to_string()))
        );
    }

    #[test]
    fn vector_blob_roundtrip_and_cosine() {
        let vector = vec![0.5f32, -1.25, 3.0];
        assert_eq!(blob_to_vector(&vector_to_blob(&vector)), vector);
        assert!((cosine_with_blob(&vector, &vector_to_blob(&vector)) - 1.0).abs() < 1e-6);
        assert_eq!(
            cosine_with_blob(&[1.0, 0.0], &vector_to_blob(&[0.0, 1.0])),
            0.0
        );
        // Dimension mismatch and zero vectors score 0, never panic.
        assert_eq!(cosine_with_blob(&[1.0], &vector_to_blob(&[1.0, 2.0])), 0.0);
        assert_eq!(
            cosine_with_blob(&[0.0, 0.0], &vector_to_blob(&[0.0, 0.0])),
            0.0
        );
    }
}
