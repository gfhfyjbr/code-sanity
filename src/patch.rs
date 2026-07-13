use crate::config::{Config, Layout, normalize_rel_path, normalize_safe_rel_path};
use crate::db;
use crate::index::{
    index_single_file_locked, init_workspace_locked, reconverge_workspace,
    stored_protected_union_with_override,
};
use crate::journal::{
    JournalEntry, JournalStatus, PendingFile, list_journal_entries, new_journal_id, write_journal,
};
use crate::map::{SpanMap, common_changed_range, load_span_map, sha256_hex};
use crate::path_projection::PathProjection;
use crate::sanitize::{
    Term, adapt_replacement, collect_protected_identifiers, hits_in_run, normalize_term,
    sanitize_content, sanitize_run_text, term_table, word_runs,
};
use crate::search::{ensure_existing_path_inside, normalize_sanitized_rel_path};
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Typed conflict error so the CLI can exit with the dedicated conflict code.
#[derive(Debug)]
pub struct ConflictError {
    pub message: String,
    pub journal_path: PathBuf,
}

impl std::fmt::Display for ConflictError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}; conflict journal written to {}",
            self.message,
            self.journal_path.display()
        )
    }
}

impl std::error::Error for ConflictError {}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ApplyReport {
    pub files: Vec<String>,
    /// `None` for a dry run: nothing was journaled as an apply.
    pub journal_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct ApplyOptions {
    pub session_id: Option<String>,
    pub agent: Option<String>,
    /// Plan and validate only: the full parse/translate/conflict pipeline
    /// runs (a conflicting patch still raises ConflictError and records a
    /// conflict journal entry — state-dir-only writes, kept as the audit
    /// trail), but no real file, mirror, or apply journal entry is written.
    pub dry_run: bool,
}

/// One already-validated real-source update produced by the semantic v2
/// transaction planner. The commit path below reuses the durable v1 journal,
/// rollback, permission preservation, and reindex machinery.
pub(crate) struct RealFileUpdate {
    pub rel: PathBuf,
    pub before: String,
    pub after: String,
}

pub(crate) fn commit_real_file_updates_locked(
    root: &Path,
    layout: &Layout,
    updates: &[RealFileUpdate],
    expected_mirrors: &BTreeMap<String, (PathBuf, String)>,
    agent: Option<String>,
    session_id: Option<String>,
) -> Result<PathBuf> {
    if updates.is_empty() {
        bail!("semantic transaction contains no file updates");
    }
    let conn = db::connect(layout)?;
    db::ensure_schema(&conn)?;
    let config = Config::load_or_default(layout)?;
    let files = updates
        .iter()
        .map(|update| normalize_rel_path(&update.rel))
        .collect::<Vec<_>>();
    let mut original_patch = String::new();
    let planned = updates
        .iter()
        .map(|update| {
            let rel_string = normalize_rel_path(&update.rel);
            original_patch.push_str(&whole_file_patch(
                &update.rel,
                &update.before,
                &update.after,
            ));
            PlannedFileApply {
                rel: update.rel.clone(),
                before: Some(update.before.clone()),
                op: PlannedOp::Write(update.after.clone()),
                expected_mirror: expected_mirrors.get(&rel_string).cloned(),
            }
        })
        .collect::<Vec<_>>();
    commit_planned_apply(
        root,
        layout,
        &conn,
        &ApplyOptions {
            agent,
            session_id,
            dry_run: false,
        },
        &original_patch,
        &original_patch,
        &files,
        &planned,
        config.journal.max_entries,
        None,
    )
}

#[derive(Debug, Clone)]
struct UnifiedPatch {
    files: Vec<FilePatch>,
}

#[derive(Debug, Clone)]
struct FilePatch {
    old_path: String,
    new_path: String,
    hunks: Vec<Hunk>,
}

#[derive(Debug, Clone)]
struct Hunk {
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
    lines: Vec<HunkLine>,
}

#[derive(Debug, Clone)]
enum HunkLine {
    Context(String),
    Add(String),
    Remove(String),
}

pub fn apply_patch_text(root: &Path, patch_text: &str) -> Result<ApplyReport> {
    apply_patch_text_with_options(root, patch_text, ApplyOptions::default())
}

pub fn apply_patch_text_with_options(
    root: &Path,
    patch_text: &str,
    options: ApplyOptions,
) -> Result<ApplyReport> {
    apply_patch_text_with_options_inner(root, patch_text, options, None)
}

#[cfg(test)]
fn apply_patch_text_with_failure_after_writes(
    root: &Path,
    patch_text: &str,
    fail_after_writes: usize,
) -> Result<ApplyReport> {
    apply_patch_text_with_options_inner(
        root,
        patch_text,
        ApplyOptions::default(),
        Some(fail_after_writes),
    )
}

fn apply_patch_text_with_options_inner(
    root: &Path,
    patch_text: &str,
    options: ApplyOptions,
    fail_after_writes_for_test: Option<usize>,
) -> Result<ApplyReport> {
    let (layout, _lock) = init_workspace_locked(root)?;
    apply_patch_text_locked(
        root,
        &layout,
        patch_text,
        options,
        fail_after_writes_for_test,
    )
}

/// Apply a patch with the exclusive workspace lock already held by the caller
/// (multi-step flows like project-edit hold one lock across refresh + apply).
pub(crate) fn apply_patch_text_locked(
    root: &Path,
    layout: &Layout,
    patch_text: &str,
    options: ApplyOptions,
    fail_after_writes_for_test: Option<usize>,
) -> Result<ApplyReport> {
    crate::journal::ensure_no_interrupted_apply(layout)?;
    let config = Config::load_or_default(layout)?;
    let conn = db::connect(layout)?;
    db::ensure_schema(&conn)?;

    let parsed = parse_unified_patch(patch_text)?;
    if parsed.files.is_empty() {
        bail!("patch contains no file changes");
    }

    let mut planned = Vec::<PlannedFileApply>::new();
    let mut original_patch = String::new();
    let mut files = Vec::new();

    for file_patch in parsed.files {
        match classify_file_op(&file_patch) {
            FileOp::Modify => plan_modify(
                root,
                layout,
                &config,
                &conn,
                &options,
                patch_text,
                &file_patch,
                &mut planned,
                &mut original_patch,
                &mut files,
            )?,
            FileOp::Create => plan_create(
                root,
                layout,
                &config,
                &conn,
                &options,
                patch_text,
                &file_patch,
                &mut planned,
                &mut original_patch,
                &mut files,
            )?,
            FileOp::Delete => plan_delete(
                root,
                layout,
                &conn,
                &options,
                patch_text,
                &file_patch,
                &mut planned,
                &mut original_patch,
                &mut files,
            )?,
        }
    }

    // Dry run stops here: the full plan ran (parse, sanitize projection,
    // conflict detection — a conflict already bailed above), nothing was
    // journaled as an apply and no real file was touched.
    if options.dry_run {
        return Ok(ApplyReport {
            files,
            journal_path: None,
        });
    }

    let journal_path = commit_planned_apply(
        root,
        layout,
        &conn,
        &options,
        patch_text,
        &original_patch,
        &files,
        &planned,
        config.journal.max_entries,
        fail_after_writes_for_test,
    )?;

    Ok(ApplyReport {
        files,
        journal_path: Some(journal_path),
    })
}

enum FileOp {
    Modify,
    Create,
    Delete,
}

fn classify_file_op(file_patch: &FilePatch) -> FileOp {
    let old_null = file_patch.old_path == "/dev/null";
    let new_null = file_patch.new_path == "/dev/null";
    match (old_null, new_null) {
        (true, false) => FileOp::Create,
        (false, true) => FileOp::Delete,
        _ => FileOp::Modify,
    }
}

#[allow(clippy::too_many_arguments)]
fn plan_modify(
    root: &Path,
    layout: &Layout,
    config: &Config,
    conn: &rusqlite::Connection,
    options: &ApplyOptions,
    patch_text: &str,
    file_patch: &FilePatch,
    planned: &mut Vec<PlannedFileApply>,
    original_patch: &mut String,
    files: &mut Vec<String>,
) -> Result<()> {
    let agent_rel = normalize_patch_file_path(&file_patch.new_path, root, layout)
        .or_else(|_| normalize_patch_file_path(&file_patch.old_path, root, layout))
        .with_context(|| {
            format!(
                "patch paths are not inside sanitized mirror or repo: {} -> {}",
                file_patch.old_path, file_patch.new_path
            )
        })?;
    let projection = PathProjection::from_connection(config, conn)?;
    let rel = projection.real_for_agent(&agent_rel)?;
    let projected_rel = projection.projected_for_real(&rel)?;
    let rel_string = normalize_rel_path(&rel);
    let display_string = normalize_rel_path(&projected_rel);
    let real_path = root.join(&rel);
    let mirror_path = layout.mirror_dir.join(&projected_rel);
    let map_path = layout.map_path(&rel);

    let span_map = load_span_map(&map_path)
        .with_context(|| format!("load span map {}; run index first", map_path.display()))?;
    let (db_original_hash, db_sanitized_hash) = db::file_hashes(conn, &rel_string)?
        .ok_or_else(|| anyhow!("{display_string}: file is not tracked; run index first"))?;
    let real_content = fs::read_to_string(&real_path)
        .with_context(|| format!("read real file {}", real_path.display()))?;
    let mirror_content = fs::read_to_string(&mirror_path)
        .with_context(|| format!("read mirror file {}", mirror_path.display()))?;

    let real_hash = sha256_hex(real_content.as_bytes());
    let mirror_hash = sha256_hex(mirror_content.as_bytes());
    if real_hash != db_original_hash {
        return write_conflict_and_bail(
            layout,
            conn,
            options,
            patch_text,
            original_patch,
            files,
            format!("{display_string}: real file drifted since last index; run `code-sanity sync`"),
        );
    }
    if mirror_hash != db_sanitized_hash || mirror_hash != span_map.sanitized_hash {
        return write_conflict_and_bail(
            layout,
            conn,
            options,
            patch_text,
            original_patch,
            files,
            format!(
                "{display_string}: sanitized mirror drifted since last index; run `code-sanity sync`"
            ),
        );
    }

    for (start, end) in changed_ranges(&mirror_content, file_patch)? {
        if span_map.conflicts_with_sanitized_edit(start, end) {
            return write_conflict_and_bail(
                layout,
                conn,
                options,
                patch_text,
                original_patch,
                files,
                format!(
                    "{display_string}: patch edits sanitized replacement span at bytes {start}..{end}; automatic apply refused"
                ),
            );
        }
    }

    let patched_sanitized = apply_file_patch_to_content(&mirror_content, file_patch)
        .with_context(|| format!("apply sanitized patch to {display_string}"))?;
    let stored_union = crate::index::stored_protected_union(conn)?;
    let semantic_aliases = crate::semantic_store::accepted_alias_pairs(conn)?;
    let original_file_patch = match translate_file_patch(
        file_patch,
        &span_map,
        &mirror_content,
        &patched_sanitized,
        config,
        &semantic_aliases,
        &stored_union,
    ) {
        Ok(translated) => translated,
        Err(err) => {
            return write_conflict_and_bail(
                layout,
                conn,
                options,
                patch_text,
                original_patch,
                files,
                format!("{display_string}: {err:#}"),
            );
        }
    };
    let patched_original = apply_file_patch_to_content(&real_content, &original_file_patch)
        .with_context(|| format!("apply translated patch to {display_string}"))?;
    // The patch must not introduce an alias word into REAL content (typically
    // via prose/comment runs that reverse mapping deliberately leaves
    // verbatim): the post-apply mirror would be ambiguous and the reindex of
    // this file would refuse, stranding the workspace mid-apply. Conflict now.
    let terms_after = crate::sanitize::term_table(config);
    if let Some(collision) =
        crate::sanitize::alias_collisions(&patched_original, &terms_after).first()
    {
        return write_conflict_and_bail(
            layout,
            conn,
            options,
            patch_text,
            &render_file_patch(&original_file_patch),
            files,
            format!(
                "{display_string}: patch would introduce alias word {:?} (alias of {:?}) into the \
                 real file; the sanitized view would be ambiguous — use a different word or \
                 change the alias in .code-sanity/config.toml",
                collision.word, collision.term
            ),
        );
    }
    // Sanitize with the protected union that will hold AFTER this file lands,
    // exactly what the post-apply reindex of this file will use.
    let fresh_protected = collect_protected_identifiers(&rel, &patched_original);
    let union_after = stored_protected_union_with_override(conn, &rel_string, &fresh_protected)?;
    let rendered_after = sanitize_content(&rel, &patched_original, config, &union_after)
        .with_context(|| format!("resanitize patched {display_string}"))?;
    let semantic_projection = span_map
        .replacements
        .iter()
        .any(|replacement| replacement.policy_source == "semantic-alias");
    if !semantic_projection && rendered_after.sanitized != patched_sanitized {
        return write_conflict_and_bail(
            layout,
            conn,
            options,
            patch_text,
            &render_file_patch(&original_file_patch),
            files,
            format!(
                "{display_string}: translated patch does not preserve sanitize(real) == patched mirror invariant"
            ),
        );
    }
    // Bidirectional invariant: reverse-projecting the patched mirror through
    // the fresh span map must reproduce the patched real file byte-for-byte.
    let reverse_projected = match reverse_sanitized_region(
        &rendered_after.span_map,
        &rendered_after.sanitized,
        0,
        rendered_after.sanitized.len(),
    ) {
        Ok(projected) => projected,
        Err(err) => {
            return write_conflict_and_bail(
                layout,
                conn,
                options,
                patch_text,
                &render_file_patch(&original_file_patch),
                files,
                format!("{display_string}: corrupt span map during reverse projection: {err:#}"),
            );
        }
    };
    if !semantic_projection && reverse_projected != patched_original {
        return write_conflict_and_bail(
            layout,
            conn,
            options,
            patch_text,
            &render_file_patch(&original_file_patch),
            files,
            format!(
                "{display_string}: reverse projection of patched mirror does not reproduce patched real file"
            ),
        );
    }

    original_patch.push_str(&render_file_patch(&original_file_patch));
    files.push(normalize_rel_path(&projected_rel));
    planned.push(PlannedFileApply {
        rel,
        before: Some(real_content),
        op: PlannedOp::Write(patched_original),
        expected_mirror: Some((projected_rel, patched_sanitized)),
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn plan_create(
    root: &Path,
    layout: &Layout,
    config: &Config,
    conn: &rusqlite::Connection,
    options: &ApplyOptions,
    patch_text: &str,
    file_patch: &FilePatch,
    planned: &mut Vec<PlannedFileApply>,
    original_patch: &mut String,
    files: &mut Vec<String>,
) -> Result<()> {
    let agent_rel = normalize_patch_file_path(&file_patch.new_path, root, layout)
        .with_context(|| format!("create target is not inside repo: {}", file_patch.new_path))?;
    let projection = PathProjection::from_connection(config, conn)?;
    let (_, containment_candidate) = projection.real_candidate_for_new_agent_path(&agent_rel)?;
    crate::fsutil::ensure_real_path_containment(root, &containment_candidate)?;
    let rel = projection.real_for_new_agent_path(&agent_rel, config)?;
    let rel_string = normalize_rel_path(&rel);
    let display_string = normalize_rel_path(&agent_rel);
    let real_path = root.join(&rel);

    if real_path.exists() {
        return write_conflict_and_bail(
            layout,
            conn,
            options,
            patch_text,
            original_patch,
            files,
            format!("{display_string}: create target already exists; send a modify patch instead"),
        );
    }

    let created = created_content_from_patch(file_patch)
        .with_context(|| format!("build created content for {display_string}"))?;
    // Same ambiguity guard as plan_modify: a created file containing an alias
    // word would make its own mirror ambiguous.
    let terms_after = crate::sanitize::term_table(config);
    if let Some(collision) = crate::sanitize::alias_collisions(&created, &terms_after).first() {
        return write_conflict_and_bail(
            layout,
            conn,
            options,
            patch_text,
            original_patch,
            files,
            format!(
                "{display_string}: created file contains alias word {:?} (alias of {:?}); the \
                 sanitized view would be ambiguous — use a different word or change the alias",
                collision.word, collision.term
            ),
        );
    }
    // Resolve accepted symbol aliases in the new agent-authored file before it
    // becomes real source. Declarations may never reuse an existing alias;
    // references are mapped through the workspace-wide injective alias table.
    let semantic_aliases = crate::semantic_store::accepted_alias_pairs(conn)?;
    let neutral_render = sanitize_content(&rel, &created, config, &BTreeSet::new())
        .with_context(|| format!("analyze created {display_string}"))?;
    let reverse = reverse_alias_table(&neutral_render.span_map, config, &semantic_aliases);
    let mut forward_terms = crate::sanitize::term_table(config);
    forward_terms.extend(semantic_aliases.iter().map(|pair| Term {
        raw: pair.original.clone(),
        normalized: normalize_term(&pair.original),
        replacement: pair.alias.clone(),
        policy_source: "semantic-alias",
    }));
    let zones = ProseZones::new(&rel, &created);
    let real_created = reverse_map_new_text(
        &created,
        &reverse,
        &forward_terms,
        &BTreeSet::new(),
        &|offset| zones.in_prose(offset),
        &|offset| zones.in_declaration(offset),
    )
    .with_context(|| format!("back-project created {display_string}"))?;

    // The lexical part must already round-trip. Semantic-only differences are
    // resolved by the semantic reindex/compiler binding refresh after commit.
    let fresh_protected = collect_protected_identifiers(&rel, &real_created);
    let union_after = stored_protected_union_with_override(conn, &rel_string, &fresh_protected)?;
    let rendered = sanitize_content(&rel, &real_created, config, &union_after)
        .with_context(|| format!("sanitize created {display_string}"))?;
    if semantic_aliases.is_empty() && rendered.sanitized != created {
        return write_conflict_and_bail(
            layout,
            conn,
            options,
            patch_text,
            original_patch,
            files,
            format!(
                "{display_string}: created file contains sanitizable text; create already-neutral content or rename after create"
            ),
        );
    }

    original_patch.push_str(&render_file_patch(file_patch));
    files.push(normalize_rel_path(&agent_rel));
    planned.push(PlannedFileApply {
        rel,
        before: None,
        op: PlannedOp::Write(real_created),
        expected_mirror: Some((agent_rel, created)),
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn plan_delete(
    root: &Path,
    layout: &Layout,
    conn: &rusqlite::Connection,
    options: &ApplyOptions,
    patch_text: &str,
    file_patch: &FilePatch,
    planned: &mut Vec<PlannedFileApply>,
    original_patch: &mut String,
    files: &mut Vec<String>,
) -> Result<()> {
    let agent_rel = normalize_patch_file_path(&file_patch.old_path, root, layout)
        .with_context(|| format!("delete target is not inside repo: {}", file_patch.old_path))?;
    let config = Config::load_or_default(layout)?;
    let projection = PathProjection::from_connection(&config, conn)?;
    let rel = projection.real_for_agent(&agent_rel)?;
    let projected_rel = projection.projected_for_real(&rel)?;
    let rel_string = normalize_rel_path(&rel);
    let display_string = normalize_rel_path(&projected_rel);
    let real_path = root.join(&rel);
    let mirror_path = layout.mirror_dir.join(&projected_rel);
    let map_path = layout.map_path(&rel);

    let span_map = load_span_map(&map_path)
        .with_context(|| format!("load span map {}; run index first", map_path.display()))?;
    let (db_original_hash, db_sanitized_hash) = db::file_hashes(conn, &rel_string)?
        .ok_or_else(|| anyhow!("{display_string}: file is not tracked; run index first"))?;
    let real_content = fs::read_to_string(&real_path)
        .with_context(|| format!("read real file {}", real_path.display()))?;
    let mirror_content = fs::read_to_string(&mirror_path)
        .with_context(|| format!("read mirror file {}", mirror_path.display()))?;

    if sha256_hex(real_content.as_bytes()) != db_original_hash {
        return write_conflict_and_bail(
            layout,
            conn,
            options,
            patch_text,
            original_patch,
            files,
            format!("{display_string}: real file drifted since last index; run `code-sanity sync`"),
        );
    }
    let mirror_hash = sha256_hex(mirror_content.as_bytes());
    if mirror_hash != db_sanitized_hash || mirror_hash != span_map.sanitized_hash {
        return write_conflict_and_bail(
            layout,
            conn,
            options,
            patch_text,
            original_patch,
            files,
            format!(
                "{display_string}: sanitized mirror drifted since last index; run `code-sanity sync`"
            ),
        );
    }

    let patched_mirror = apply_file_patch_to_content(&mirror_content, file_patch)
        .with_context(|| format!("apply delete patch to {display_string}"))?;
    if !patched_mirror.is_empty() {
        return write_conflict_and_bail(
            layout,
            conn,
            options,
            patch_text,
            original_patch,
            files,
            format!("{display_string}: delete patch must remove the entire file"),
        );
    }

    original_patch.push_str(&whole_file_delete_patch(&rel_string, &real_content));
    files.push(normalize_rel_path(&projected_rel));
    planned.push(PlannedFileApply {
        rel,
        before: Some(real_content),
        op: PlannedOp::Delete,
        expected_mirror: None,
    });
    Ok(())
}

pub fn write_sanitized_content(
    root: &Path,
    rel_path: &Path,
    sanitized_content: &str,
) -> Result<ApplyReport> {
    let agent_rel = normalize_sanitized_rel_path(rel_path)?;
    // One exclusive lock across read-diff-apply: reading the mirror unlocked
    // would let a concurrent sync change it between the diff and the apply.
    let (layout, _lock) = init_workspace_locked(root)?;
    let config = Config::load_or_default(&layout)?;
    let conn = db::connect(&layout)?;
    db::ensure_schema(&conn)?;
    let projection = PathProjection::from_connection(&config, &conn)?;
    let real_rel = projection.real_for_agent(&agent_rel)?;
    let projected_rel = projection.projected_for_real(&real_rel)?;
    let mirror_path = layout.mirror_dir.join(&projected_rel);
    ensure_existing_path_inside(&mirror_path, &layout.mirror_dir, &projected_rel)?;
    let current = fs::read_to_string(&mirror_path).with_context(|| {
        format!(
            "read current sanitized file {}; run `code-sanity index` first",
            projected_rel.display()
        )
    })?;
    if current == sanitized_content {
        let entry = JournalEntry {
            id: new_journal_id(),
            status: JournalStatus::Success,
            session_id: None,
            agent: None,
            files: vec![normalize_rel_path(&projected_rel)],
            sanitized_patch: String::new(),
            original_patch: String::new(),
            error: None,
            created_at: Utc::now().to_rfc3339(),
            pending: None,
        };
        let journal_path = write_journal(&layout, &entry)?;
        return Ok(ApplyReport {
            files: entry.files,
            journal_path: Some(journal_path),
        });
    }
    let patch = whole_file_patch(&projected_rel, &current, sanitized_content);
    apply_patch_text_locked(root, &layout, &patch, ApplyOptions::default(), None)
}

/// Back-project an in-place edit of a mirror file to the real repo. This is the
/// primitive an editor adapter (e.g. the opencode plugin) calls after the agent
/// edits the sanitized mirror file directly: the mirror on disk now holds the
/// new sanitized content, so the baseline is refreshed from `sanitize(real)` and
/// the difference is driven through the normal patch bridge (span-aware, with
/// conflict detection). Editing a replacement span still conflicts and leaves
/// the real file untouched.
pub fn project_mirror_edit(
    root: &Path,
    rel_path: &Path,
    options: ApplyOptions,
) -> Result<ApplyReport> {
    let agent_rel = normalize_sanitized_rel_path(rel_path)?;
    // One exclusive lock across the whole read-refresh-diff-apply sequence: a
    // concurrent sync or edit can neither clobber the captured mirror edit nor
    // interleave with the baseline refresh.
    let (layout, _lock) = init_workspace_locked(root)?;
    crate::journal::ensure_no_interrupted_apply(&layout)?;
    let config = Config::load_or_default(&layout)?;
    let conn = db::connect(&layout)?;
    db::ensure_schema(&conn)?;
    let projection = PathProjection::from_connection(&config, &conn)?;
    let existing_real = projection.real_for_agent(&agent_rel).ok();
    let projected_rel = match existing_real.as_ref() {
        Some(real) => projection.projected_for_real(real)?,
        None => agent_rel.clone(),
    };
    let mirror_path = layout.mirror_dir.join(&projected_rel);
    ensure_existing_path_inside(&mirror_path, &layout.mirror_dir, &projected_rel)?;
    let new_mirror = fs::read_to_string(&mirror_path)
        .with_context(|| format!("read edited mirror {}", projected_rel.display()))?;
    let rel = match existing_real {
        Some(real) => real,
        None => projection.real_for_new_agent_path(&agent_rel, &config)?,
    };
    let rel_string = normalize_rel_path(&rel);

    let real_path = root.join(&rel);
    if !real_path.exists() {
        // The agent created a new mirror file; route through a create patch so
        // the standard "must already be neutral" create checks apply.
        let line_count = new_mirror.lines().count().max(1);
        let projected_string = normalize_rel_path(&projected_rel);
        let mut patch =
            format!("--- /dev/null\n+++ b/{projected_string}\n@@ -0,0 +1,{line_count} @@\n");
        for line in new_mirror.lines() {
            patch.push_str(&format!("+{line}\n"));
        }
        return apply_patch_text_locked(root, &layout, &patch, options, None);
    }

    // A mirror edit is only projectable against the baseline it was made on.
    // If the real file drifted since the last index, the agent edited a stale
    // view: blindly diffing against a refreshed baseline would silently revert
    // the external change. Record the edit in a conflict journal entry (the
    // durable copy), reset the mirror to sanitize(real), and refuse.
    let real_content = fs::read_to_string(&real_path)
        .with_context(|| format!("read real file {}", real_path.display()))?;
    let drifted = match db::file_hashes(&conn, &rel_string)? {
        Some((original_hash, _)) => sha256_hex(real_content.as_bytes()) != original_hash,
        None => false,
    };
    if drifted {
        index_single_file_locked(root, &layout, &rel, true)?;
        let baseline = fs::read_to_string(&mirror_path)
            .with_context(|| format!("read refreshed mirror {}", projected_rel.display()))?;
        let recorded_edit = whole_file_patch(&projected_rel, &baseline, &new_mirror);
        return write_conflict_and_bail(
            &layout,
            &conn,
            &options,
            &recorded_edit,
            "",
            std::slice::from_ref(&normalize_rel_path(&projected_rel)),
            format!(
                "{}: real file drifted since the mirror edit was made; the edit \
                 is recorded in the conflict journal and the mirror was reset to \
                 sanitize(real) — re-apply it against the fresh mirror",
                normalize_rel_path(&projected_rel)
            ),
        );
    }

    // Refresh the baseline: reindex real so the mirror on disk and the db both
    // hold sanitize(real) again. `new_mirror` was captured first, and the
    // force reset keeps a durable stash copy of it under journal/discarded/.
    let refreshed = index_single_file_locked(root, &layout, &rel, true)?;
    let baseline = fs::read_to_string(&mirror_path)
        .with_context(|| format!("read refreshed mirror {}", projected_rel.display()))?;
    if baseline == new_mirror {
        // No-op edit: record a Success journal entry for the audit trail.
        let entry = JournalEntry {
            id: new_journal_id(),
            status: JournalStatus::Success,
            session_id: options.session_id.clone(),
            agent: options.agent.clone(),
            files: vec![normalize_rel_path(&projected_rel)],
            sanitized_patch: String::new(),
            original_patch: String::new(),
            error: None,
            created_at: Utc::now().to_rfc3339(),
            pending: None,
        };
        let journal_path = write_journal(&layout, &entry)?;
        return Ok(ApplyReport {
            files: entry.files,
            journal_path: Some(journal_path),
        });
    }
    let patch = whole_file_patch(&projected_rel, &baseline, &new_mirror);
    apply_patch_text_locked(root, &layout, &patch, options, None).map_err(|err| {
        match refreshed.stashed.as_ref() {
            Some(stash) => err.context(format!(
                "the mirror was reset to sanitize(real); the edit is kept at {}",
                stash.display()
            )),
            None => err,
        }
    })
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RenameReport {
    pub apply: ApplyReport,
    pub real_from: String,
    pub sanitized_to: String,
    pub occurrences: usize,
}

/// Rename a symbol the agent sees under a sanitized alias. Editing inside a
/// replacement span via a normal patch is refused on purpose; this is the
/// sanctioned path. `from` is the sanitized identifier visible in the mirror;
/// it is reversed through the span map to the real identifier, which is then
/// renamed to `to` across the real file and reindexed. Because the real repo is
/// the source of truth, the rename lands on the real symbol, not just the alias.
pub fn rename_alias(
    root: &Path,
    rel_path: &Path,
    from: &str,
    to: &str,
    options: ApplyOptions,
) -> Result<RenameReport> {
    if from.is_empty() {
        bail!("rename source alias must not be empty");
    }
    if !is_valid_identifier(to) {
        bail!("rename target {to:?} is not a valid identifier");
    }

    let (layout, _lock) = init_workspace_locked(root)?;
    crate::journal::ensure_no_interrupted_apply(&layout)?;
    let conn = db::connect(&layout)?;
    db::ensure_schema(&conn)?;

    // Renaming a real identifier TO a configured alias would collide the
    // moment this file is reindexed (ambiguous mirror): refuse up front.
    let config_for_rename = Config::load_or_default(&layout)?;
    let to_normalized = crate::sanitize::normalize_term(to);
    if let Some(term) = crate::sanitize::term_table(&config_for_rename)
        .iter()
        .find(|term| crate::sanitize::normalize_term(&term.replacement) == to_normalized)
    {
        bail!(
            "{to:?} is already the alias of term {:?} ({}); pick a different name",
            term.raw,
            term.policy_source
        );
    }

    let agent_rel = normalize_sanitized_rel_path(rel_path)?;
    let projection = PathProjection::from_connection(&config_for_rename, &conn)?;
    let rel = projection.real_for_agent(&agent_rel)?;
    let projected_rel = projection.projected_for_real(&rel)?;
    let rel_string = normalize_rel_path(&rel);
    let display_string = normalize_rel_path(&projected_rel);
    let real_path = root.join(&rel);
    let mirror_path = layout.mirror_dir.join(&projected_rel);
    let map_path = layout.map_path(&rel);

    let span_map = load_span_map(&map_path)
        .with_context(|| format!("load span map {}; run index first", map_path.display()))?;
    let (db_original_hash, db_sanitized_hash) = db::file_hashes(&conn, &rel_string)?
        .ok_or_else(|| anyhow!("{display_string}: file is not tracked; run index first"))?;
    let real_content = fs::read_to_string(&real_path)
        .with_context(|| format!("read real file {}", real_path.display()))?;
    let mirror_content = fs::read_to_string(&mirror_path)
        .with_context(|| format!("read mirror file {}", mirror_path.display()))?;
    if sha256_hex(real_content.as_bytes()) != db_original_hash
        || sha256_hex(mirror_content.as_bytes()) != db_sanitized_hash
    {
        bail!("{display_string}: real or mirror drifted since last index; run `code-sanity sync`");
    }

    // Resolve `from` through the span map first: replacement spans record
    // exactly which real identifier each alias stands for, so a colliding
    // plain identifier with the same spelling can never be renamed by mistake.
    let alias_originals: BTreeSet<&str> = span_map
        .replacements
        .iter()
        .filter(|replacement| replacement.sanitized_text == from)
        .map(|replacement| replacement.original_text.as_str())
        .collect();
    let real_from = match alias_originals.len() {
        0 => {
            // Not an alias in this file: rename a plain (unsanitized) word.
            let (from_start, from_end) =
                find_whole_word(&mirror_content, from).ok_or_else(|| {
                    anyhow!("alias {from:?} not found as a whole word in {display_string}")
                })?;
            reverse_sanitized_region(&span_map, &mirror_content, from_start, from_end)
                .with_context(|| format!("reverse-map {from:?} in {display_string}"))?
        }
        1 => {
            let original = alias_originals.iter().next().expect("one original");
            if find_whole_word(&mirror_content, from).is_none() && !mirror_content.contains(from) {
                bail!("alias {from:?} not found in {display_string}");
            }
            (*original).to_string()
        }
        _ => bail!(
            "{display_string}: alias {from:?} is ambiguous (stands for {} different real \
             identifiers); rename is refused",
            alias_originals.len()
        ),
    };
    if real_from == to {
        bail!("{display_string}: alias {from:?} already maps to real identifier {to:?}");
    }

    let (next_real, occurrences) = replace_whole_word(&real_content, &real_from, to);
    if occurrences == 0 {
        bail!(
            "{display_string}: could not locate real identifier {real_from:?} for alias {from:?}"
        );
    }

    let original_patch = whole_file_patch(&rel, &real_content, &next_real);
    let note = format!("rename alias {from} -> {to} (real {real_from} -> {to})");
    let planned = vec![PlannedFileApply {
        rel: rel.clone(),
        before: Some(real_content),
        op: PlannedOp::Write(next_real),
        expected_mirror: None,
    }];
    let journal_path = commit_planned_apply(
        root,
        &layout,
        &conn,
        &options,
        &note,
        &original_patch,
        std::slice::from_ref(&normalize_rel_path(&projected_rel)),
        &planned,
        config_for_rename.journal.max_entries,
        None,
    )?;

    let sanitized_after = fs::read_to_string(&mirror_path).unwrap_or_default();
    let sanitized_to = find_whole_word(&sanitized_after, to)
        .map(|_| to.to_string())
        .unwrap_or_else(|| "<re-aliased>".to_string());

    Ok(RenameReport {
        apply: ApplyReport {
            files: vec![normalize_rel_path(&projected_rel)],
            journal_path: Some(journal_path),
        },
        real_from,
        sanitized_to,
        occurrences,
    })
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RecoverReport {
    pub recovered: Vec<String>,
    pub rolled_back: bool,
    /// Files whose current content matched neither the recorded snapshot nor
    /// the target; left untouched, journal entry kept in `applying`.
    pub conflicts: Vec<String>,
    /// Stranded atomic-write temp files deleted from the workspace and from
    /// directories the interrupted apply was writing into.
    pub temp_files_removed: usize,
}

/// Finish or undo any apply that was interrupted after its `applying` journal
/// entry was written but before it reached a terminal state. By default the
/// apply is replayed to its recorded `after` state (roll-forward). With
/// `rollback`, every touched file is restored to its `before` state instead.
///
/// Freshness: a file is only driven to its target when its current content
/// still matches the snapshot the journal recorded (or already equals the
/// target). Anything else means newer work landed after the crash; the file is
/// reported as a conflict and left alone unless `force` overrides.
pub fn recover_workspace(root: &Path, rollback: bool, force: bool) -> Result<RecoverReport> {
    // flock is released by the kernel when the crashed process died, so a
    // leftover lock file is harmless; recover just takes the lock normally.
    let (layout, _lock) = init_workspace_locked(root)?;
    let conn = db::connect(&layout)?;
    db::ensure_schema(&conn)?;

    let mut report = RecoverReport {
        rolled_back: rollback,
        ..RecoverReport::default()
    };
    // The crashed process may have died inside an atomic write, stranding a
    // temp file next to its target (verify reports those in the mirror as
    // untracked). Every writer runs under the exclusive lock we now hold, so
    // any temp file is dead and safe to sweep: the whole state dir plus the
    // real directories the interrupted applies were writing into.
    report.temp_files_removed += crate::fsutil::remove_stale_temp_files(&layout.state_dir)?;
    let mut swept_real_dirs = std::collections::BTreeSet::new();
    let mut protected_drift = false;
    let listing = list_journal_entries(&layout)?;
    // Corrupt entries are reported and left in place: one might be the sole
    // record of an interrupted apply, and only a human can decide whether the
    // workspace is actually consistent (`verify` still runs while blocked).
    for (corrupt_path, reason) in &listing.corrupt {
        report.conflicts.push(format!(
            "{}: journal entry cannot be parsed ({reason}); run `code-sanity verify`, \
             then move the entry (and its journal/inflight marker, if any) aside manually",
            corrupt_path.display()
        ));
    }
    for (path, mut entry) in listing.entries {
        if entry.status != JournalStatus::Applying {
            continue;
        }
        let Some(pending) = entry.pending.clone() else {
            continue;
        };
        // The journal is local state, but recover writes REAL files from it:
        // a hand-tampered entry with a `..` rel must not direct writes (or
        // temp sweeps) outside the repo. Fail closed: report, leave the entry
        // in `applying`, recover the rest.
        if let Some(bad) = pending
            .iter()
            .find(|file| normalize_safe_rel_path(Path::new(&file.rel), "repo").is_err())
        {
            report.conflicts.push(format!(
                "{}: journal entry {} has a pending path escaping the repo; \
                 refusing to recover it (inspect {} manually)",
                bad.rel,
                entry.id,
                path.display()
            ));
            continue;
        }
        // Same fail-closed rule for a symlinked directory component resolving
        // outside the repo — lexical checks cannot see it, and both the temp
        // sweep and the writes below would otherwise follow it.
        if let Some((bad, err)) = pending.iter().find_map(|file| {
            crate::fsutil::ensure_real_path_containment(root, Path::new(&file.rel))
                .err()
                .map(|err| (file, err))
        }) {
            report.conflicts.push(format!(
                "{}: journal entry {}: {err:#}; refusing to recover it \
                 (inspect {} manually)",
                bad.rel,
                entry.id,
                path.display()
            ));
            continue;
        }
        for pending_file in &pending {
            let real_dir = root
                .join(&pending_file.rel)
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| root.to_path_buf());
            if swept_real_dirs.insert(real_dir.clone()) {
                report.temp_files_removed +=
                    crate::fsutil::remove_stale_temp_files_shallow(&real_dir)?;
            }
        }
        let mut entry_conflicts = Vec::new();
        for pending_file in &pending {
            let rel = PathBuf::from(&pending_file.rel);
            let (target, target_mode, precondition) = if rollback {
                (
                    pending_file.before.as_deref(),
                    pending_file.before_mode,
                    pending_file.after.as_deref(),
                )
            } else {
                (
                    pending_file.after.as_deref(),
                    pending_file.after_mode,
                    pending_file.before.as_deref(),
                )
            };
            // Bytes, not UTF-8: a power-loss-torn file rarely decodes, and
            // recover is exactly the tool that must survive it. An
            // unreadable-but-present file is a per-entry freshness conflict
            // (overridable with --force), never a whole-run abort.
            let (current, read_error) = match fs::read(root.join(&rel)) {
                Ok(bytes) => (Some(bytes), None),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => (None, None),
                Err(err) => (None, Some(err)),
            };
            let matches = |snapshot: Option<&str>| {
                read_error.is_none() && current.as_deref() == snapshot.map(str::as_bytes)
            };
            let fresh = matches(precondition) || matches(target);
            if !fresh && !force {
                entry_conflicts.push(match &read_error {
                    Some(err) => format!(
                        "{}: cannot read current content ({err}); resolve manually or rerun \
                         with --force",
                        pending_file.rel
                    ),
                    None => format!(
                        "{}: current content matches neither the recorded snapshot nor the \
                         target; resolve manually or rerun with --force",
                        pending_file.rel
                    ),
                });
                continue;
            }
            // A write failure here stays loud: it is not a freshness question
            // (disk full, EACCES, containment), --force cannot fix it, and
            // recover is idempotent on re-run because freshness accepts
            // current == target. The entry stays `Applying` and the workspace
            // stays safely blocked.
            protected_drift |= set_file_state(root, &layout, &conn, &rel, target, target_mode)
                .with_context(|| format!("recover {}", pending_file.rel))?;
        }
        if !entry_conflicts.is_empty() {
            // Newer work landed on these files after the crash. Leave the
            // journal entry in `applying` so the workspace stays blocked until
            // a human resolves it (or reruns recover with --force).
            report.conflicts.extend(entry_conflicts);
            continue;
        }
        entry.status = if rollback {
            JournalStatus::RolledBack
        } else {
            JournalStatus::Success
        };
        entry.pending = None;
        entry.error = Some(if rollback {
            "recovered: rolled back interrupted apply".to_string()
        } else {
            "recovered: replayed interrupted apply".to_string()
        });
        write_journal(&layout, &entry)?;
        db::insert_journal_row(
            &conn,
            entry.session_id.as_deref(),
            entry.agent.as_deref(),
            &entry.sanitized_patch,
            &entry.original_patch,
            if rollback { "rolled-back" } else { "success" },
            &Utc::now().to_rfc3339(),
        )?;
        report.recovered.push(
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string(),
        );
    }
    if protected_drift {
        reconverge_workspace(root, &layout)
            .context("reindex after recovered protected symbol change")?;
    } else if !report.recovered.is_empty() {
        crate::semantic_store::index_workspace_locked(root, &layout)
            .context("refresh semantic index after recovery")?;
    }
    Ok(report)
}

/// Reconstruct a created file's content from a pure-add patch hunk.
fn created_content_from_patch(file_patch: &FilePatch) -> Result<String> {
    let mut out = String::new();
    for hunk in &file_patch.hunks {
        for line in &hunk.lines {
            match line {
                HunkLine::Add(text) => {
                    out.push_str(text);
                    out.push('\n');
                }
                HunkLine::Context(_) | HunkLine::Remove(_) => {
                    bail!("create patch must contain only added lines");
                }
            }
        }
    }
    if out.is_empty() {
        bail!("create patch adds no content");
    }
    Ok(out)
}

fn whole_file_delete_patch(rel: &str, content: &str) -> String {
    let count = content.lines().count();
    let mut out = String::new();
    out.push_str(&format!(
        "--- a/{rel}\n+++ /dev/null\n@@ -1,{count} +0,0 @@\n"
    ));
    for line in content.lines() {
        out.push_str(&format!("-{line}\n"));
    }
    out
}

struct PlannedFileApply {
    rel: PathBuf,
    /// Content before this apply, or `None` if the file is being created.
    before: Option<String>,
    op: PlannedOp,
    /// Exact agent-facing content expected after semantic reindex. Legacy
    /// renames/structured transactions may leave this unset and rely on their
    /// own preview invariants.
    expected_mirror: Option<(PathBuf, String)>,
}

enum PlannedOp {
    /// Create or modify: the file's final content.
    Write(String),
    /// Remove the file, mirror, map, and db row.
    Delete,
}

impl PlannedFileApply {
    fn after(&self) -> Option<&str> {
        match &self.op {
            PlannedOp::Write(content) => Some(content.as_str()),
            PlannedOp::Delete => None,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn commit_planned_apply(
    root: &Path,
    layout: &Layout,
    conn: &rusqlite::Connection,
    options: &ApplyOptions,
    sanitized_patch: &str,
    original_patch: &str,
    files: &[String],
    planned: &[PlannedFileApply],
    journal_max_entries: u64,
    fail_after_writes_for_test: Option<usize>,
) -> Result<PathBuf> {
    // Record the full intent (before/after per file) BEFORE touching any real
    // file. If the process dies mid-apply, this durable `applying` entry lets
    // `code-sanity recover` replay or roll back the half-finished apply.
    // Permission bits ride along: a modify keeps the file's current mode, a
    // create has none to keep (default mode; diffs carry no mode channel from
    // the mirror), and recovery re-creating a deleted file needs before_mode.
    let modes: Vec<Option<u32>> = planned
        .iter()
        .map(|planned_file| current_file_mode(&root.join(&planned_file.rel)))
        .collect();
    let pending: Vec<PendingFile> = planned
        .iter()
        .zip(&modes)
        .map(|(planned_file, mode)| PendingFile {
            rel: normalize_rel_path(&planned_file.rel),
            before: planned_file.before.clone(),
            after: planned_file.after().map(ToOwned::to_owned),
            before_mode: planned_file.before.as_ref().and(*mode),
            after_mode: planned_file.after().and(*mode),
        })
        .collect();
    let id = new_journal_id();
    let created_at = Utc::now().to_rfc3339();
    let mut entry = JournalEntry {
        id,
        status: JournalStatus::Applying,
        session_id: options.session_id.clone(),
        agent: options.agent.clone(),
        files: files.to_vec(),
        sanitized_patch: sanitized_patch.to_string(),
        original_patch: original_patch.to_string(),
        error: None,
        created_at,
        pending: Some(pending),
    };
    let journal_path = write_journal(layout, &entry)?;

    let mut applied = Vec::<usize>::new();
    let mut protected_drift = false;
    let commit_result = (|| -> Result<()> {
        for (idx, planned_file) in planned.iter().enumerate() {
            protected_drift |= set_file_state(
                root,
                layout,
                conn,
                &planned_file.rel,
                planned_file.after(),
                planned_file.after().and(modes[idx]),
            )
            .with_context(|| format!("apply {}", planned_file.rel.display()))?;
            applied.push(idx);
            // Crash-test hook: pause after the first write so a test harness
            // can SIGKILL this process deterministically mid-apply.
            if idx == 0 && std::env::var_os("CODE_SANITY_TEST_SLEEP_AFTER_FIRST_WRITE").is_some() {
                std::thread::sleep(std::time::Duration::from_secs(30));
            }
            if fail_after_writes_for_test == Some(idx + 1) {
                bail!("simulated apply failure after {} write(s)", idx + 1);
            }
        }
        let semantic = crate::semantic_store::index_workspace_locked(root, layout)
            .context("refresh semantic index after file writes")?;
        if !semantic.errors.is_empty() {
            bail!(
                "semantic projection refresh failed: {}",
                semantic.errors.join("; ")
            );
        }
        for planned_file in planned {
            if let Some((projected_rel, expected)) = &planned_file.expected_mirror {
                let actual = fs::read_to_string(layout.mirror_dir.join(projected_rel))
                    .with_context(|| {
                        format!("read projected result {}", projected_rel.display())
                    })?;
                if &actual != expected {
                    bail!(
                        "{}: committed real source does not reproduce the agent-facing edit after semantic reindex",
                        projected_rel.display()
                    );
                }
            }
        }
        Ok(())
    })();

    match commit_result {
        Ok(()) => {
            entry.status = JournalStatus::Success;
            entry.pending = None;
            write_journal(layout, &entry)?;
            db::insert_journal_row(
                conn,
                options.session_id.as_deref(),
                options.agent.as_deref(),
                sanitized_patch,
                original_patch,
                "success",
                &entry.created_at,
            )?;
            if protected_drift {
                // The repo-wide protected symbol set changed, so other files'
                // renderings are stale; reconverge before releasing the lock.
                reconverge_workspace(root, layout)
                    .context("reindex after protected symbol change")?;
            }
            // Best-effort retention sweep under the already-held lock: a
            // pruning failure must never fail an apply that already landed.
            if let Err(err) = crate::journal::prune_terminal_entries(layout, journal_max_entries)
                .and_then(|_| crate::journal::prune_discarded_stashes(layout, journal_max_entries))
                .and_then(|_| db::prune_journal_rows(conn, journal_max_entries))
            {
                log::warn!("journal pruning failed: {err:#}");
            }
            Ok(journal_path)
        }
        Err(err) => {
            let rollback = (|| -> Result<()> {
                for &idx in applied.iter().rev() {
                    let planned_file = &planned[idx];
                    protected_drift |= set_file_state(
                        root,
                        layout,
                        conn,
                        &planned_file.rel,
                        planned_file.before.as_deref(),
                        planned_file.before.as_ref().and(modes[idx]),
                    )?;
                }
                if protected_drift {
                    reconverge_workspace(root, layout)
                        .context("reindex after rolled-back protected symbol change")?;
                } else {
                    crate::semantic_store::index_workspace_locked(root, layout)
                        .context("refresh semantic index after rollback")?;
                }
                Ok(())
            })();
            rollback.with_context(|| format!("apply failed ({err}); rollback failed"))?;
            entry.status = JournalStatus::RolledBack;
            // Files are restored: the before/after snapshots are dead weight,
            // and keeping them stored full real-file contents forever.
            entry.pending = None;
            entry.error = Some(err.to_string());
            write_journal(layout, &entry)?;
            db::insert_journal_row(
                conn,
                options.session_id.as_deref(),
                options.agent.as_deref(),
                sanitized_patch,
                original_patch,
                "rolled-back",
                &entry.created_at,
            )?;
            Err(err.context("apply failed after writes; rolled back real files"))
        }
    }
}

/// Current permission bits of a regular file, or `None` when it does not
/// exist (or is not a regular file — a symlink's own mode is meaningless).
fn current_file_mode(path: &Path) -> Option<u32> {
    fs::symlink_metadata(path)
        .ok()
        .filter(fs::Metadata::is_file)
        .map(|meta| {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o7777
        })
}

/// Drive `rel` to a target state: `Some(content)` writes the real file and
/// reindexes its mirror/map/db; `None` deletes the real file plus its mirror,
/// map, and db row. This is the single primitive shared by apply, rollback,
/// and recover so every path is create/delete/modify aware. The caller must
/// hold the workspace lock. Returns whether the repo-wide protected symbol
/// set changed (the caller then owes a full reindex).
///
/// `mode`, when recorded, is authoritative for the written file's permission
/// bits: the atomic write only preserves an EXISTING target's mode, so a
/// rollback or recovery that re-creates a deleted file must restore the
/// journaled bits explicitly.
fn set_file_state(
    root: &Path,
    layout: &Layout,
    conn: &rusqlite::Connection,
    rel: &Path,
    target: Option<&str>,
    mode: Option<u32>,
) -> Result<bool> {
    // Lexical rel validation upstream cannot see a symlinked directory
    // component escaping the repo; resolve-and-contain before any mutation.
    let real_path = crate::fsutil::ensure_real_path_containment(root, rel)?;
    match target {
        Some(content) => {
            crate::fsutil::atomic_write_sync(&real_path, content)
                .with_context(|| format!("write {}", real_path.display()))?;
            if let Some(mode) = mode {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&real_path, fs::Permissions::from_mode(mode))
                    .with_context(|| format!("chmod {}", real_path.display()))?;
            }
            let indexed = index_single_file_locked(root, layout, rel, true)
                .with_context(|| format!("reindex {}", rel.display()))?;
            Ok(indexed.protected_changed)
        }
        None => {
            let rel_string = normalize_rel_path(rel);
            let projected_rel = load_span_map(&layout.map_path(rel))
                .ok()
                .and_then(|map| (!map.projected_path.is_empty()).then_some(map.projected_path))
                .map(PathBuf::from)
                .unwrap_or_else(|| rel.to_path_buf());
            let had_protected = db::all_index_states(conn)?
                .iter()
                .any(|state| state.rel_path == rel_string && !state.protected().is_empty());
            remove_file_if_exists(&real_path)?;
            remove_file_if_exists(&layout.mirror_dir.join(projected_rel))?;
            remove_file_if_exists(&layout.map_path(rel))?;
            db::remove_file(conn, &rel_string)?;
            Ok(had_protected)
        }
    }
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("remove {}", path.display())),
    }
}

fn write_conflict_and_bail<T>(
    layout: &Layout,
    conn: &rusqlite::Connection,
    options: &ApplyOptions,
    sanitized_patch: &str,
    original_patch: &str,
    files: &[String],
    error: String,
) -> Result<T> {
    let entry = JournalEntry {
        id: new_journal_id(),
        status: JournalStatus::Conflict,
        session_id: options.session_id.clone(),
        agent: options.agent.clone(),
        files: files.to_vec(),
        sanitized_patch: sanitized_patch.to_string(),
        original_patch: original_patch.to_string(),
        error: Some(error.clone()),
        created_at: Utc::now().to_rfc3339(),
        pending: None,
    };
    let journal_path = write_journal(layout, &entry)?;
    db::insert_journal_row(
        conn,
        options.session_id.as_deref(),
        options.agent.as_deref(),
        sanitized_patch,
        original_patch,
        "conflict",
        &entry.created_at,
    )?;
    Err(anyhow::Error::new(ConflictError {
        message: error,
        journal_path,
    }))
}

// Hoisted: parse_unified_patch is called per patch and (via fuzzing) at very
// high frequency; compiling the regex per call dominates the pure-parse cost.
static HUNK_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@").unwrap()
});

fn parse_unified_patch(input: &str) -> Result<UnifiedPatch> {
    let mut lines = input.lines().peekable();
    let mut files = Vec::new();
    let hunk_re = &*HUNK_RE;

    while let Some(line) = lines.next() {
        if !line.starts_with("--- ") {
            continue;
        }
        let old_path = parse_patch_path(line, "--- ")?;
        let Some(next) = lines.next() else {
            bail!("patch header for {old_path} is missing +++ line");
        };
        if !next.starts_with("+++ ") {
            bail!("patch header for {old_path} has invalid +++ line: {next}");
        }
        let new_path = parse_patch_path(next, "+++ ")?;
        let mut hunks = Vec::new();

        while let Some(peek) = lines.peek().copied() {
            if peek.starts_with("--- ") {
                break;
            }
            if !peek.starts_with("@@ ") {
                lines.next();
                continue;
            }
            let header = lines.next().unwrap();
            let Some(captures) = hunk_re.captures(header) else {
                bail!("invalid hunk header: {header}");
            };
            let old_start = captures[1].parse::<usize>()?;
            let old_count = captures
                .get(2)
                .map(|m| m.as_str().parse::<usize>())
                .transpose()?
                .unwrap_or(1);
            let new_start = captures[3].parse::<usize>()?;
            let new_count = captures
                .get(4)
                .map(|m| m.as_str().parse::<usize>())
                .transpose()?
                .unwrap_or(1);
            // Consume exactly the number of lines the header declares. Peeking
            // for the next "@@ "/"--- " marker instead would misparse content
            // that legitimately starts with those bytes (a removed SQL/Lua
            // `-- comment` renders as `--- comment`).
            let mut hunk_lines = Vec::new();
            let mut remaining_old = old_count;
            let mut remaining_new = new_count;
            while remaining_old > 0 || remaining_new > 0 {
                let Some(hunk_line) = lines.next() else {
                    bail!(
                        "hunk @@ -{old_start},{old_count} +{new_start},{new_count} @@ \
                         ends before its declared line counts are satisfied"
                    );
                };
                if hunk_line.starts_with('\\') {
                    // "\ No newline at end of file" annotates the previous line.
                    continue;
                }
                let take = |n: &mut usize| -> Result<()> {
                    *n = n.checked_sub(1).ok_or_else(|| {
                        anyhow!("hunk line counts exceed header at {hunk_line:?}")
                    })?;
                    Ok(())
                };
                // Split off the marker as a CHAR, not a byte: a line starting
                // with a multi-byte character (agent output, stripped prefix)
                // must be a parse error, not a slice panic (fuzz finding).
                let mut chars = hunk_line.chars();
                let Some(prefix) = chars.next() else {
                    // Some tools strip the trailing space from empty context lines.
                    take(&mut remaining_old)?;
                    take(&mut remaining_new)?;
                    hunk_lines.push(HunkLine::Context(String::new()));
                    continue;
                };
                let content = chars.as_str().to_string();
                match prefix {
                    ' ' => {
                        take(&mut remaining_old)?;
                        take(&mut remaining_new)?;
                        hunk_lines.push(HunkLine::Context(content));
                    }
                    '+' => {
                        take(&mut remaining_new)?;
                        hunk_lines.push(HunkLine::Add(content));
                    }
                    '-' => {
                        take(&mut remaining_old)?;
                        hunk_lines.push(HunkLine::Remove(content));
                    }
                    other => bail!("invalid hunk line prefix {other:?}"),
                }
            }
            // A trailing no-newline marker for the hunk's last line.
            while lines.peek().is_some_and(|line| line.starts_with('\\')) {
                lines.next();
            }
            hunks.push(Hunk {
                old_start,
                old_count,
                new_start,
                new_count,
                lines: hunk_lines,
            });
        }

        files.push(FilePatch {
            old_path,
            new_path,
            hunks,
        });
    }

    Ok(UnifiedPatch { files })
}

/// Extract the path from a `---`/`+++` header line. POSIX diff separates an
/// optional timestamp with a TAB, so split there — splitting on any
/// whitespace silently truncated `dir with space/f.rs` to `dir`, misrouting
/// the patch. Paths with embedded whitespace or git-style quoting are refused
/// loudly instead of guessed at.
fn parse_patch_path(line: &str, prefix: &str) -> Result<String> {
    let rest = line.strip_prefix(prefix).unwrap_or(line);
    let path = rest.split('\t').next().unwrap_or(rest).trim_end();
    if path.is_empty() {
        bail!("empty patch path line: {line}");
    }
    if path.starts_with('"') {
        bail!("quoted patch paths are not supported: {line}");
    }
    if path.chars().any(char::is_whitespace) {
        bail!(
            "patch path contains whitespace: {path:?} (paths with spaces are \
             unsupported; timestamps must be tab-separated)"
        );
    }
    Ok(path.to_string())
}

fn normalize_patch_file_path(path: &str, root: &Path, layout: &Layout) -> Result<PathBuf> {
    if path == "/dev/null" {
        // Reachable only when BOTH sides of a header are /dev/null (classify
        // maps a one-sided /dev/null to create/delete before this runs).
        bail!("/dev/null is not a patch target (malformed patch header)");
    }
    let mut candidate = PathBuf::from(path);
    if candidate.is_absolute() {
        if let Ok(stripped) = candidate.strip_prefix(&layout.mirror_dir) {
            candidate = stripped.to_path_buf();
        } else if let Ok(stripped) = candidate.strip_prefix(root) {
            candidate = stripped.to_path_buf();
        } else {
            bail!("absolute patch path is outside repo: {path}");
        }
    }
    let mut components = candidate.components();
    if let Some(Component::Normal(first)) = components.next() {
        if first == "a" || first == "b" {
            candidate = components.as_path().to_path_buf();
        }
    }
    let mirror_prefix = Path::new(".code-sanity").join("mirror");
    if let Ok(stripped) = candidate.strip_prefix(&mirror_prefix) {
        candidate = stripped.to_path_buf();
    }
    if candidate.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        bail!("patch path escapes repo: {path}");
    }
    normalize_safe_rel_path(&candidate, "repo")
}

/// 0-based cursor of the first old-file line a hunk touches. For a pure
/// insertion (old_count == 0) the unified-diff convention is "insert AFTER
/// line old_start", so the cursor sits one line further down; `@@ -0,0 ...`
/// inserts at the top of the file.
fn hunk_anchor_line(hunk: &Hunk) -> usize {
    if hunk.old_count == 0 {
        hunk.old_start
    } else {
        hunk.old_start.saturating_sub(1)
    }
}

/// 0-based new-file cursor of the first line a hunk produces.
fn hunk_new_anchor_line(hunk: &Hunk) -> usize {
    if hunk.new_count == 0 {
        hunk.new_start
    } else {
        hunk.new_start.saturating_sub(1)
    }
}

/// The dominant line terminator of `content`: "\r\n" when MORE than half of
/// the newlines are CRLF, else "\n" (an exact tie prefers LF — the native
/// ending on the supported platforms). Added lines adopt it so a patch does
/// not mix endings into a CRLF file.
fn dominant_eol(content: &str) -> &'static str {
    let total = content.matches('\n').count();
    let crlf = content.matches("\r\n").count();
    if total > 0 && crlf * 2 > total {
        "\r\n"
    } else {
        "\n"
    }
}

/// Hunk-line content without a trailing '\r' (a diff of a CRLF file carries
/// the CR inside the line content).
fn line_body_text(text: &str) -> &str {
    text.strip_suffix('\r').unwrap_or(text)
}

/// Byte ranges of the old content this patch edits, at line and sub-line
/// granularity: a removed line paired with its added replacement narrows to
/// their changed byte range, and a pure insertion contributes an empty range
/// at its insertion point. A purely deleted line contributes nothing — its
/// spans are removed whole, which is safe, unlike a partial span edit.
/// Disjoint edits inside one hunk stay disjoint, so an untouched alias
/// between them no longer reads as edited.
fn changed_ranges(content: &str, file_patch: &FilePatch) -> Result<Vec<(usize, usize)>> {
    let line_starts = line_starts(content);
    let lines = split_lines(content);
    let mut ranges = Vec::new();
    for hunk in &file_patch.hunks {
        let mut line_idx = hunk_anchor_line(hunk);
        let mut removed: std::collections::VecDeque<(usize, usize)> =
            std::collections::VecDeque::new();
        for line in &hunk.lines {
            match line {
                HunkLine::Context(_) => {
                    removed.clear();
                    line_idx += 1;
                }
                HunkLine::Remove(_) => {
                    let start = byte_for_line(&line_starts, content.len(), line_idx + 1);
                    let body_len = lines
                        .get(line_idx)
                        .map(|line| line_body(line).len())
                        .unwrap_or(0);
                    removed.push_back((start, start + body_len));
                    line_idx += 1;
                }
                HunkLine::Add(text) => {
                    if let Some((start, end)) = removed.pop_front() {
                        let old_body = &content[start..end];
                        let (local_start, local_end) =
                            common_changed_range(old_body, line_body_text(text));
                        ranges.push((start + local_start, start + local_end));
                    } else {
                        let start = byte_for_line(&line_starts, content.len(), line_idx + 1);
                        ranges.push((start, start));
                    }
                }
            }
        }
    }
    Ok(ranges)
}

fn apply_file_patch_to_content(content: &str, file_patch: &FilePatch) -> Result<String> {
    let lines = split_lines(content);
    let eol = dominant_eol(content);
    let mut out = Vec::<String>::new();
    let mut cursor = 0usize;

    for hunk in &file_patch.hunks {
        let start_idx = hunk_anchor_line(hunk);
        if start_idx < cursor {
            bail!("overlapping hunks at line {}", hunk.old_start);
        }
        if start_idx > lines.len() {
            bail!("hunk starts past end of file at line {}", hunk.old_start);
        }
        out.extend(lines[cursor..start_idx].iter().cloned());
        cursor = start_idx;

        for line in &hunk.lines {
            match line {
                HunkLine::Context(expected) => {
                    let actual = lines
                        .get(cursor)
                        .ok_or_else(|| anyhow!("missing context line {}", cursor + 1))?;
                    if line_body(actual) != line_body_text(expected) {
                        bail!(
                            "context mismatch at line {}: expected {:?}, got {:?}",
                            cursor + 1,
                            line_body_text(expected),
                            line_body(actual)
                        );
                    }
                    out.push(actual.clone());
                    cursor += 1;
                }
                HunkLine::Remove(expected) => {
                    let actual = lines
                        .get(cursor)
                        .ok_or_else(|| anyhow!("missing remove line {}", cursor + 1))?;
                    if line_body(actual) != line_body_text(expected) {
                        bail!(
                            "remove mismatch at line {}: expected {:?}, got {:?}",
                            cursor + 1,
                            line_body_text(expected),
                            line_body(actual)
                        );
                    }
                    cursor += 1;
                }
                HunkLine::Add(content) => {
                    // A context/remove line at EOF may lack a trailing newline
                    // (kept verbatim above); appending after it must not merge
                    // the two lines. The parser drops `\ No newline` markers,
                    // so bridge output is newline-normalized by design.
                    if let Some(last) = out.last_mut() {
                        if !last.ends_with('\n') {
                            last.push_str(eol);
                        }
                    }
                    out.push(format!("{}{eol}", line_body_text(content)));
                }
            }
        }
    }

    out.extend(lines[cursor..].iter().cloned());
    Ok(out.concat())
}

#[derive(Debug, Clone)]
struct AliasRange {
    start: usize,
    end: usize,
    sanitized_text: String,
    original_text: String,
}

/// Reverse alias table for new (Add-line) text: normalized alias text mapped
/// to the real original it stands for. Built from this file's span map plus
/// the global alias registry. An alias observed with two different originals
/// is ambiguous; using it in new text is a conflict.
struct ReverseAliases {
    /// Term list shaped for `hits_in_run` (replacement = representative
    /// original text).
    terms: Vec<Term>,
    /// Normalized originals per entry, aligned with `terms`; len > 1 means
    /// ambiguous.
    originals: Vec<BTreeSet<String>>,
}

/// Back-project one agent-authored fragment using the full projected document
/// as syntax context. Structured edits use this before planning real-source
/// byte edits, so aliases in references resolve while strings/comments stay
/// verbatim and declarations cannot capture an existing projected name.
pub(crate) fn back_project_agent_fragment(
    conn: &rusqlite::Connection,
    config: &Config,
    rel_path: &Path,
    projected_document: &str,
    fragment_start: usize,
    fragment_end: usize,
) -> Result<String> {
    if fragment_start > fragment_end
        || fragment_end > projected_document.len()
        || !projected_document.is_char_boundary(fragment_start)
        || !projected_document.is_char_boundary(fragment_end)
    {
        bail!("agent replacement has an invalid projected UTF-8 range");
    }
    let lexical = sanitize_content(rel_path, projected_document, config, &BTreeSet::new())?;
    let semantic_aliases = crate::semantic_store::accepted_alias_pairs(conn)?;
    let reverse = reverse_alias_table(&lexical.span_map, config, &semantic_aliases);
    let mut terms = term_table(config);
    terms.extend(semantic_aliases.iter().map(|pair| Term {
        raw: pair.original.clone(),
        normalized: normalize_term(&pair.original),
        replacement: pair.alias.clone(),
        policy_source: "semantic-alias",
    }));
    let zones = ProseZones::new(rel_path, projected_document);
    reverse_map_new_text(
        &projected_document[fragment_start..fragment_end],
        &reverse,
        &terms,
        &BTreeSet::new(),
        &|offset| zones.in_prose(fragment_start + offset),
        &|offset| zones.in_declaration(fragment_start + offset),
    )
}

fn reverse_alias_table(
    span_map: &SpanMap,
    config: &Config,
    semantic_aliases: &[crate::semantic_store::SemanticAliasPair],
) -> ReverseAliases {
    let mut by_alias: BTreeMap<String, (String, String, BTreeSet<String>)> = BTreeMap::new();
    let mut add = |alias: &str, original: &str| {
        let key = normalize_term(alias);
        if key.is_empty() || key == normalize_term(original) {
            return;
        }
        let entry = by_alias
            .entry(key)
            .or_insert_with(|| (alias.to_string(), original.to_string(), BTreeSet::new()));
        entry.2.insert(normalize_term(original));
    };
    for replacement in &span_map.replacements {
        add(&replacement.sanitized_text, &replacement.original_text);
    }
    for (term, alias) in &config.sanitizer.alias_registry {
        add(alias, term);
    }
    for pair in semantic_aliases {
        add(&pair.alias, &pair.original);
    }

    let mut terms = Vec::with_capacity(by_alias.len());
    let mut originals = Vec::with_capacity(by_alias.len());
    for (normalized, (raw_alias, original, normalized_originals)) in by_alias {
        terms.push(Term {
            raw: raw_alias,
            normalized,
            replacement: original,
            policy_source: "reverse-alias",
        });
        originals.push(normalized_originals);
    }
    ReverseAliases { terms, originals }
}

/// Reverse-map aliases inside one line of newly added text. Every word run is
/// scanned with the same primitive the sanitizer uses; a reversal is kept only
/// if sanitizing the reversed run reproduces the run the agent wrote (the
/// run-level roundtrip filter), so an innocent identifier that merely contains
/// an alias-looking substring is left alone instead of being corrupted.
///
/// `in_prose` receives each run's byte offset within `text`: a run inside a
/// comment or string literal of the patched mirror is plain language, not a
/// symbol reference, and is never rewritten into a real term.
fn reverse_map_new_text(
    text: &str,
    reverse: &ReverseAliases,
    terms: &[Term],
    protected: &BTreeSet<String>,
    in_prose: &dyn Fn(usize) -> bool,
    in_declaration: &dyn Fn(usize) -> bool,
) -> Result<String> {
    if reverse.terms.is_empty() {
        return Ok(text.to_string());
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    let mut hits = Vec::new();
    for (run_start, run_end) in word_runs(text) {
        out.push_str(&text[cursor..run_start]);
        cursor = run_end;
        let run = &text[run_start..run_end];
        if in_prose(run_start) {
            out.push_str(run);
            continue;
        }
        hits.clear();
        hits_in_run(run, 0, &reverse.terms, &mut hits);
        hits.sort_by(|a, b| {
            a.start
                .cmp(&b.start)
                .then_with(|| (b.end - b.start).cmp(&(a.end - a.start)))
        });
        if !hits.is_empty() && in_declaration(run_start) {
            bail!(
                "alias {:?} is used as a new declaration; choose a fresh neutral name instead of reusing an existing projected symbol",
                reverse.terms[hits[0].term_index].raw
            );
        }
        if protected.contains(run) {
            out.push_str(run);
            continue;
        }
        let mut reversed = String::with_capacity(run.len());
        let mut run_cursor = 0usize;
        for hit in &hits {
            if hit.start < run_cursor {
                continue;
            }
            if reverse.originals[hit.term_index].len() > 1 {
                bail!(
                    "alias {:?} in added text is ambiguous (multiple originals); \
                     rewrite the line without it or resolve the alias registry",
                    reverse.terms[hit.term_index].raw
                );
            }
            reversed.push_str(&run[run_cursor..hit.start]);
            reversed.push_str(&adapt_replacement(
                &run[hit.start..hit.end],
                &reverse.terms[hit.term_index].replacement,
            ));
            run_cursor = hit.end;
        }
        reversed.push_str(&run[run_cursor..]);

        if reversed != run && sanitize_run_text(&reversed, terms, protected) == run {
            out.push_str(&reversed);
        } else {
            out.push_str(run);
        }
    }
    out.push_str(&text[cursor..]);
    Ok(out)
}

/// Comment/string zones of the patched mirror plus its line offsets, used to
/// keep reverse mapping out of prose in added lines.
struct ProseZones {
    strings: Vec<crate::sanitize::ByteRange>,
    comments: Vec<crate::sanitize::ByteRange>,
    declarations: Vec<crate::sanitize::ByteRange>,
    line_starts: Vec<usize>,
    len: usize,
}

impl ProseZones {
    fn new(rel_path: &Path, patched_sanitized: &str) -> Self {
        let language = crate::sanitize::detect_language(rel_path, patched_sanitized);
        let strings = crate::sanitize::string_ranges(&language, patched_sanitized);
        let comments = crate::sanitize::comment_ranges(&language, patched_sanitized, &strings);
        let declarations = crate::semantic::parse_document(rel_path, patched_sanitized)
            .map(|document| {
                document
                    .occurrences
                    .into_iter()
                    .filter(|occurrence| {
                        occurrence.role == crate::semantic::OccurrenceRole::Declaration
                    })
                    .map(|occurrence| crate::sanitize::ByteRange {
                        start: occurrence.range.start_byte,
                        end: occurrence.range.end_byte,
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            strings,
            comments,
            declarations,
            line_starts: line_starts(patched_sanitized),
            len: patched_sanitized.len(),
        }
    }

    fn in_prose(&self, offset: usize) -> bool {
        crate::sanitize::range_contains(&self.strings, offset)
            || crate::sanitize::range_contains(&self.comments, offset)
    }

    fn in_declaration(&self, offset: usize) -> bool {
        crate::sanitize::range_contains(&self.declarations, offset)
    }

    fn line_offset(&self, one_based_line: usize) -> usize {
        byte_for_line(&self.line_starts, self.len, one_based_line)
    }
}

fn translate_file_patch(
    file_patch: &FilePatch,
    span_map: &SpanMap,
    sanitized_content: &str,
    patched_sanitized: &str,
    config: &Config,
    semantic_aliases: &[crate::semantic_store::SemanticAliasPair],
    protected: &BTreeSet<String>,
) -> Result<FilePatch> {
    let starts = line_starts(sanitized_content);
    let reverse = reverse_alias_table(span_map, config, semantic_aliases);
    let mut terms = term_table(config);
    terms.extend(semantic_aliases.iter().map(|pair| Term {
        raw: pair.original.clone(),
        normalized: normalize_term(&pair.original),
        replacement: pair.alias.clone(),
        policy_source: "semantic-alias",
    }));
    let prose = ProseZones::new(Path::new(&span_map.rel_path), patched_sanitized);
    let hunks = file_patch
        .hunks
        .iter()
        .map(|hunk| {
            translate_hunk(
                hunk,
                span_map,
                sanitized_content,
                &starts,
                &reverse,
                &terms,
                protected,
                &prose,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(FilePatch {
        old_path: file_patch.old_path.clone(),
        new_path: file_patch.new_path.clone(),
        hunks,
    })
}

/// Translate one hunk line by line. Context/Remove lines translate the alias
/// ranges recorded for their exact mirror line; an added line paired with a
/// removed line (a modification) inherits that line's alias ranges projected
/// through the pair's changed byte range, then reverse-maps any remaining
/// alias words the agent wrote. Per-line pairing keeps disjoint edits in one
/// hunk independent — CRLF or a length change in one line no longer skews
/// alias offsets for the rest of the hunk.
#[allow(clippy::too_many_arguments)]
fn translate_hunk(
    hunk: &Hunk,
    span_map: &SpanMap,
    sanitized_content: &str,
    line_starts: &[usize],
    reverse: &ReverseAliases,
    terms: &[Term],
    protected: &BTreeSet<String>,
    prose: &ProseZones,
) -> Result<Hunk> {
    let mut old_line = hunk_anchor_line(hunk);
    let mut new_line = hunk_new_anchor_line(hunk);
    let mut removed: std::collections::VecDeque<(String, Vec<AliasRange>)> =
        std::collections::VecDeque::new();
    let mut lines = Vec::with_capacity(hunk.lines.len());

    for line in &hunk.lines {
        match line {
            HunkLine::Context(text) => {
                removed.clear();
                let (body, ranges) =
                    line_alias_ranges(span_map, sanitized_content, line_starts, old_line)?;
                if line_body_text(text) != body {
                    bail!(
                        "context mismatch at sanitized line {}: expected {:?}, got {:?}",
                        old_line + 1,
                        line_body_text(text),
                        body
                    );
                }
                let (translated, _) = translate_known_alias_ranges(body, &ranges)?;
                lines.push(HunkLine::Context(translated));
                old_line += 1;
                new_line += 1;
            }
            HunkLine::Remove(text) => {
                let (body, ranges) =
                    line_alias_ranges(span_map, sanitized_content, line_starts, old_line)?;
                if line_body_text(text) != body {
                    bail!(
                        "remove mismatch at sanitized line {}: expected {:?}, got {:?}",
                        old_line + 1,
                        line_body_text(text),
                        body
                    );
                }
                let (translated, _) = translate_known_alias_ranges(body, &ranges)?;
                removed.push_back((body.to_string(), ranges));
                lines.push(HunkLine::Remove(translated));
                old_line += 1;
            }
            HunkLine::Add(text) => {
                let body = line_body_text(text);
                let projected = match removed.pop_front() {
                    Some((old_body, old_ranges)) => {
                        project_line_alias_ranges(&old_body, body, &old_ranges)?
                    }
                    None => Vec::new(),
                };
                let (translated, splices) = translate_known_alias_ranges(body, &projected)?;
                // Newly added text may use aliases the agent saw in the mirror
                // (whole words or inside identifiers); map them back to the
                // real names so the real file stays semantically coherent.
                let line_abs = prose.line_offset(new_line + 1);
                let translated = reverse_map_new_text(
                    &translated,
                    reverse,
                    terms,
                    protected,
                    &|offset| prose.in_prose(line_abs + body_offset_for(&splices, offset)),
                    &|offset| prose.in_declaration(line_abs + body_offset_for(&splices, offset)),
                )?;
                lines.push(HunkLine::Add(translated));
                new_line += 1;
            }
        }
    }

    Ok(Hunk {
        old_start: hunk.old_start,
        old_count: hunk.old_count,
        new_start: hunk.new_start,
        new_count: hunk.new_count,
        lines,
    })
}

/// The body text of 0-based `line_idx` in the sanitized content plus the alias
/// ranges falling inside it, rebased to line-local offsets.
fn line_alias_ranges<'content>(
    span_map: &SpanMap,
    sanitized_content: &'content str,
    starts: &[usize],
    line_idx: usize,
) -> Result<(&'content str, Vec<AliasRange>)> {
    let len = sanitized_content.len();
    let line_start = byte_for_line(starts, len, line_idx + 1);
    let line_end = byte_after_lines(starts, len, line_idx + 1, 1);
    let body = line_body(&sanitized_content[line_start..line_end]);
    let body_end = line_start + body.len();
    let mut ranges = Vec::new();
    for replacement in &span_map.replacements {
        if replacement.sanitized_start >= line_start && replacement.sanitized_end <= body_end {
            ranges.push(AliasRange {
                start: replacement.sanitized_start - line_start,
                end: replacement.sanitized_end - line_start,
                sanitized_text: replacement.sanitized_text.clone(),
                original_text: replacement.original_text.clone(),
            });
        }
    }
    Ok((body, ranges))
}

/// Project the alias ranges of a removed line onto its paired added line:
/// ranges in the unchanged prefix keep their offsets, ranges in the unchanged
/// suffix shift by the length delta, and a range overlapping the changed
/// middle is a refused span edit.
fn project_line_alias_ranges(
    old_body: &str,
    new_body: &str,
    ranges: &[AliasRange],
) -> Result<Vec<AliasRange>> {
    let (changed_start, changed_old_end) = common_changed_range(old_body, new_body);
    let mut projected = Vec::with_capacity(ranges.len());
    for range in ranges {
        let (start, end) = if range.end <= changed_start {
            (range.start, range.end)
        } else if range.start >= changed_old_end {
            (
                new_body.len() - (old_body.len() - range.start),
                new_body.len() - (old_body.len() - range.end),
            )
        } else {
            bail!("patch changes sanitized replacement span");
        };
        if start > end || end > new_body.len() {
            bail!("projected alias range is outside patched line");
        }
        projected.push(AliasRange {
            start,
            end,
            sanitized_text: range.sanitized_text.clone(),
            original_text: range.original_text.clone(),
        });
    }
    Ok(projected)
}

/// A splice performed by `translate_known_alias_ranges`: everything after
/// `translated_end` in the translated text sits `delta` bytes away from its
/// position in the pre-translation body.
struct Splice {
    translated_end: usize,
    delta: isize,
}

/// Map an offset in the translated text back to the pre-translation body.
fn body_offset_for(splices: &[Splice], translated_offset: usize) -> usize {
    let mut delta = 0isize;
    for splice in splices {
        if splice.translated_end <= translated_offset {
            delta = splice.delta;
        } else {
            break;
        }
    }
    (translated_offset as isize - delta).max(0) as usize
}

/// Splice each line-local alias range's original text into `text`, verifying
/// the range still holds the recorded sanitized text. Returns the translated
/// line and the splice offsets (for mapping translated offsets back to the
/// input body).
fn translate_known_alias_ranges(
    text: &str,
    ranges: &[AliasRange],
) -> Result<(String, Vec<Splice>)> {
    let mut sorted: Vec<&AliasRange> = ranges.iter().collect();
    sorted.sort_by_key(|range| range.start);
    let mut cursor = 0usize;
    let mut out = String::with_capacity(text.len());
    let mut splices = Vec::new();
    let mut delta = 0isize;
    for range in sorted {
        if range.end > text.len() {
            bail!("replacement span is outside line bounds");
        }
        if !text.is_char_boundary(range.start) || !text.is_char_boundary(range.end) {
            bail!("replacement span is not on UTF-8 boundaries");
        }
        let actual = &text[range.start..range.end];
        if actual != range.sanitized_text {
            bail!(
                "replacement span mismatch: expected {:?}, got {:?}",
                range.sanitized_text,
                actual
            );
        }
        out.push_str(&text[cursor..range.start]);
        out.push_str(&range.original_text);
        cursor = range.end;
        delta += range.original_text.len() as isize - (range.end - range.start) as isize;
        splices.push(Splice {
            translated_end: out.len(),
            delta,
        });
    }
    out.push_str(&text[cursor..]);
    Ok((out, splices))
}

fn render_file_patch(file_patch: &FilePatch) -> String {
    let mut out = String::new();
    out.push_str(&format!("--- {}\n", file_patch.old_path));
    out.push_str(&format!("+++ {}\n", file_patch.new_path));
    for hunk in &file_patch.hunks {
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
        ));
        for line in &hunk.lines {
            match line {
                HunkLine::Context(text) => out.push_str(&format!(" {text}\n")),
                HunkLine::Add(text) => out.push_str(&format!("+{text}\n")),
                HunkLine::Remove(text) => out.push_str(&format!("-{text}\n")),
            }
        }
    }
    out
}

/// A unified diff between two versions of one file with localized hunks (a
/// real line diff, not one whole-file hunk), so disjoint edits stay
/// independent for conflict checks and per-line alias projection. Rendered by
/// hand from the diff ops so hunk headers follow the exact `diff -U`
/// conventions our parser and applier implement (including the
/// insert-after-line form `-N,0`).
fn whole_file_patch(rel_path: &Path, old: &str, new: &str) -> String {
    let rel = normalize_rel_path(rel_path);
    let diff = similar::TextDiff::from_lines(old, new);
    let mut out = format!("--- a/{rel}\n+++ b/{rel}\n");
    for group in diff.grouped_ops(3) {
        if group.is_empty() {
            continue;
        }
        // Hunk extents from the whole group: op index bookkeeping is not
        // monotonic for degenerate diffs, so first/last ranges cannot be
        // trusted, but starts and consumed-line sums always can.
        let old_start = group
            .iter()
            .map(|op| op.old_range().start)
            .min()
            .unwrap_or(0);
        let old_count: usize = group.iter().map(|op| op.old_range().len()).sum();
        let new_start = group
            .iter()
            .map(|op| op.new_range().start)
            .min()
            .unwrap_or(0);
        let new_count: usize = group.iter().map(|op| op.new_range().len()).sum();
        out.push_str(&format!(
            "@@ -{},{old_count} +{},{new_count} @@\n",
            if old_count == 0 {
                old_start
            } else {
                old_start + 1
            },
            if new_count == 0 {
                new_start
            } else {
                new_start + 1
            },
        ));
        for op in &group {
            for change in diff.iter_changes(op) {
                let sign = match change.tag() {
                    similar::ChangeTag::Equal => ' ',
                    similar::ChangeTag::Delete => '-',
                    similar::ChangeTag::Insert => '+',
                };
                let value = change.value();
                out.push(sign);
                out.push_str(value.strip_suffix('\n').unwrap_or(value));
                out.push('\n');
            }
        }
    }
    out
}

fn split_lines(content: &str) -> Vec<String> {
    content
        .split_inclusive('\n')
        .map(ToOwned::to_owned)
        .collect()
}

fn line_body(line: &str) -> &str {
    let without_lf = line.strip_suffix('\n').unwrap_or(line);
    without_lf.strip_suffix('\r').unwrap_or(without_lf)
}

fn is_ident_char(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

fn is_valid_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) if first == '_' || first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(is_ident_char)
}

/// Find the first occurrence of `word` in `hay` bounded by non-identifier
/// characters on both sides (a whole-token match).
fn find_whole_word(hay: &str, word: &str) -> Option<(usize, usize)> {
    if word.is_empty() {
        return None;
    }
    let mut from = 0usize;
    while let Some(rel) = hay[from..].find(word) {
        let start = from + rel;
        let end = start + word.len();
        let before_ok = hay[..start]
            .chars()
            .next_back()
            .is_none_or(|ch| !is_ident_char(ch));
        let after_ok = hay[end..]
            .chars()
            .next()
            .is_none_or(|ch| !is_ident_char(ch));
        if before_ok && after_ok {
            return Some((start, end));
        }
        from = end;
    }
    None
}

/// Replace every whole-word occurrence of `target` with `replacement`, returning
/// the new text and the number of replacements made.
fn replace_whole_word(content: &str, target: &str, replacement: &str) -> (String, usize) {
    if target.is_empty() {
        return (content.to_string(), 0);
    }
    let mut out = String::with_capacity(content.len());
    let mut cursor = 0usize;
    let mut count = 0usize;
    while let Some(rel) = content[cursor..].find(target) {
        let start = cursor + rel;
        let end = start + target.len();
        let before_ok = content[..start]
            .chars()
            .next_back()
            .is_none_or(|ch| !is_ident_char(ch));
        let after_ok = content[end..]
            .chars()
            .next()
            .is_none_or(|ch| !is_ident_char(ch));
        if before_ok && after_ok {
            out.push_str(&content[cursor..start]);
            out.push_str(replacement);
            cursor = end;
            count += 1;
        } else {
            out.push_str(&content[cursor..end]);
            cursor = end;
        }
    }
    out.push_str(&content[cursor..]);
    (out, count)
}

/// Reconstruct the original text of a sanitized byte range by splicing each
/// fully-contained replacement's original text back in; unchanged regions in a
/// span map already hold identical bytes in both views.
///
/// Span maps are read from disk (`.code-sanity/maps/*.json`) and may be
/// hand-edited or corrupt: every offset is validated (bounds, ordering,
/// UTF-8 boundaries) so a broken map is an error, never a slice panic.
fn reverse_sanitized_region(
    span_map: &SpanMap,
    mirror: &str,
    start: usize,
    end: usize,
) -> Result<String> {
    let mut replacements: Vec<_> = span_map
        .replacements
        .iter()
        .filter(|replacement| {
            replacement.sanitized_start >= start && replacement.sanitized_end <= end
        })
        .collect();
    replacements.sort_by_key(|replacement| replacement.sanitized_start);

    let mut out = String::new();
    let mut cursor = start;
    for replacement in replacements {
        if replacement.sanitized_end < replacement.sanitized_start {
            bail!(
                "span map replacement {:?} has an inverted range",
                replacement.sanitized_text
            );
        }
        if replacement.sanitized_start < cursor {
            bail!(
                "span map replacement {:?} overlaps a previous span",
                replacement.sanitized_text
            );
        }
        if replacement.sanitized_end > mirror.len()
            || !mirror.is_char_boundary(replacement.sanitized_start)
            || !mirror.is_char_boundary(replacement.sanitized_end)
        {
            bail!(
                "span map replacement {:?} is outside content bounds or off a \
                 UTF-8 boundary",
                replacement.sanitized_text
            );
        }
        out.push_str(&mirror[cursor..replacement.sanitized_start]);
        out.push_str(&replacement.original_text);
        cursor = replacement.sanitized_end;
    }
    out.push_str(&mirror[cursor..end]);
    Ok(out)
}

fn line_starts(content: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (idx, byte) in content.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(idx + 1);
        }
    }
    starts
}

fn byte_for_line(starts: &[usize], len: usize, one_based_line: usize) -> usize {
    if one_based_line == 0 {
        return 0;
    }
    starts.get(one_based_line - 1).copied().unwrap_or(len)
}

fn byte_after_lines(starts: &[usize], len: usize, one_based_line: usize, count: usize) -> usize {
    if count == 0 {
        return byte_for_line(starts, len, one_based_line);
    }
    let start_idx = one_based_line.saturating_sub(1);
    starts.get(start_idx + count).copied().unwrap_or(len)
}

/// Fuzzing surface (`--features fuzzing`, used by the `fuzz/` crate and the
/// corpus replay test): the private parser and the pure applier, with the
/// invariants a fuzzer can falsify asserted inside. Not part of the public
/// API.
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub mod fuzz_api {
    /// Parse arbitrary input; only panics are findings. Accepted output must
    /// be internally consistent (the counted-hunk contract: the parser
    /// consumes exactly `old_count` old-side and `new_count` new-side lines)
    /// and parsing must be deterministic.
    pub fn parse(input: &str) {
        let Ok(first) = super::parse_unified_patch(input) else {
            return;
        };
        let second = super::parse_unified_patch(input)
            .expect("parse is nondeterministic: accepted input rejected on the second run");
        assert_eq!(
            format!("{first:?}"),
            format!("{second:?}"),
            "parse is nondeterministic"
        );
        for file in &first.files {
            for hunk in &file.hunks {
                let old_side = hunk
                    .lines
                    .iter()
                    .filter(|line| {
                        matches!(
                            line,
                            super::HunkLine::Context(_) | super::HunkLine::Remove(_)
                        )
                    })
                    .count();
                let new_side = hunk
                    .lines
                    .iter()
                    .filter(|line| {
                        matches!(line, super::HunkLine::Context(_) | super::HunkLine::Add(_))
                    })
                    .count();
                assert_eq!(
                    old_side, hunk.old_count,
                    "accepted hunk breaks the old-count contract ({})",
                    file.old_path
                );
                assert_eq!(
                    new_side, hunk.new_count,
                    "accepted hunk breaks the new-count contract ({})",
                    file.new_path
                );
            }
        }
    }

    /// Parse `patch` and run the pure applier over `content` for every file
    /// patch. Apply errors are expected outcomes; only panics are findings
    /// (anchor math, CRLF handling, splice offsets).
    pub fn parse_and_apply(content: &str, patch: &str) {
        let Ok(parsed) = super::parse_unified_patch(patch) else {
            return;
        };
        for file_patch in &parsed.files {
            let _ = super::apply_file_patch_to_content(content, file_patch);
        }
    }

    /// Split a `fuzz_apply_patch` seed into (content, patch) at the first
    /// line holding exactly `%%%`. Byte-level seeds stay hand-writable and
    /// replayable (an Arbitrary-encoded tuple would be neither); input
    /// without the marker fuzzes the applier against empty content.
    pub fn split_apply_seed(data: &[u8]) -> (String, String) {
        let text = String::from_utf8_lossy(data);
        // A seed that STARTS with the marker is an empty-content seed (the
        // `\n%%%\n` form needs a preceding content line).
        if let Some(patch) = text.strip_prefix("%%%\n") {
            return (String::new(), patch.to_string());
        }
        match text.split_once("\n%%%\n") {
            Some((content, patch)) => (format!("{content}\n"), patch.to_string()),
            None => (String::new(), text.into_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzz_corpus_replays_clean() {
        // Every committed corpus seed (including any future crash artifacts
        // promoted to seeds) must pass the fuzz invariants on stable, so a
        // fuzz finding becomes a permanent regression test.
        let corpus =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus/fuzz_parse_patch");
        let mut seeds = 0;
        for entry in std::fs::read_dir(&corpus).expect("fuzz corpus directory is missing") {
            let path = entry.unwrap().path();
            let input = String::from_utf8_lossy(&std::fs::read(&path).unwrap()).into_owned();
            fuzz_api::parse(&input);
            fuzz_api::parse_and_apply("fn alpha() -> usize {\n    1\n}\n", &input);
            seeds += 1;
        }
        assert!(seeds >= 8, "corpus seeds missing (found {seeds})");

        // The apply corpus replays through the exact split the fuzz target
        // uses, so apply-side findings become permanent regressions too.
        let apply_corpus =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus/fuzz_apply_patch");
        let mut apply_seeds = 0;
        for entry in std::fs::read_dir(&apply_corpus).expect("apply corpus directory is missing") {
            let data = std::fs::read(entry.unwrap().path()).unwrap();
            let (content, patch) = fuzz_api::split_apply_seed(&data);
            fuzz_api::parse_and_apply(&content, &patch);
            apply_seeds += 1;
        }
        assert!(
            apply_seeds >= 8,
            "apply corpus seeds missing (found {apply_seeds})"
        );
    }

    #[test]
    fn multibyte_hunk_line_prefix_is_an_error_not_a_panic() {
        // Fuzz finding: `hunk_line[1..]` panicked when a hunk line began with
        // a multi-byte UTF-8 character (not a valid ' '/'+'/'-' marker).
        let err = parse_unified_patch(
            "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,1 +1,1 @@\n\u{e9}fn x() {}\n",
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("invalid hunk line prefix"),
            "{err:#}"
        );
        // A multi-byte character AFTER the marker is legitimate content.
        let parsed = parse_unified_patch(
            "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,1 +1,1 @@\n-\u{e9}t\u{e9}\n+ete\n",
        )
        .unwrap();
        assert_eq!(parsed.files.len(), 1);
    }

    #[test]
    fn parses_and_applies_patch() {
        let patch = parse_unified_patch(
            "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,2 @@\n fn neutral_parser() {\n-    1\n+    2\n }\n",
        )
        .unwrap();
        assert_eq!(patch.files.len(), 1);
        let next =
            apply_file_patch_to_content("fn neutral_parser() {\n    1\n}\n", &patch.files[0])
                .unwrap();
        assert_eq!(next, "fn neutral_parser() {\n    2\n}\n");
    }

    #[test]
    fn changed_range_allows_adjacent_insert() {
        let patch = parse_unified_patch(
            "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,1 +1,1 @@\n-fn neutral_parser() {}\n+fn neutral_parser(input: &str) {}\n",
        )
        .unwrap();
        let ranges = changed_ranges("fn neutral_parser() {}\n", &patch.files[0]).unwrap();
        assert_eq!(ranges.len(), 1);
        assert!(ranges[0].0 >= "fn neutral_parser(".len());
    }

    #[test]
    fn insertion_hunk_lands_after_its_anchor_line() {
        // Golden convention from `diff -U0`: "@@ -1,0 +2 @@" inserts AFTER
        // line 1 of the old file.
        let patch =
            parse_unified_patch("--- a/f.txt\n+++ b/f.txt\n@@ -1,0 +2 @@\n+inserted\n").unwrap();
        let next = apply_file_patch_to_content("one\ntwo\nthree\n", &patch.files[0]).unwrap();
        assert_eq!(next, "one\ninserted\ntwo\nthree\n");

        let top =
            parse_unified_patch("--- a/f.txt\n+++ b/f.txt\n@@ -0,0 +1,2 @@\n+x\n+y\n").unwrap();
        let next = apply_file_patch_to_content("one\n", &top.files[0]).unwrap();
        assert_eq!(next, "x\ny\none\n");
    }

    #[test]
    fn insertion_hunk_changed_range_is_empty_at_insertion_point() {
        let patch =
            parse_unified_patch("--- a/f.txt\n+++ b/f.txt\n@@ -1,0 +2 @@\n+inserted\n").unwrap();
        let ranges = changed_ranges("one\ntwo\n", &patch.files[0]).unwrap();
        assert_eq!(ranges, vec![(4, 4)]);
    }

    #[test]
    fn removed_sql_comment_lines_parse_by_count_not_marker() {
        // A removed `-- comment` renders as `--- comment`; counting hunk lines
        // from the header keeps it inside the hunk.
        let patch_text =
            "--- a/q.sql\n+++ b/q.sql\n@@ -1,3 +1,2 @@\n select 1;\n--- drop me\n select 2;\n";
        let patch = parse_unified_patch(patch_text).unwrap();
        assert_eq!(patch.files.len(), 1);
        assert_eq!(patch.files[0].hunks.len(), 1);
        let next =
            apply_file_patch_to_content("select 1;\n-- drop me\nselect 2;\n", &patch.files[0])
                .unwrap();
        assert_eq!(next, "select 1;\nselect 2;\n");
    }

    #[test]
    fn empty_context_lines_without_leading_space_are_tolerated() {
        let patch_text = "--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@\n a\n\n-b\n+c\n";
        let patch = parse_unified_patch(patch_text).unwrap();
        let next = apply_file_patch_to_content("a\n\nb\n", &patch.files[0]).unwrap();
        assert_eq!(next, "a\n\nc\n");
    }

    #[test]
    fn hunk_line_count_mismatch_is_rejected() {
        let err = parse_unified_patch("--- a/f\n+++ b/f\n@@ -1,2 +1,1 @@\n-a\n").unwrap_err();
        assert!(err.to_string().contains("ends before"), "{err:#}");
    }

    #[test]
    fn crlf_added_lines_adopt_the_dominant_eol() {
        let patch = parse_unified_patch(
            "--- a/f.txt\n+++ b/f.txt\n@@ -1,2 +1,3 @@\n alpha\n-beta\n+gamma\n+delta\n",
        )
        .unwrap();
        let next = apply_file_patch_to_content("alpha\r\nbeta\r\n", &patch.files[0]).unwrap();
        assert_eq!(next, "alpha\r\ngamma\r\ndelta\r\n");
    }

    #[test]
    fn dominant_eol_tie_prefers_lf() {
        assert_eq!(dominant_eol("a\r\nb\n"), "\n"); // exact 50/50 tie
        assert_eq!(dominant_eol("a\r\nb\r\nc\n"), "\r\n");
        assert_eq!(dominant_eol("a\nb\n"), "\n");
        assert_eq!(dominant_eol(""), "\n");
    }

    #[test]
    fn append_after_file_without_trailing_newline_keeps_lines_separate() {
        // Context line is the final unterminated line: the added line must
        // not merge into it.
        let patch =
            parse_unified_patch("--- a/f.txt\n+++ b/f.txt\n@@ -1,2 +1,3 @@\n a\n b\n+c\n").unwrap();
        let next = apply_file_patch_to_content("a\nb", &patch.files[0]).unwrap();
        assert_eq!(next, "a\nb\nc\n");

        // Same shape via remove+add at EOF.
        let patch = parse_unified_patch("--- a/f.txt\n+++ b/f.txt\n@@ -2,1 +2,2 @@\n-b\n+b2\n+c\n")
            .unwrap();
        let next = apply_file_patch_to_content("a\nb", &patch.files[0]).unwrap();
        assert_eq!(next, "a\nb2\nc\n");
    }

    #[test]
    fn parse_patch_path_handles_tabs_spaces_and_quotes() {
        // POSIX timestamp after a TAB is dropped.
        assert_eq!(
            parse_patch_path("--- a/src/f.rs\t2026-01-01 00:00:00", "--- ").unwrap(),
            "a/src/f.rs"
        );
        // Spaces inside the path are refused, not truncated to the wrong file.
        let err = parse_patch_path("--- a/dir with space/f.rs", "--- ").unwrap_err();
        assert!(err.to_string().contains("whitespace"), "{err:#}");
        let err = parse_patch_path("--- \"a/quoted path\"", "--- ").unwrap_err();
        assert!(err.to_string().contains("quoted"), "{err:#}");
    }

    #[test]
    fn dev_null_on_both_sides_is_a_clear_error() {
        // Parsing tolerates it (classify handles one-sided /dev/null); the
        // path normalizer is where a both-sides /dev/null target surfaces.
        let repo = tempfile::tempdir().unwrap();
        let layout = Layout::new(repo.path());
        let err = normalize_patch_file_path("/dev/null", repo.path(), &layout).unwrap_err();
        assert!(
            format!("{err:#}").contains("malformed patch header"),
            "{err:#}"
        );
    }

    #[test]
    fn reverse_sanitized_region_bails_on_corrupt_span_maps() {
        use crate::map::{Replacement, SpanMap};
        let base = SpanMap {
            rel_path: "f.rs".into(),
            projected_path: "f.rs".into(),
            original_hash: String::new(),
            sanitized_hash: String::new(),
            original_size: 0,
            sanitized_size: 0,
            language: "rust".into(),
            replacements: vec![Replacement {
                id: 0,
                category: "identifier".into(),
                original_text: "real".into(),
                sanitized_text: "alias".into(),
                confidence: 1.0,
                policy_source: "static-dictionary".into(),
                stable_key: "k".into(),
                original_start: 0,
                original_end: 4,
                original_line_start: 1,
                sanitized_start: 0,
                sanitized_end: 5,
                sanitized_line_start: 1,
            }],
            spans: Vec::new(),
            updated_at: String::new(),
        };
        let mirror = "alias here";
        assert_eq!(
            reverse_sanitized_region(&base, mirror, 0, mirror.len()).unwrap(),
            "real here"
        );

        // Inverted range (end < start).
        let mut inverted = base.clone();
        inverted.replacements[0].sanitized_start = 5;
        inverted.replacements[0].sanitized_end = 2;
        let err = reverse_sanitized_region(&inverted, mirror, 0, mirror.len()).unwrap_err();
        assert!(format!("{err:#}").contains("inverted"), "{err:#}");

        // Overlapping spans.
        let mut overlapping = base.clone();
        overlapping.replacements.push(Replacement {
            sanitized_start: 3,
            sanitized_end: 7,
            ..overlapping.replacements[0].clone()
        });
        let err = reverse_sanitized_region(&overlapping, mirror, 0, mirror.len()).unwrap_err();
        assert!(format!("{err:#}").contains("overlaps"), "{err:#}");

        // Off a UTF-8 boundary.
        let mut off_boundary = base.clone();
        off_boundary.replacements[0].sanitized_start = 1;
        off_boundary.replacements[0].sanitized_end = 3;
        let unicode = "жalias";
        let err = reverse_sanitized_region(&off_boundary, unicode, 0, unicode.len());
        assert!(err.is_err(), "non-boundary span must error, not panic");
    }

    #[test]
    fn crlf_patch_content_with_carriage_returns_matches_lf_file() {
        let patch = parse_unified_patch(
            "--- a/f.txt\n+++ b/f.txt\n@@ -1,2 +1,2 @@\n alpha\r\n-beta\r\n+gamma\r\n",
        )
        .unwrap();
        let next = apply_file_patch_to_content("alpha\nbeta\n", &patch.files[0]).unwrap();
        assert_eq!(next, "alpha\ngamma\n");
    }

    #[test]
    fn disjoint_edits_in_one_hunk_produce_disjoint_ranges() {
        let patch = parse_unified_patch(
            "--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@\n-aaa\n+axa\n keep\n-bbb\n+bxb\n",
        )
        .unwrap();
        let content = "aaa\nkeep\nbbb\n";
        let ranges = changed_ranges(content, &patch.files[0]).unwrap();
        assert_eq!(ranges.len(), 2);
        let keep_start = content.find("keep").unwrap();
        let keep_end = keep_start + "keep".len();
        for (start, end) in ranges {
            assert!(
                end <= keep_start || start >= keep_end,
                "range {start}..{end} covers the untouched middle line"
            );
        }
    }

    #[test]
    fn whole_file_patch_localizes_disjoint_edits_into_separate_hunks() {
        let old: String = (1..=20).map(|i| format!("line{i}\n")).collect();
        let new = old
            .replace("line2\n", "edited2\n")
            .replace("line18\n", "edited18\n");
        let patch_text = whole_file_patch(Path::new("f.txt"), &old, &new);
        let parsed = parse_unified_patch(&patch_text).unwrap();
        assert_eq!(parsed.files[0].hunks.len(), 2, "{patch_text}");
        let applied = apply_file_patch_to_content(&old, &parsed.files[0]).unwrap();
        assert_eq!(applied, new);
    }

    #[test]
    fn rename_resolves_alias_via_span_map() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        // "clientele" shares a prefix with the alias but is a different word
        // run; rename must target the real symbol behind the alias only.
        std::fs::write(
            repo.path().join("src/a.rs"),
            "const S: &str = \"clientele says\";\nfn acme() -> usize {\n    1\n}\n",
        )
        .unwrap();
        let layout = crate::index::init_workspace(repo.path()).unwrap();
        let mut config = Config::load_or_default(&layout).unwrap();
        config.sanitizer.dictionary =
            std::collections::BTreeMap::from([("acme".to_string(), "client".to_string())]);
        config.save(&layout).unwrap();
        crate::index::index_workspace(repo.path()).unwrap();
        rename_alias(
            repo.path(),
            Path::new("src/a.rs"),
            "client",
            "fetcher",
            ApplyOptions::default(),
        )
        .unwrap();
        let real = std::fs::read_to_string(repo.path().join("src/a.rs")).unwrap();
        assert!(real.contains("fn fetcher()"), "{real}");
        assert!(real.contains("\"clientele says\""), "{real}");
        assert!(crate::verify::verify_workspace(repo.path()).is_ok());
    }

    #[test]
    fn index_fails_when_real_repo_contains_alias_word() {
        // The old ambiguity: dictionary acme -> client while the repo itself
        // contains the word "client". This is now a hard collision error at
        // index time (naming term, alias, and colliding word), not a silently
        // ambiguous mirror.
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("src/a.rs"),
            "const S: &str = \"client says\";\nfn acme() -> usize {\n    1\n}\n",
        )
        .unwrap();
        let layout = crate::index::init_workspace(repo.path()).unwrap();
        let mut config = Config::load_or_default(&layout).unwrap();
        config.sanitizer.dictionary =
            std::collections::BTreeMap::from([("acme".to_string(), "client".to_string())]);
        config.save(&layout).unwrap();
        let err = crate::index::index_workspace(repo.path()).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("client"), "{message}");
        assert!(message.contains("acme"), "{message}");
        assert!(message.contains("ambiguous"), "{message}");
    }

    #[test]
    fn non_injective_registry_is_refused_at_save() {
        // Two terms -> one alias used to be caught only at rename time (the
        // runtime "ambiguous" bail is now an unreached defense); config.save
        // refuses to persist it in the first place.
        let repo = tempfile::tempdir().unwrap();
        let layout = crate::index::init_workspace(repo.path()).unwrap();
        let mut config = Config::load_or_default(&layout).unwrap();
        config.sanitizer.dictionary = std::collections::BTreeMap::from([
            ("acme".to_string(), "shared".to_string()),
            ("corpx".to_string(), "shared".to_string()),
        ]);
        let err = config.save(&layout).unwrap_err();
        assert!(format!("{err:#}").contains("used for both"), "{err:#}");
    }

    #[test]
    fn rename_target_must_not_be_an_existing_alias() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(repo.path().join("src/a.rs"), "fn acme() {}\nfn util() {}\n").unwrap();
        let layout = crate::index::init_workspace(repo.path()).unwrap();
        let mut config = Config::load_or_default(&layout).unwrap();
        config.sanitizer.dictionary =
            std::collections::BTreeMap::from([("acme".to_string(), "gadget".to_string())]);
        config.save(&layout).unwrap();
        crate::index::index_workspace(repo.path()).unwrap();
        let err = rename_alias(
            repo.path(),
            Path::new("src/a.rs"),
            "util",
            "gadget",
            ApplyOptions::default(),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("already the alias"), "{err:#}");
    }

    proptest::proptest! {
        #[test]
        fn diff_parse_apply_roundtrips(
            old_lines in proptest::collection::vec("[a-z ]{0,8}", 0..30),
            new_lines in proptest::collection::vec("[a-z ]{0,8}", 0..30),
        ) {
            let old: String = old_lines.iter().map(|line| format!("{line}\n")).collect();
            let new: String = new_lines.iter().map(|line| format!("{line}\n")).collect();
            let patch_text = whole_file_patch(Path::new("f.txt"), &old, &new);
            if old != new {
                let parsed = parse_unified_patch(&patch_text).unwrap();
                proptest::prop_assert_eq!(parsed.files.len(), 1);
                let applied = apply_file_patch_to_content(&old, &parsed.files[0]).unwrap();
                proptest::prop_assert_eq!(applied, new);
            }
        }
    }

    #[test]
    fn rollback_restores_real_files_after_late_multi_file_failure() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("src/a.rs"),
            "fn safe_a() -> usize {\n    1\n}\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("src/b.rs"),
            "fn safe_b() -> usize {\n    1\n}\n",
        )
        .unwrap();
        crate::index::index_workspace(repo.path()).unwrap();
        let before_a = std::fs::read_to_string(repo.path().join("src/a.rs")).unwrap();
        let before_b = std::fs::read_to_string(repo.path().join("src/b.rs")).unwrap();

        let patch = "\
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,3 @@
 fn safe_a() -> usize {
-    1
+    2
 }
--- a/src/b.rs
+++ b/src/b.rs
@@ -1,3 +1,3 @@
 fn safe_b() -> usize {
-    1
+    2
 }
";
        let err = apply_patch_text_with_failure_after_writes(repo.path(), patch, 1).unwrap_err();
        assert!(err.to_string().contains("rolled back real files"));
        assert_eq!(
            std::fs::read_to_string(repo.path().join("src/a.rs")).unwrap(),
            before_a
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("src/b.rs")).unwrap(),
            before_b
        );
        assert!(crate::verify::verify_workspace(repo.path()).is_ok());

        // The rolled-back entry must not keep full file snapshots forever:
        // `pending` is cleared once the files are restored, and its in-flight
        // marker is gone.
        let layout = Layout::new(repo.path());
        let listing = crate::journal::list_journal_entries(&layout).unwrap();
        let (_, rolled_back) = listing
            .entries
            .iter()
            .find(|(_, entry)| entry.status == crate::journal::JournalStatus::RolledBack)
            .expect("rolled-back journal entry");
        assert!(
            rolled_back.pending.is_none(),
            "rollback kept before/after snapshots"
        );
        let markers = std::fs::read_dir(layout.journal_dir.join("inflight"))
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(markers, 0, "rollback left an in-flight marker");
    }
}
