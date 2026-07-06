//! Incremental indexer.
//!
//! Every file is a component owning its mirror file, span map, and db rows.
//! A file is re-rendered only when its input fingerprint (content sha256 with
//! an mtime/size pre-check) or the logic fingerprint (dictionary, registry,
//! allow/deny lists, salt, sanitizer behavior version, and the repo-wide
//! protected symbol table) changes; a file that disappeared takes its targets
//! with it. Each file commits in a single transaction with idempotent upserts.

use crate::config::{Config, Layout, normalize_rel_path, rel_path};
use crate::db::{self, IndexState};
use crate::lock::WorkspaceLock;
use crate::map::{SpanMap, load_span_map, sha256_hex};
use crate::sanitize::{
    SANITIZER_BEHAVIOR_VERSION, collect_protected_identifiers, sanitize_content,
};
use anyhow::{Context, Result};
use ignore::{DirEntry, WalkBuilder};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct IndexReport {
    pub indexed: usize,
    pub skipped: usize,
    pub removed: usize,
    pub unchanged: usize,
    /// Mirror files left untouched because they hold a pending agent edit
    /// (mirror on disk differs from the last indexed sanitized hash).
    pub pending: usize,
    /// Durable copies of pending agent edits that a force pass discarded.
    pub stashed: Vec<String>,
}

pub fn init_workspace(root: &Path) -> Result<Layout> {
    let layout = Layout::new(root);
    layout.ensure_dirs()?;
    let mut config = Config::default();
    config.salt = crate::config::random_salt();
    config.write_if_missing(&layout)?;
    ensure_gitignore_entry(root, ".code-sanity/")?;
    let conn = db::connect(&layout)?;
    db::init_schema(&conn)?;
    Ok(layout)
}

pub fn index_workspace(root: &Path) -> Result<IndexReport> {
    let layout = init_workspace(root)?;
    let _lock = WorkspaceLock::acquire(&layout)?;
    crate::journal::ensure_no_interrupted_apply(&layout)?;
    index_workspace_locked(root, &layout)
}

/// Full index pass that also resets mirror files holding pending (or planted)
/// edits back to sanitize(real). The recovery path when a mirror was tampered
/// with or an agent edit must be discarded.
pub fn index_workspace_force(root: &Path) -> Result<IndexReport> {
    let layout = init_workspace(root)?;
    let _lock = WorkspaceLock::acquire(&layout)?;
    crate::journal::ensure_no_interrupted_apply(&layout)?;
    index_workspace_locked_inner(root, &layout, true)
}

/// Full index pass; the caller must hold the workspace lock.
pub(crate) fn index_workspace_locked(root: &Path, layout: &Layout) -> Result<IndexReport> {
    index_workspace_locked_inner(root, layout, false)
}

fn index_workspace_locked_inner(
    root: &Path,
    layout: &Layout,
    force_mirror: bool,
) -> Result<IndexReport> {
    let config = Config::load_or_default(layout)?;
    let mut conn = db::connect(layout)?;
    db::init_schema(&conn)?;

    let states: BTreeMap<String, IndexState> = db::all_index_states(&conn)?
        .into_iter()
        .map(|state| (state.rel_path.clone(), state))
        .collect();

    struct Candidate {
        rel: PathBuf,
        rel_string: String,
        mtime_ns: i64,
        size: i64,
        /// Content and sha are loaded only when the mtime/size pre-check missed.
        content: Option<String>,
        sha: Option<String>,
        protected: BTreeSet<String>,
        fast: bool,
    }

    let mut report = IndexReport::default();
    let mut candidates = Vec::new();
    for entry in walk_repo(root, &config)? {
        let entry = entry?;
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            continue;
        }
        let rel = rel_path(root, entry.path())?;
        let metadata = fs::metadata(entry.path())
            .with_context(|| format!("metadata {}", entry.path().display()))?;
        if should_skip_file(&rel, entry.path(), &metadata, &config)? {
            report.skipped += 1;
            continue;
        }
        let rel_string = normalize_rel_path(&rel);
        let mtime = mtime_ns(&metadata);
        let size = metadata.len() as i64;

        if let Some(state) = states.get(&rel_string)
            && state.mtime_ns == mtime
            && state.size == size
        {
            candidates.push(Candidate {
                rel,
                rel_string,
                mtime_ns: mtime,
                size,
                content: None,
                sha: None,
                protected: state.protected(),
                fast: true,
            });
            continue;
        }

        let content = fs::read_to_string(entry.path())
            .with_context(|| format!("read source {}", entry.path().display()))?;
        let sha = sha256_hex(content.as_bytes());
        let protected = collect_protected_identifiers(&content);
        candidates.push(Candidate {
            rel,
            rel_string,
            mtime_ns: mtime,
            size,
            content: Some(content),
            sha: Some(sha),
            protected,
            fast: false,
        });
    }

    let union: BTreeSet<String> = candidates
        .iter()
        .flat_map(|candidate| candidate.protected.iter().cloned())
        .collect();
    let logic = logic_fingerprint(&config, &union);

    let mut seen = BTreeSet::new();
    for candidate in &candidates {
        seen.insert(candidate.rel_string.clone());
        let state = states.get(&candidate.rel_string);

        if !force_mirror
            && candidate.fast
            && let Some(state) = state
            && state.logic_fingerprint == logic
            && layout.mirror_dir.join(&candidate.rel).exists()
            && layout.map_path(&candidate.rel).exists()
        {
            report.unchanged += 1;
            continue;
        }

        let content = match &candidate.content {
            Some(content) => content.clone(),
            None => fs::read_to_string(root.join(&candidate.rel))
                .with_context(|| format!("read source {}", candidate.rel.display()))?,
        };
        let sha = candidate
            .sha
            .clone()
            .unwrap_or_else(|| sha256_hex(content.as_bytes()));
        let protected = if candidate.fast {
            collect_protected_identifiers(&content)
        } else {
            candidate.protected.clone()
        };

        // Content proved unchanged by hash and logic matches: refresh the
        // mtime/size pre-check columns without re-rendering.
        if !force_mirror
            && let Some(state) = state
            && state.input_sha256 == sha
            && state.logic_fingerprint == logic
            && layout.mirror_dir.join(&candidate.rel).exists()
            && layout.map_path(&candidate.rel).exists()
        {
            db::touch_index_state(
                &conn,
                &candidate.rel_string,
                candidate.mtime_ns,
                candidate.size,
            )?;
            report.unchanged += 1;
            continue;
        }

        let state_row = IndexState {
            rel_path: candidate.rel_string.clone(),
            input_sha256: sha,
            mtime_ns: candidate.mtime_ns,
            size: candidate.size,
            logic_fingerprint: logic.clone(),
            protected_json: db::protected_to_json(&protected),
        };
        let (outcome, _, stashed) = render_and_store(
            root,
            layout,
            &config,
            &mut conn,
            &candidate.rel,
            &content,
            state_row,
            &union,
            force_mirror,
        )?;
        if let Some(stash) = stashed {
            report.stashed.push(stash.display().to_string());
        }
        match outcome {
            FileOutcome::Updated => report.indexed += 1,
            FileOutcome::Unchanged => report.unchanged += 1,
            FileOutcome::PendingSkipped => report.pending += 1,
        }
    }

    let mut stale: BTreeSet<String> = db::tracked_files(&conn)?.into_iter().collect();
    stale.extend(states.keys().cloned());
    for tracked in stale {
        if !seen.contains(&tracked) {
            db::remove_file(&conn, &tracked)?;
            remove_if_exists(layout.mirror_dir.join(&tracked))?;
            remove_if_exists(layout.map_path(Path::new(&tracked)))?;
            report.removed += 1;
        }
    }

    Ok(report)
}

/// Index one file with the workspace lock held by this call. Force-writes the
/// mirror (used by the patch bridge and proposal approval, where the mirror
/// must be reset to sanitize(real)).
pub fn index_single_file(root: &Path, rel: &Path) -> Result<SpanMap> {
    let layout = init_workspace(root)?;
    let _lock = WorkspaceLock::acquire(&layout)?;
    crate::journal::ensure_no_interrupted_apply(&layout)?;
    let indexed = index_single_file_locked(root, &layout, rel, true)?;
    Ok(indexed.span_map)
}

/// Sync one path (used by agent hooks): pending mirror edits are preserved.
pub fn sync_single_file(root: &Path, rel: &Path) -> Result<IndexReport> {
    let layout = init_workspace(root)?;
    let _lock = WorkspaceLock::acquire(&layout)?;
    crate::journal::ensure_no_interrupted_apply(&layout)?;
    let mut report = IndexReport::default();
    if !root.join(rel).exists() {
        // The real file is gone: drop its targets.
        let conn = db::connect(&layout)?;
        db::init_schema(&conn)?;
        let rel_string = normalize_rel_path(rel);
        db::remove_file(&conn, &rel_string)?;
        remove_if_exists(layout.mirror_dir.join(rel))?;
        remove_if_exists(layout.map_path(rel))?;
        report.removed += 1;
        return Ok(report);
    }
    match index_single_file_locked(root, &layout, rel, false)?.outcome {
        FileOutcome::Updated => report.indexed += 1,
        FileOutcome::Unchanged => report.unchanged += 1,
        FileOutcome::PendingSkipped => report.pending += 1,
    }
    Ok(report)
}

pub(crate) enum FileOutcome {
    Updated,
    Unchanged,
    PendingSkipped,
}

/// Result of indexing one file under the caller's lock.
pub(crate) struct SingleFileIndex {
    pub(crate) outcome: FileOutcome,
    pub(crate) span_map: SpanMap,
    /// The file's protected identifier set changed: other files' renderings
    /// are stale and the caller owes a reconverge pass.
    pub(crate) protected_changed: bool,
    /// Durable copy of a pending mirror edit that a force reset displaced.
    pub(crate) stashed: Option<PathBuf>,
}

/// Index one file; the caller must already hold the workspace lock.
pub(crate) fn index_single_file_locked(
    root: &Path,
    layout: &Layout,
    rel: &Path,
    force_mirror: bool,
) -> Result<SingleFileIndex> {
    let config = Config::load_or_default(layout)?;
    let mut conn = db::connect(layout)?;
    db::init_schema(&conn)?;

    let rel_string = normalize_rel_path(rel);
    let source_path = root.join(rel);
    let (content, metadata) = read_with_stat(&source_path)?;
    let sha = sha256_hex(content.as_bytes());
    let fresh_protected = collect_protected_identifiers(&content);

    let states = db::all_index_states(&conn)?;
    let old_protected = states
        .iter()
        .find(|state| state.rel_path == rel_string)
        .map(|state| state.protected())
        .unwrap_or_default();
    let protected_changed = old_protected != fresh_protected;

    let mut union: BTreeSet<String> = states
        .iter()
        .filter(|state| state.rel_path != rel_string)
        .flat_map(|state| state.protected())
        .collect();
    union.extend(fresh_protected.iter().cloned());
    let logic = logic_fingerprint(&config, &union);

    let state_row = IndexState {
        rel_path: rel_string,
        input_sha256: sha,
        mtime_ns: mtime_ns(&metadata),
        size: metadata.len() as i64,
        logic_fingerprint: logic,
        protected_json: db::protected_to_json(&fresh_protected),
    };
    let (outcome, span_map, stashed) = render_and_store(
        root,
        layout,
        &config,
        &mut conn,
        rel,
        &content,
        state_row,
        &union,
        force_mirror,
    )?;
    Ok(SingleFileIndex {
        outcome,
        span_map,
        protected_changed,
        stashed,
    })
}

/// Read a file as a consistent (content, metadata) snapshot: stat, read,
/// re-stat, retrying when the file changed mid-read. Recording a stat NEWER
/// than the content would wedge the incremental index on a stale render (the
/// mtime/size pre-check would keep matching).
fn read_with_stat(path: &Path) -> Result<(String, fs::Metadata)> {
    for _ in 0..3 {
        let before = fs::metadata(path).with_context(|| format!("metadata {}", path.display()))?;
        let content =
            fs::read_to_string(path).with_context(|| format!("read source {}", path.display()))?;
        let after = fs::metadata(path).with_context(|| format!("metadata {}", path.display()))?;
        if mtime_ns(&before) == mtime_ns(&after) && before.len() == after.len() {
            return Ok((content, after));
        }
    }
    anyhow::bail!(
        "{} keeps changing while being read; retry later",
        path.display()
    )
}

/// Re-render everything whose rendering became stale after a policy or
/// protected-set change; the caller must hold the exclusive lock. Currently a
/// full incremental pass — the single swap point for a narrower dirty-set
/// reconvergence later.
pub(crate) fn reconverge_workspace(root: &Path, layout: &Layout) -> Result<IndexReport> {
    index_workspace_locked(root, layout)
}

/// The repo-wide protected identifier union as last indexed.
pub(crate) fn stored_protected_union(conn: &rusqlite::Connection) -> Result<BTreeSet<String>> {
    Ok(db::all_index_states(conn)?
        .iter()
        .flat_map(|state| state.protected())
        .collect())
}

/// The stored union with one file's protected set replaced by `fresh` —
/// the union that will be in effect after that file changes to new content.
pub(crate) fn stored_protected_union_with_override(
    conn: &rusqlite::Connection,
    rel_string: &str,
    fresh: &BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    let mut union: BTreeSet<String> = db::all_index_states(conn)?
        .iter()
        .filter(|state| state.rel_path != rel_string)
        .flat_map(|state| state.protected())
        .collect();
    union.extend(fresh.iter().cloned());
    Ok(union)
}

pub(crate) fn logic_fingerprint(config: &Config, protected_union: &BTreeSet<String>) -> String {
    let mut allowlist = config.sanitizer.allowlist.clone();
    allowlist.sort();
    let mut denylist = config.sanitizer.denylist.clone();
    denylist.sort();
    let payload = serde_json::json!({
        "behavior": SANITIZER_BEHAVIOR_VERSION,
        "salt": config.salt,
        "dictionary": config.sanitizer.dictionary,
        "alias_registry": config.sanitizer.alias_registry,
        "allowlist": allowlist,
        "denylist": denylist,
        "protected": protected_union,
    });
    sha256_hex(payload.to_string().as_bytes())
}

#[allow(clippy::too_many_arguments)]
fn render_and_store(
    root: &Path,
    layout: &Layout,
    config: &Config,
    conn: &mut rusqlite::Connection,
    rel: &Path,
    content: &str,
    state: IndexState,
    protected_union: &BTreeSet<String>,
    force_mirror: bool,
) -> Result<(FileOutcome, SpanMap, Option<PathBuf>)> {
    let _ = root;
    let mut rendered = sanitize_content(rel, content, config, protected_union)
        .with_context(|| format!("sanitize {}", rel.display()))?;

    let mirror_path = layout.mirror_dir.join(rel);
    let map_path = layout.map_path(rel);
    let old_mirror = fs::read_to_string(&mirror_path).ok();
    let db_sanitized_hash = db::file_hashes(conn, &state.rel_path)?.map(|(_, hash)| hash);

    // Pending-edit protection: the agent edited this mirror file and the edit
    // has not been projected yet (mirror on disk differs from the last indexed
    // sanitized hash). Sync must not clobber it; only the patch bridge (force)
    // may reset the mirror to sanitize(real).
    //
    // Self-heal: a mirror that already equals the fresh render is converged
    // content with a stale db row (a crash between the mirror write and the db
    // commit), not a pending edit — fall through so the upsert repairs the row.
    if !force_mirror
        && let Some(old) = old_mirror.as_deref()
        && old != rendered.sanitized
        && let Some(hash) = db_sanitized_hash.as_deref()
        && sha256_hex(old.as_bytes()) != hash
    {
        let previous = load_span_map(&map_path).unwrap_or(rendered.span_map);
        return Ok((FileOutcome::PendingSkipped, previous, None));
    }

    // A force reset is about to discard an un-projected agent edit: keep a
    // durable copy under journal/discarded/ before overwriting it.
    let mut stashed = None;
    if force_mirror
        && let Some(old) = old_mirror.as_deref()
        && old != rendered.sanitized
        && db_sanitized_hash
            .as_deref()
            .is_none_or(|hash| sha256_hex(old.as_bytes()) != hash)
    {
        let stash_path = layout
            .journal_dir
            .join("discarded")
            .join(crate::journal::new_journal_id())
            .join(rel);
        crate::fsutil::atomic_write_sync(&stash_path, old)
            .with_context(|| format!("stash pending mirror edit for {}", rel.display()))?;
        log::info!(
            "resetting a pending mirror edit for {}; copy kept at {}",
            rel.display(),
            stash_path.display()
        );
        stashed = Some(stash_path);
    }

    let old_map_raw = fs::read_to_string(&map_path).ok();
    if let Some(old_map) = old_map_raw
        .as_deref()
        .and_then(|raw| serde_json::from_str::<SpanMap>(raw).ok())
        && old_map.original_hash == rendered.span_map.original_hash
        && old_map.sanitized_hash == rendered.span_map.sanitized_hash
        && old_map.replacements == rendered.span_map.replacements
        && old_map.spans == rendered.span_map.spans
    {
        rendered.span_map.updated_at = old_map.updated_at;
    }
    let next_map = serde_json::to_string_pretty(&rendered.span_map).context("serialize map")?;
    let unchanged = old_mirror.as_deref() == Some(rendered.sanitized.as_str())
        && old_map_raw.as_deref() == Some(next_map.as_str());

    crate::fsutil::atomic_write_if_changed(&mirror_path, &rendered.sanitized)
        .with_context(|| format!("write mirror {}", mirror_path.display()))?;
    crate::fsutil::atomic_write_if_changed(&map_path, &next_map)
        .with_context(|| format!("write map {}", map_path.display()))?;
    db::upsert_indexed_file(conn, &rendered.span_map, &state)?;

    Ok((
        if unchanged {
            FileOutcome::Unchanged
        } else {
            FileOutcome::Updated
        },
        rendered.span_map,
        stashed,
    ))
}

fn mtime_ns(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as i64)
        .unwrap_or(0)
}

fn walk_repo(
    root: &Path,
    config: &Config,
) -> Result<Vec<std::result::Result<DirEntry, ignore::Error>>> {
    let extra_dirs = config
        .ignore
        .extra_dirs
        .iter()
        .chain(config.ignore.generated_dirs.iter())
        .cloned()
        .collect::<BTreeSet<_>>();
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .parents(false)
        .require_git(false)
        .filter_entry(move |entry| {
            let name = entry.file_name().to_string_lossy();
            !extra_dirs.contains(name.as_ref())
        })
        .build()
        .collect::<Vec<_>>();
    Ok(walker)
}

fn should_skip_file(
    rel: &Path,
    path: &Path,
    metadata: &fs::Metadata,
    config: &Config,
) -> Result<bool> {
    let Some(file_name) = rel.file_name().and_then(|name| name.to_str()) else {
        return Ok(true);
    };
    if config.ignore.lockfiles.iter().any(|lock| lock == file_name) {
        return Ok(true);
    }
    if metadata.len() > config.ignore.max_file_bytes {
        return Ok(true);
    }
    is_binary(path)
}

fn is_binary(path: &Path) -> Result<bool> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut buf = [0u8; 8192];
    let read = file
        .read(&mut buf)
        .with_context(|| format!("read {}", path.display()))?;
    if buf[..read].contains(&0) {
        return Ok(true);
    }
    match std::str::from_utf8(&buf[..read]) {
        Ok(_) => Ok(false),
        // An incomplete multibyte sequence at the end of a full probe merely
        // straddles the probe boundary; only a sequence invalid mid-buffer (or
        // truncated at true EOF) marks the file binary.
        Err(err) => Ok(!(err.error_len().is_none() && read == buf.len())),
    }
}

fn remove_if_exists(path: PathBuf) -> Result<()> {
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("remove {}", path.display())),
    }
}

fn ensure_gitignore_entry(root: &Path, entry: &str) -> Result<()> {
    let path = root.join(".gitignore");
    // Only a missing file means "empty": treating a read error as empty would
    // clobber the user's .gitignore on the rewrite below.
    let current = match fs::read_to_string(&path) {
        Ok(current) => current,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    if current
        .lines()
        .any(|line| line.trim() == entry.trim_end_matches('/'))
        || current.lines().any(|line| line.trim() == entry)
    {
        return Ok(());
    }

    let mut next = current;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(entry);
    next.push('\n');
    crate::fsutil::atomic_write_sync(&path, &next)
        .with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multibyte_char_straddling_probe_boundary_is_not_binary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boundary.txt");
        // 8191 ASCII bytes then a 2-byte char: its second byte falls outside
        // the 8192-byte probe window.
        let mut content = "a".repeat(8191);
        content.push('\u{e9}');
        content.push_str(" tail");
        fs::write(&path, &content).unwrap();
        assert!(!is_binary(&path).unwrap());
    }

    #[test]
    fn nul_bytes_and_invalid_utf8_mid_buffer_are_binary() {
        let dir = tempfile::tempdir().unwrap();
        let nul = dir.path().join("nul.bin");
        fs::write(&nul, b"abc\0def").unwrap();
        assert!(is_binary(&nul).unwrap());
        let bad = dir.path().join("bad.bin");
        fs::write(&bad, [b'a', 0xC3, b'(', b'b']).unwrap();
        assert!(is_binary(&bad).unwrap());
    }

    #[test]
    fn truncated_sequence_at_true_eof_is_binary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trunc.bin");
        let mut bytes = b"abc".to_vec();
        bytes.push(0xC3);
        fs::write(&path, bytes).unwrap();
        assert!(is_binary(&path).unwrap());
    }
}
