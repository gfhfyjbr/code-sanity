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
use crate::path_projection::{PATH_PROJECTION_VERSION, PathProjection, project_rel_path};
use crate::sanitize::{
    SANITIZER_BEHAVIOR_VERSION, collect_external_identifiers, collect_protected_identifiers,
    sanitize_content,
};
use anyhow::{Context, Result};
use ignore::{DirEntry, WalkBuilder};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, serde::Serialize)]
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
    /// Files skipped with a reason (unreadable, invalid UTF-8 past the binary
    /// probe, metadata/walk errors). Their previous index state, mirror, and
    /// map are preserved until they become indexable again; `verify` is the
    /// strict gate that keeps reporting them.
    /// Serialized as `[{"path", "reason"}]` objects, not bare 2-tuples: the
    /// `--json` contract must stay self-describing.
    #[serde(serialize_with = "serialize_path_reason_pairs")]
    pub errors: Vec<(String, String)>,
    /// Symlinked entries encountered; never followed (following could escape
    /// the repo boundary), never indexed.
    pub skipped_symlinks: usize,
    /// Versioned AST/semantic index refreshed from the same real-file
    /// snapshot. Unsupported languages are recorded as explicit read-only
    /// documents instead of being rewritten lexically.
    pub semantic: crate::semantic_store::SemanticIndexReport,
}

fn serialize_path_reason_pairs<S: serde::Serializer>(
    pairs: &[(String, String)],
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    #[derive(serde::Serialize)]
    struct PathReason<'a> {
        path: &'a str,
        reason: &'a str,
    }
    serializer.collect_seq(
        pairs
            .iter()
            .map(|(path, reason)| PathReason { path, reason }),
    )
}

/// Ensure dirs, acquire the exclusive lock, then — under it — write the
/// default config/salt if missing, ensure the `.gitignore` entry, and ensure
/// the DB schema. Returns the held lock so the caller continues under the
/// same acquisition: re-acquiring from the same process would self-deadlock
/// (flock, see lock.rs), and init's read-modify-writes (salt, .gitignore,
/// schema migration) must not race a concurrent first run.
pub(crate) fn init_workspace_locked(root: &Path) -> Result<(Layout, WorkspaceLock)> {
    let layout = Layout::new(root);
    layout.ensure_dirs()?;
    let lock = WorkspaceLock::acquire(&layout)?;
    if !layout.config_path.exists() {
        // A missing config on a workspace with initialized state means the
        // config was LOST, not never written: regenerating defaults here
        // would replace the salt and drop the denylist/alias registry, then
        // re-render the whole mirror without the user's policy. Hard error;
        // the loader guard alone fires too late for init/index, which write
        // the default config before anything loads it.
        if layout.has_initialized_state() {
            return Err(crate::config::missing_config_error(&layout));
        }
        let mut config = Config::default();
        config.salt = crate::config::random_salt();
        // Default aliases carry a salted suffix: derive them from the REAL
        // salt, not the deterministic stub Config::default() uses.
        config.sanitizer.dictionary = crate::config::default_dictionary(&config.salt);
        config.save(&layout)?;
    }
    ensure_gitignore_entry(root, ".code-sanity/")?;
    ensure_gitignore_entry(root, ".env")?;
    let conn = db::connect(&layout)?;
    db::ensure_schema(&conn)?;
    Ok((layout, lock))
}

pub fn init_workspace(root: &Path) -> Result<Layout> {
    let (layout, _lock) = init_workspace_locked(root)?;
    Ok(layout)
}

pub fn index_workspace(root: &Path) -> Result<IndexReport> {
    let (layout, _lock) = init_workspace_locked(root)?;
    crate::journal::ensure_no_interrupted_apply(&layout)?;
    index_workspace_locked(root, &layout)
}

/// Full index pass that also resets mirror files holding pending (or planted)
/// edits back to sanitize(real). The recovery path when a mirror was tampered
/// with or an agent edit must be discarded.
pub fn index_workspace_force(root: &Path) -> Result<IndexReport> {
    let (layout, _lock) = init_workspace_locked(root)?;
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
    db::ensure_schema(&conn)?;

    let states: BTreeMap<String, IndexState> = db::all_index_states(&conn)?
        .into_iter()
        .map(|state| (state.rel_path.clone(), state))
        .collect();

    struct Candidate {
        rel: PathBuf,
        projected: PathBuf,
        rel_string: String,
        mtime_ns: i64,
        size: i64,
        /// Content and sha are loaded only when the mtime/size pre-check missed.
        content: Option<String>,
        sha: Option<String>,
        protected: BTreeSet<String>,
        external: BTreeSet<String>,
        fast: bool,
    }

    let mut report = IndexReport::default();
    let mut candidates = Vec::new();
    // `seen` is shared with the stale sweep below and must include files that
    // errored: an unreadable file keeps its previous mirror/map/db row instead
    // of being swept as if it were deleted.
    let mut seen = BTreeSet::new();
    let mut first_entry = true;
    for entry in walk_repo(root, &config)? {
        let is_root_probe = std::mem::take(&mut first_entry);
        let entry = match entry {
            Ok(entry) => entry,
            // The first walk item is the root itself: an unreadable root is a
            // workspace-level failure, not a per-file skip.
            Err(err) if is_root_probe => {
                return Err(err).with_context(|| format!("walk repo root {}", root.display()));
            }
            Err(err) => {
                report
                    .errors
                    .push(("<walk>".to_string(), format!("walk: {err}")));
                continue;
            }
        };
        let file_type = entry.file_type();
        if file_type.is_some_and(|file_type| file_type.is_symlink()) {
            report.skipped_symlinks += 1;
            continue;
        }
        if !file_type.is_some_and(|file_type| file_type.is_file()) {
            continue;
        }
        let rel = rel_path(root, entry.path())?;
        let projected = project_rel_path(&rel, &config)?;
        let rel_string = normalize_rel_path(&rel);
        let projected_string = normalize_rel_path(&projected);
        let metadata = match fs::metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(err) => {
                report
                    .errors
                    .push((projected_string.clone(), format!("metadata: {err}")));
                seen.insert(rel_string);
                continue;
            }
        };
        match should_skip_file(&rel, entry.path(), &metadata, &config) {
            Ok(true) => {
                report.skipped += 1;
                continue;
            }
            Ok(false) => {}
            Err(err) => {
                report
                    .errors
                    .push((projected_string.clone(), format!("probe: {err:#}")));
                seen.insert(rel_string);
                continue;
            }
        }
        let mtime = mtime_ns(&metadata);
        let size = metadata.len() as i64;

        if let Some(state) = states.get(&rel_string) {
            if fast_path_matches(state, mtime, size) {
                candidates.push(Candidate {
                    rel,
                    projected,
                    rel_string,
                    mtime_ns: mtime,
                    size,
                    content: None,
                    sha: None,
                    protected: state.protected(),
                    external: state.external(),
                    fast: true,
                });
                continue;
            }
        }

        // Invalid UTF-8 past the 8 KiB binary probe (or a permission flip
        // between probe and read) lands here: skip the file, keep the pass.
        let content = match fs::read_to_string(entry.path()) {
            Ok(content) => content,
            Err(err) => {
                report
                    .errors
                    .push((projected_string, format!("read: {err}")));
                seen.insert(rel_string);
                continue;
            }
        };
        let sha = sha256_hex(content.as_bytes());
        let protected = collect_protected_identifiers(&rel, &content);
        let external = collect_external_identifiers(&rel, &content);
        candidates.push(Candidate {
            rel,
            projected,
            rel_string,
            mtime_ns: mtime,
            size,
            content: Some(content),
            sha: Some(sha),
            protected,
            external,
            fast: false,
        });
    }

    let union: BTreeSet<String> = candidates
        .iter()
        .flat_map(|candidate| candidate.protected.iter().cloned())
        .collect();
    // Validate reversibility for the complete workspace before writing even
    // one projected mirror path. This catches both file collisions and two
    // real directory prefixes collapsing into one agent-facing directory.
    let path_projection = PathProjection::build(
        &config,
        candidates
            .iter()
            .map(|candidate| candidate.rel_string.as_str())
            // A transiently unreadable file keeps its old db/map/mirror
            // state. Include it in the injectivity proof so a new file can
            // never take over the same projected path while it is retained.
            .chain(seen.iter().map(String::as_str)),
    )?;
    for candidate in &candidates {
        debug_assert_eq!(
            path_projection.projected_for_real(&candidate.rel)?,
            candidate.projected
        );
    }
    let mut declared_in: BTreeMap<String, String> = BTreeMap::new();
    for candidate in &candidates {
        for name in &candidate.protected {
            declared_in
                .entry(name.clone())
                .or_insert_with(|| normalize_rel_path(&candidate.projected));
        }
    }
    refuse_denylist_protected_conflicts(&config, &union, &declared_in)?;
    let logic = logic_fingerprint(&config, &union);

    for candidate in &candidates {
        seen.insert(candidate.rel_string.clone());
        let state = states.get(&candidate.rel_string);

        let fast_unchanged = state.is_some_and(|state| {
            !force_mirror
                && candidate.fast
                && state.logic_fingerprint == logic
                && layout.mirror_dir.join(&candidate.projected).exists()
                && layout.map_path(&candidate.rel).exists()
        });
        if fast_unchanged {
            report.unchanged += 1;
            continue;
        }

        let content = match &candidate.content {
            Some(content) => content.clone(),
            None => match fs::read_to_string(root.join(&candidate.rel)) {
                Ok(content) => content,
                // Became unreadable between pass 1 and here: same skip-and-
                // report semantics; the rel is already in `seen`.
                Err(err) => {
                    report.errors.push((
                        normalize_rel_path(&candidate.projected),
                        format!("read: {err}"),
                    ));
                    continue;
                }
            },
        };
        let sha = candidate
            .sha
            .clone()
            .unwrap_or_else(|| sha256_hex(content.as_bytes()));
        let protected = if candidate.fast {
            collect_protected_identifiers(&candidate.rel, &content)
        } else {
            candidate.protected.clone()
        };
        let external = if candidate.fast {
            collect_external_identifiers(&candidate.rel, &content)
        } else {
            candidate.external.clone()
        };

        // Content proved unchanged by hash and logic matches: refresh the
        // mtime/size pre-check columns without re-rendering.
        let hash_unchanged = state.is_some_and(|state| {
            !force_mirror
                && state.input_sha256 == sha
                && state.logic_fingerprint == logic
                && layout.mirror_dir.join(&candidate.projected).exists()
                && layout.map_path(&candidate.rel).exists()
        });
        if hash_unchanged {
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
            external_json: db::protected_to_json(&external),
        };
        let (outcome, _, stashed) = render_and_store(
            root,
            layout,
            &config,
            &mut conn,
            &candidate.rel,
            &candidate.projected,
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
            // Never touch the filesystem from an unvalidated stored path: a DB
            // poisoned with a `..` rel (pre-validation versions) must lose its
            // row without `mirror_dir.join("..")` deleting outside the mirror.
            match crate::config::normalize_safe_rel_path(Path::new(&tracked), "mirror") {
                Ok(rel) => {
                    let old_projected = load_span_map(&layout.map_path(&rel))
                        .ok()
                        .and_then(|map| {
                            (!map.projected_path.is_empty()).then_some(map.projected_path)
                        })
                        .map(PathBuf::from)
                        .unwrap_or_else(|| rel.clone());
                    remove_if_exists(layout.mirror_dir.join(old_projected))?;
                    remove_if_exists(layout.map_path(&rel))?;
                }
                Err(err) => {
                    log::warn!("dropping db row with unsafe path {tracked:?}: {err:#}");
                }
            }
            report.removed += 1;
        }
    }

    report.semantic = crate::semantic_store::index_workspace_locked(root, layout)?;
    Ok(report)
}

/// Resolve a CLI/hook-supplied path into a safe repo-relative path. Absolute
/// paths inside the mirror or the repo are accepted and stripped (hooks pass
/// both shapes); anything escaping the repo — `..`, absolute paths elsewhere —
/// is an error. Every downstream join (real file, mirror, map, db rel key)
/// must go through this: an unvalidated `..` rel reads files outside the repo,
/// writes sanitized copies outside the mirror, and poisons the stale sweep
/// into deleting outside `.code-sanity/`.
fn resolve_repo_rel(root: &Path, layout: &Layout, path: &Path) -> Result<PathBuf> {
    enum Spelling {
        Mirror,
        RealAbsolute,
        Relative,
    }
    let (candidate, spelling) = if path.is_absolute() {
        if let Ok(stripped) = path.strip_prefix(&layout.mirror_dir) {
            (stripped, Spelling::Mirror)
        } else if let Ok(stripped) = path.strip_prefix(root) {
            (stripped, Spelling::RealAbsolute)
        } else {
            return Err(anyhow::anyhow!(
                "path escapes repo: {} is not under {}",
                path.display(),
                root.display()
            ));
        }
    } else {
        (path, Spelling::Relative)
    };
    let candidate = crate::config::normalize_safe_rel_path(candidate, "repo")?;
    if matches!(spelling, Spelling::RealAbsolute) && root.join(&candidate).exists() {
        return Ok(candidate);
    }
    let config = Config::load_or_default(layout)?;
    let conn = db::connect(layout)?;
    db::check_schema(&conn)?;
    let projection = PathProjection::from_connection(&config, &conn)?;
    match projection.real_for_agent(&candidate) {
        Ok(real) => Ok(real),
        Err(_) if matches!(spelling, Spelling::Relative) && root.join(&candidate).exists() => {
            Ok(candidate)
        }
        Err(err) => Err(err),
    }
}

/// Index one file with the workspace lock held by this call. Force-writes the
/// mirror (used by the patch bridge and proposal approval, where the mirror
/// must be reset to sanitize(real)).
pub fn index_single_file(root: &Path, rel: &Path) -> Result<SpanMap> {
    let (layout, _lock) = init_workspace_locked(root)?;
    // Explicit user action (`sync --path --force`): escaping paths hard-fail.
    let rel = resolve_repo_rel(root, &layout, rel)?;
    crate::journal::ensure_no_interrupted_apply(&layout)?;
    let indexed = index_single_file_locked(root, &layout, &rel, true)?;
    crate::semantic_store::index_workspace_locked(root, &layout)?;
    Ok(indexed.span_map)
}

/// Sync one path (used by agent hooks): pending mirror edits are preserved.
pub fn sync_single_file(root: &Path, rel: &Path) -> Result<IndexReport> {
    let (layout, _lock) = init_workspace_locked(root)?;
    let mut report = IndexReport::default();
    // Hooks fire on every editor save and compute relpath from the cwd, so an
    // edit outside the workspace root arrives here as `../…`: a clean no-op
    // skip, not an error (the file is simply not part of this workspace).
    let rel = match resolve_repo_rel(root, &layout, rel) {
        Ok(rel) => rel,
        Err(err) => {
            log::info!("sync --path {}: {err:#}; skipped", rel.display());
            report.skipped += 1;
            return Ok(report);
        }
    };
    crate::journal::ensure_no_interrupted_apply(&layout)?;
    if !root.join(&rel).exists() {
        // The real file is gone: drop its targets.
        let conn = db::connect(&layout)?;
        let rel_string = normalize_rel_path(&rel);
        let projected = load_span_map(&layout.map_path(&rel))
            .ok()
            .and_then(|map| (!map.projected_path.is_empty()).then_some(map.projected_path))
            .map(PathBuf::from)
            .unwrap_or_else(|| rel.clone());
        db::remove_file(&conn, &rel_string)?;
        remove_if_exists(layout.mirror_dir.join(projected))?;
        remove_if_exists(layout.map_path(&rel))?;
        report.removed += 1;
        report.semantic = crate::semantic_store::index_workspace_locked(root, &layout)?;
        return Ok(report);
    }
    match index_single_file_locked(root, &layout, &rel, false)?.outcome {
        FileOutcome::Updated => report.indexed += 1,
        FileOutcome::Unchanged => report.unchanged += 1,
        FileOutcome::PendingSkipped => report.pending += 1,
    }
    report.semantic = crate::semantic_store::index_workspace_locked(root, &layout)?;
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
    db::ensure_schema(&conn)?;

    let rel_string = normalize_rel_path(rel);
    let source_path = root.join(rel);
    let (content, metadata) = read_with_stat(&source_path)?;
    let sha = sha256_hex(content.as_bytes());
    let fresh_protected = collect_protected_identifiers(rel, &content);
    let fresh_external = collect_external_identifiers(rel, &content);

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
    let mut declared_in: BTreeMap<String, String> = BTreeMap::new();
    for state in states.iter().filter(|state| state.rel_path != rel_string) {
        for name in state.protected() {
            declared_in.entry(name).or_insert_with(|| {
                project_rel_path(Path::new(&state.rel_path), &config)
                    .map(|path| normalize_rel_path(&path))
                    .unwrap_or_else(|_| state.rel_path.clone())
            });
        }
    }
    for name in &fresh_protected {
        declared_in.entry(name.clone()).or_insert_with(|| {
            project_rel_path(rel, &config)
                .map(|path| normalize_rel_path(&path))
                .unwrap_or_else(|_| rel_string.clone())
        });
    }
    refuse_denylist_protected_conflicts(&config, &union, &declared_in)?;
    let logic = logic_fingerprint(&config, &union);
    let mut tracked_paths = db::tracked_files(&conn)?;
    if !tracked_paths.iter().any(|path| path == &rel_string) {
        tracked_paths.push(rel_string.clone());
    }
    let path_projection = PathProjection::build(&config, tracked_paths.iter())?;
    let projected = path_projection.projected_for_real(rel)?;

    let state_row = IndexState {
        rel_path: rel_string,
        input_sha256: sha,
        mtime_ns: mtime_ns(&metadata),
        size: metadata.len() as i64,
        logic_fingerprint: logic,
        protected_json: db::protected_to_json(&fresh_protected),
        external_json: db::protected_to_json(&fresh_external),
    };
    let (outcome, span_map, stashed) = render_and_store(
        root,
        layout,
        &config,
        &mut conn,
        rel,
        &projected,
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

pub(crate) fn pending_mirror_edit_count(layout: &Layout) -> Result<usize> {
    let conn = db::connect(layout)?;
    db::check_schema(&conn)?;
    let mut pending = 0usize;
    for state in db::all_index_states(&conn)? {
        let rel = crate::config::normalize_safe_rel_path(
            Path::new(&state.rel_path),
            "approval mirror preflight",
        )?;
        let map = match load_span_map(&layout.map_path(&rel)) {
            Ok(map) => map,
            Err(_) => continue,
        };
        let projected = if map.projected_path.is_empty() {
            rel.clone()
        } else {
            crate::config::normalize_safe_rel_path(
                Path::new(&map.projected_path),
                "approval projected mirror",
            )?
        };
        let mirror = match fs::read(layout.mirror_dir.join(projected)) {
            Ok(mirror) => mirror,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                pending += 1;
                continue;
            }
        };
        let persisted_hash = db::file_hashes(&conn, &state.rel_path)?
            .map(|(_, sanitized)| sanitized)
            .unwrap_or(map.sanitized_hash);
        pending += usize::from(sha256_hex(&mirror) != persisted_hash);
    }
    Ok(pending)
}

/// Cheap approval preflight over persisted state. Unlike a normal incremental
/// index this does no parsing: it only proves that source bytes, policy logic,
/// semantic resolver data, maps, and mirrors all describe the same snapshot.
/// Any mismatch sends the caller through one ordinary index pass before it
/// resolves proposal targets.
pub(crate) fn persisted_workspace_is_current(root: &Path, layout: &Layout) -> Result<bool> {
    let config = Config::load_or_default(layout)?;
    let conn = db::connect(layout)?;
    db::check_schema(&conn)?;
    let states = db::all_index_states(&conn)?;
    let tracked = db::tracked_files(&conn)?;
    let state_paths = states
        .iter()
        .map(|state| state.rel_path.as_str())
        .collect::<BTreeSet<_>>();
    let tracked_paths = tracked.iter().map(String::as_str).collect::<BTreeSet<_>>();
    if state_paths != tracked_paths || !crate::semantic_store::semantic_index_is_current(&conn)? {
        return Ok(false);
    }

    let protected_union = states
        .iter()
        .flat_map(IndexState::protected)
        .collect::<BTreeSet<_>>();
    let logic = logic_fingerprint(&config, &protected_union);
    let projection = PathProjection::build(&config, tracked.iter())?;
    for state in &states {
        if state.logic_fingerprint != logic {
            return Ok(false);
        }
        let rel = crate::config::normalize_safe_rel_path(
            Path::new(&state.rel_path),
            "approval index preflight",
        )?;
        let content = match fs::read_to_string(root.join(&rel)) {
            Ok(content) => content,
            Err(_) => return Ok(false),
        };
        if sha256_hex(content.as_bytes()) != state.input_sha256 {
            return Ok(false);
        }
        let map = match load_span_map(&layout.map_path(&rel)) {
            Ok(map) => map,
            Err(_) => return Ok(false),
        };
        let projected = projection.projected_string_for_real(&state.rel_path)?;
        if map.original_hash != state.input_sha256 || map.projected_path != projected {
            return Ok(false);
        }
        let mirror = match fs::read(layout.mirror_dir.join(&projected)) {
            Ok(mirror) => mirror,
            Err(_) => return Ok(false),
        };
        if sha256_hex(&mirror) != map.sanitized_hash {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Re-render and move every tracked mirror after a path-only policy change
/// without reparsing the semantic workspace. Path aliases cannot change real
/// content, protected identifiers, or symbol ownership, so the persisted
/// index state is a safe render input as long as every source hash still
/// matches. If anything drifted, fall back to the ordinary full index before
/// writing a single mirror.
pub(crate) fn reproject_tracked_mirrors_locked(
    root: &Path,
    layout: &Layout,
) -> Result<IndexReport> {
    let config = Config::load_or_default(layout)?;
    let mut conn = db::connect(layout)?;
    db::check_schema(&conn)?;
    let states = db::all_index_states(&conn)?;
    let tracked = db::tracked_files(&conn)?;
    let state_paths = states
        .iter()
        .map(|state| state.rel_path.as_str())
        .collect::<BTreeSet<_>>();
    let tracked_paths = tracked.iter().map(String::as_str).collect::<BTreeSet<_>>();
    if state_paths != tracked_paths || !crate::semantic_store::semantic_index_is_current(&conn)? {
        drop(conn);
        return index_workspace_locked(root, layout);
    }

    struct Snapshot {
        rel: PathBuf,
        projected: PathBuf,
        content: String,
        metadata: fs::Metadata,
        state: IndexState,
    }

    let projection = PathProjection::build(&config, tracked.iter())?;
    let protected_union = states
        .iter()
        .flat_map(IndexState::protected)
        .collect::<BTreeSet<_>>();
    let mut declared_in = BTreeMap::<String, String>::new();
    for state in &states {
        let projected = projection.projected_string_for_real(&state.rel_path)?;
        for name in state.protected() {
            declared_in.entry(name).or_insert_with(|| projected.clone());
        }
    }
    refuse_denylist_protected_conflicts(&config, &protected_union, &declared_in)?;
    let logic = logic_fingerprint(&config, &protected_union);

    let mut snapshots = Vec::with_capacity(states.len());
    for mut state in states {
        let rel = crate::config::normalize_safe_rel_path(
            Path::new(&state.rel_path),
            "path-only mirror refresh",
        )?;
        let (content, metadata) = read_with_stat(&root.join(&rel))?;
        if sha256_hex(content.as_bytes()) != state.input_sha256 {
            drop(conn);
            return index_workspace_locked(root, layout);
        }
        state.logic_fingerprint = logic.clone();
        state.mtime_ns = mtime_ns(&metadata);
        state.size = metadata.len() as i64;
        snapshots.push(Snapshot {
            projected: projection.projected_for_real(&rel)?,
            rel,
            content,
            metadata,
            state,
        });
    }

    let mut report = IndexReport::default();
    for snapshot in snapshots {
        debug_assert_eq!(snapshot.state.size, snapshot.metadata.len() as i64);
        let (outcome, _, stashed) = render_and_store(
            root,
            layout,
            &config,
            &mut conn,
            &snapshot.rel,
            &snapshot.projected,
            &snapshot.content,
            snapshot.state,
            &protected_union,
            false,
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
    Ok(report)
}

/// Re-render tracked mirrors after the semantic index or accepted alias set
/// changes. This pass never reparses semantics and preserves pending agent
/// edits; it only makes the persisted mirror/map use the same combined lexical
/// + symbol-scoped projection as `read_code`.
pub(crate) fn refresh_semantic_mirrors_locked(root: &Path, layout: &Layout) -> Result<()> {
    let conn = db::connect(layout)?;
    db::check_schema(&conn)?;
    let mut statement = conn
        .prepare(
            r#"
            select distinct occurrence.rel_path
            from semantic_occurrences occurrence
            join semantic_aliases alias on alias.symbol_id = occurrence.symbol_id
            where alias.status = 'accepted'
            union
            select distinct file.rel_path
            from files file
            join replacements replacement on replacement.file_id = file.id
            where replacement.policy_source = 'semantic-alias'
            order by 1
            "#,
        )
        .context("prepare semantic mirror dirty set")?;
    let candidates = statement
        .query_map([], |row| row.get::<_, String>(0))
        .context("query semantic mirror dirty set")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("collect semantic mirror dirty set")?;
    drop(statement);
    refresh_semantic_mirror_paths_locked(root, layout, conn, candidates)
}

/// Refresh only semantic components changed by one bulk approval. Existing
/// unrelated aliases already match their maps and must not make an approval
/// scan the whole historical alias set.
pub(crate) fn refresh_semantic_mirrors_for_symbols_locked(
    root: &Path,
    layout: &Layout,
    symbol_ids: &BTreeSet<String>,
) -> Result<()> {
    if symbol_ids.is_empty() {
        return Ok(());
    }
    let conn = db::connect(layout)?;
    db::check_schema(&conn)?;
    let selected = serde_json::to_string(symbol_ids).context("serialize mirror symbol set")?;
    let mut statement = conn
        .prepare(
            r#"
            with recursive edges(left_id, right_id) as (
                select canonical_symbol_id, linked_symbol_id from semantic_compiler_links
                union all
                select linked_symbol_id, canonical_symbol_id from semantic_compiler_links
            ), component(symbol_id) as (
                select value from json_each(?1)
                union
                select edge.right_id
                from edges edge join component on component.symbol_id = edge.left_id
            )
            select distinct occurrence.rel_path
            from semantic_occurrences occurrence
            join component on component.symbol_id = occurrence.symbol_id
            join semantic_aliases alias on alias.symbol_id = occurrence.symbol_id
            where alias.status = 'accepted'
            union
            select distinct file.rel_path
            from files file
            join replacements replacement on replacement.file_id = file.id
            join component
              on replacement.stable_key like 'semantic:' || component.symbol_id || ':%'
            where replacement.policy_source = 'semantic-alias'
            order by 1
            "#,
        )
        .context("prepare targeted semantic mirror dirty set")?;
    let candidates = statement
        .query_map([selected], |row| row.get::<_, String>(0))
        .context("query targeted semantic mirror dirty set")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("collect targeted semantic mirror dirty set")?;
    drop(statement);
    refresh_semantic_mirror_paths_locked(root, layout, conn, candidates)
}

fn refresh_semantic_mirror_paths_locked(
    root: &Path,
    layout: &Layout,
    conn: rusqlite::Connection,
    candidates: Vec<String>,
) -> Result<()> {
    let mut tracked = Vec::new();
    for rel in candidates {
        let rel_path = crate::config::normalize_safe_rel_path(Path::new(&rel), "semantic mirror")?;
        let matches = load_span_map(&layout.map_path(&rel_path))
            .ok()
            .is_some_and(|map| {
                crate::semantic_store::semantic_projection_matches_map(&conn, &rel, &map)
                    .unwrap_or(false)
            });
        if !matches {
            tracked.push(rel);
        }
    }
    drop(conn);
    for rel in tracked {
        let rel = crate::config::normalize_safe_rel_path(Path::new(&rel), "semantic mirror")?;
        if fs::read_to_string(root.join(&rel)).is_err() {
            // The main index pass already reported the unreadable file and
            // deliberately preserved its previous mirror/map.
            continue;
        }
        let _ = index_single_file_locked(root, layout, &rel, false)?;
    }
    Ok(())
}

/// The repo-wide protected identifier union as last indexed.
/// A denylisted term kept alive by a protected identifier is an unsatisfiable
/// policy: the protected set exists so public symbols stay real, the denylist
/// so a term never reaches the agent. Refuse loudly instead of silently
/// leaking — `verify` cannot catch this, because it sanctions protected runs
/// by construction. Same treatment as an alias collision.
fn refuse_denylist_protected_conflicts(
    config: &Config,
    union: &BTreeSet<String>,
    declared_in: &BTreeMap<String, String>,
) -> Result<()> {
    let terms = crate::sanitize::term_table(config);
    if let Some(conflict) = crate::sanitize::denylist_protected_conflicts(&terms, union).first() {
        let origin = declared_in
            .get(&conflict.protected_name)
            .map(|rel| format!(" (declared in {rel})"))
            .unwrap_or_default();
        anyhow::bail!(
            "denylist term {:?} is protected as public identifier {:?}{origin}; the mirror \
             must keep public names real, so the term would survive verbatim. Remove it from \
             the denylist, add it to sanitizer.allowlist, or rename the public symbol in the \
             real repo, then run `code-sanity sync`",
            conflict.term,
            conflict.protected_name,
        );
    }
    Ok(())
}

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
        "path_behavior": PATH_PROJECTION_VERSION,
        "salt": config.salt,
        "dictionary": config.sanitizer.dictionary,
        "alias_registry": config.sanitizer.alias_registry,
        "path_alias_registry": config.sanitizer.path_alias_registry,
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
    projected_rel: &Path,
    content: &str,
    state: IndexState,
    protected_union: &BTreeSet<String>,
    force_mirror: bool,
) -> Result<(FileOutcome, SpanMap, Option<PathBuf>)> {
    let _ = root;
    // Alias collision = ambiguous mirror: the natural word would survive into
    // the mirror indistinguishable from the alias, and an agent typing that
    // word would reverse-map into the real term. Refuse to render; the error
    // carries the remediation. The incremental fast path stays complete: the
    // logic fingerprint covers the whole term set, so any alias change
    // re-renders (and re-checks) every file.
    let terms = crate::sanitize::term_table(config);
    if let Some(collision) = crate::sanitize::alias_collisions(content, &terms).first() {
        anyhow::bail!(
            "{}: alias {:?} (for term {:?}, {}) occurs naturally in the real file as {:?} \
             at byte {}; the sanitized mirror would be ambiguous. Choose a different alias \
             for {:?} in .code-sanity/config.toml (or rename the conflicting word), then \
             run `code-sanity sync`",
            normalize_rel_path(projected_rel),
            collision.alias,
            collision.term,
            collision.policy_source,
            collision.word,
            collision.offset,
            collision.term,
        );
    }
    let lexical = sanitize_content(rel, content, config, protected_union)
        .with_context(|| format!("sanitize {}", rel.display()))?;
    let mut rendered =
        crate::semantic_store::merge_semantic_aliases(conn, &state.rel_path, content, lexical)
            .with_context(|| format!("apply semantic aliases to {}", rel.display()))?;
    rendered.span_map.projected_path = normalize_rel_path(projected_rel);

    let map_path = layout.map_path(rel);
    let old_map_raw = fs::read_to_string(&map_path).ok();
    let old_map = old_map_raw
        .as_deref()
        .and_then(|raw| serde_json::from_str::<SpanMap>(raw).ok());
    let old_projected = old_map
        .as_ref()
        .and_then(|map| (!map.projected_path.is_empty()).then_some(&map.projected_path))
        .map(PathBuf::from)
        .unwrap_or_else(|| rel.to_path_buf());
    let mirror_path = layout.mirror_dir.join(projected_rel);
    let old_mirror_path = layout.mirror_dir.join(&old_projected);
    let active_mirror_path = if old_mirror_path.exists() {
        &old_mirror_path
    } else {
        &mirror_path
    };
    let old_mirror = fs::read_to_string(active_mirror_path).ok();
    let db_sanitized_hash = db::file_hashes(conn, &state.rel_path)?.map(|(_, hash)| hash);

    // Pending-edit protection: the agent edited this mirror file and the edit
    // has not been projected yet (mirror on disk differs from the last indexed
    // sanitized hash). Sync must not clobber it; only the patch bridge (force)
    // may reset the mirror to sanitize(real).
    //
    // A missing db row (deleted db.sqlite — the documented corruption remedy —
    // or a crash before the first upsert) cannot PROVE the on-disk mirror is
    // our render, so it counts as pending too: fail safe, exactly like the
    // force path below. Only `sync --force` may reset it, and that stashes.
    //
    // Self-heal: a mirror that already equals the fresh render is converged
    // content with a stale db row (a crash between the mirror write and the db
    // commit), not a pending edit — fall through so the upsert repairs the row.
    if !force_mirror {
        if let Some(old) = old_mirror.as_deref() {
            if old != rendered.sanitized
                && db_sanitized_hash
                    .as_deref()
                    .is_none_or(|hash| sha256_hex(old.as_bytes()) != hash)
            {
                let previous = load_span_map(&map_path).unwrap_or(rendered.span_map);
                return Ok((FileOutcome::PendingSkipped, previous, None));
            }
        }
    }

    // A force reset is about to discard an un-projected agent edit: keep a
    // durable copy under journal/discarded/ before overwriting it.
    let mut stashed = None;
    if force_mirror {
        if let Some(old) = old_mirror.as_deref() {
            if old != rendered.sanitized
                && db_sanitized_hash
                    .as_deref()
                    .is_none_or(|hash| sha256_hex(old.as_bytes()) != hash)
            {
                let stash_path = layout
                    .journal_dir
                    .join("discarded")
                    .join(crate::journal::new_journal_id())
                    .join(projected_rel);
                crate::fsutil::atomic_write_sync(&stash_path, old).with_context(|| {
                    format!("stash pending mirror edit for {}", projected_rel.display())
                })?;
                log::info!(
                    "resetting a pending mirror edit for {}; copy kept at {}",
                    projected_rel.display(),
                    stash_path.display()
                );
                stashed = Some(stash_path);
                // Best-effort retention (same knob as journal entries): force-sync
                // heavy workspaces must not accumulate stash dirs without bound.
                if let Err(err) =
                    crate::journal::prune_discarded_stashes(layout, config.journal.max_entries)
                {
                    log::warn!("discarded-stash pruning failed: {err:#}");
                }
            }
        }
    }

    if let Some(old_map) = old_map {
        if old_map.original_hash == rendered.span_map.original_hash
            && old_map.sanitized_hash == rendered.span_map.sanitized_hash
            && old_map.replacements == rendered.span_map.replacements
            && old_map.spans == rendered.span_map.spans
        {
            rendered.span_map.updated_at = old_map.updated_at;
        }
    }
    let next_map = serde_json::to_string_pretty(&rendered.span_map).context("serialize map")?;
    let unchanged = old_mirror.as_deref() == Some(rendered.sanitized.as_str())
        && old_map_raw.as_deref() == Some(next_map.as_str());

    crate::fsutil::atomic_write_if_changed(&mirror_path, &rendered.sanitized)
        .with_context(|| format!("write mirror {}", mirror_path.display()))?;
    crate::fsutil::atomic_write_if_changed(&map_path, &next_map)
        .with_context(|| format!("write map {}", map_path.display()))?;
    db::upsert_indexed_file(conn, &rendered.span_map, &state)?;
    if old_mirror_path != mirror_path && old_mirror_path.exists() {
        remove_if_exists(&old_mirror_path)?;
        remove_empty_mirror_parents(&old_mirror_path, &layout.mirror_dir)?;
    }

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

/// `0` is the "unknown" sentinel (mtime-less filesystem or a metadata error).
/// See `fast_path_matches`: unknown must never satisfy the pre-check.
fn mtime_ns(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as i64)
        .unwrap_or(0)
}

/// mtime/size pre-check for skipping the content read. An unknown mtime
/// (`0`) never matches: on a filesystem without mtimes every same-size change
/// would otherwise stay invisible forever — the slow path (read + hash) is
/// the only safe answer there.
fn fast_path_matches(state: &IndexState, mtime_ns: i64, size: i64) -> bool {
    mtime_ns != 0 && state.mtime_ns == mtime_ns && state.size == size
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
            // The name-based skip applies to DIRECTORIES only: a source FILE
            // named `build` or `dist` is legitimate content.
            if !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_dir())
            {
                return true;
            }
            let name = entry.file_name().to_string_lossy();
            !extra_dirs.contains(name.as_ref())
        })
        .build()
        .collect::<Vec<_>>();
    Ok(walker)
}

/// Lightweight source discovery for UI scope selection before the first
/// index. It follows the exact workspace ignore/binary/size policy but does
/// not create mirror or database state.
pub(crate) fn discover_indexable_files(root: &Path, config: &Config) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut first_entry = true;
    for entry in walk_repo(root, config)? {
        let is_root_probe = std::mem::take(&mut first_entry);
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) if is_root_probe => {
                return Err(err).with_context(|| format!("walk repo root {}", root.display()));
            }
            Err(_) => continue,
        };
        let file_type = entry.file_type();
        if file_type.is_some_and(|file_type| file_type.is_symlink())
            || !file_type.is_some_and(|file_type| file_type.is_file())
        {
            continue;
        }
        let rel = rel_path(root, entry.path())?;
        let Ok(metadata) = fs::metadata(entry.path()) else {
            continue;
        };
        if !matches!(
            should_skip_file(&rel, entry.path(), &metadata, config),
            Ok(false)
        ) {
            continue;
        }
        files.push(rel);
    }
    Ok(files)
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
    // Workspace-local dotenv may contain provider credentials. It is loaded
    // before dispatch and must never enter the mirror or a model payload even
    // when the repository's existing ignore rules forgot it.
    if file_name == ".env" {
        return Ok(true);
    }
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

fn remove_if_exists(path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("remove {}", path.display())),
    }
}

fn remove_empty_mirror_parents(path: &Path, mirror_root: &Path) -> Result<()> {
    let mut parent = path.parent();
    while let Some(directory) = parent {
        if directory == mirror_root || !directory.starts_with(mirror_root) {
            break;
        }
        match fs::remove_dir(directory) {
            Ok(()) => parent = directory.parent(),
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) =>
            {
                break;
            }
            Err(err) => {
                return Err(err).with_context(|| format!("remove empty {}", directory.display()));
            }
        }
    }
    Ok(())
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

    #[test]
    fn approval_preflight_rejects_an_old_semantic_resolver_snapshot() {
        let repo = tempfile::tempdir().unwrap();
        fs::create_dir_all(repo.path().join("src")).unwrap();
        fs::write(
            repo.path().join("src/lib.rs"),
            "fn current_snapshot() -> usize { 1 }\n",
        )
        .unwrap();
        crate::index_workspace(repo.path()).unwrap();
        let layout = Layout::new(repo.path());
        assert!(persisted_workspace_is_current(repo.path(), &layout).unwrap());
        let mirror_path = layout.mirror_dir.join("src/lib.rs");
        let mirror = fs::read(&mirror_path).unwrap();
        let mut edited = mirror.clone();
        edited.extend_from_slice(b"// pending agent edit\n");
        fs::write(&mirror_path, edited).unwrap();
        assert_eq!(pending_mirror_edit_count(&layout).unwrap(), 1);
        assert!(!persisted_workspace_is_current(repo.path(), &layout).unwrap());
        fs::write(&mirror_path, mirror).unwrap();
        assert_eq!(pending_mirror_edit_count(&layout).unwrap(), 0);

        let conn = db::connect(&layout).unwrap();
        let raw = conn
            .query_row(
                "select capabilities_json from semantic_documents where rel_path = 'src/lib.rs'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        let mut capabilities =
            serde_json::from_str::<crate::semantic::BackendCapabilities>(&raw).unwrap();
        capabilities.resolver_version = 0;
        conn.execute(
            "update semantic_documents set capabilities_json = ?1 where rel_path = 'src/lib.rs'",
            [serde_json::to_string(&capabilities).unwrap()],
        )
        .unwrap();
        drop(conn);

        assert!(!persisted_workspace_is_current(repo.path(), &layout).unwrap());
        crate::index_workspace(repo.path()).unwrap();
        assert!(persisted_workspace_is_current(repo.path(), &layout).unwrap());
    }
}
