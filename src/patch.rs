use crate::config::{Config, Layout, normalize_rel_path, normalize_safe_rel_path};
use crate::db;
use crate::index::{
    index_single_file, index_single_file_locked, index_workspace_locked, init_workspace,
    stored_protected_union_with_override,
};
use crate::journal::{
    JournalEntry, JournalStatus, PendingFile, list_journal_entries, new_journal_id, write_journal,
};
use crate::lock::WorkspaceLock;
use crate::map::{SpanMap, common_changed_range, load_span_map, sha256_hex};
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

#[derive(Debug, Clone)]
pub struct ApplyReport {
    pub files: Vec<String>,
    pub journal_path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct ApplyOptions {
    pub session_id: Option<String>,
    pub agent: Option<String>,
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
    let layout = init_workspace(root)?;
    let config = Config::load_or_default(&layout)?;
    let conn = db::connect(&layout)?;
    db::init_schema(&conn)?;
    let _lock = WorkspaceLock::acquire(&layout)?;

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
                &layout,
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
                &layout,
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
                &layout,
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

    let journal_path = commit_planned_apply(
        root,
        &layout,
        &conn,
        &options,
        patch_text,
        &original_patch,
        &files,
        &planned,
        fail_after_writes_for_test,
    )?;

    Ok(ApplyReport {
        files,
        journal_path,
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
    let rel = normalize_patch_file_path(&file_patch.new_path, root, layout)
        .or_else(|_| normalize_patch_file_path(&file_patch.old_path, root, layout))
        .with_context(|| {
            format!(
                "patch paths are not inside sanitized mirror or repo: {} -> {}",
                file_patch.old_path, file_patch.new_path
            )
        })?;
    let rel_string = normalize_rel_path(&rel);
    let real_path = root.join(&rel);
    let mirror_path = layout.mirror_dir.join(&rel);
    let map_path = layout.map_path(&rel);

    let span_map = load_span_map(&map_path)
        .with_context(|| format!("load span map {}; run index first", map_path.display()))?;
    let (db_original_hash, db_sanitized_hash) = db::file_hashes(conn, &rel_string)?
        .ok_or_else(|| anyhow!("{rel_string}: file is not tracked; run index first"))?;
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
            format!("{rel_string}: real file drifted since last index; run `code-sanity sync`"),
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
                "{rel_string}: sanitized mirror drifted since last index; run `code-sanity sync`"
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
                    "{rel_string}: patch edits sanitized replacement span at bytes {start}..{end}; automatic apply refused"
                ),
            );
        }
    }

    let patched_sanitized = apply_file_patch_to_content(&mirror_content, file_patch)
        .with_context(|| format!("apply sanitized patch to {rel_string}"))?;
    let stored_union = crate::index::stored_protected_union(conn)?;
    let original_file_patch = match translate_file_patch(
        file_patch,
        &span_map,
        &mirror_content,
        config,
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
                format!("{rel_string}: {err:#}"),
            );
        }
    };
    let patched_original = apply_file_patch_to_content(&real_content, &original_file_patch)
        .with_context(|| format!("apply translated patch to {rel_string}"))?;
    // Sanitize with the protected union that will hold AFTER this file lands,
    // exactly what the post-apply reindex of this file will use.
    let fresh_protected = collect_protected_identifiers(&patched_original);
    let union_after = stored_protected_union_with_override(conn, &rel_string, &fresh_protected)?;
    let rendered_after = sanitize_content(&rel, &patched_original, config, &union_after)
        .with_context(|| format!("resanitize patched {rel_string}"))?;
    if rendered_after.sanitized != patched_sanitized {
        return write_conflict_and_bail(
            layout,
            conn,
            options,
            patch_text,
            &render_file_patch(&original_file_patch),
            files,
            format!(
                "{rel_string}: translated patch does not preserve sanitize(real) == patched mirror invariant"
            ),
        );
    }
    // Bidirectional invariant: reverse-projecting the patched mirror through
    // the fresh span map must reproduce the patched real file byte-for-byte.
    let reverse_projected = reverse_sanitized_region(
        &rendered_after.span_map,
        &rendered_after.sanitized,
        0,
        rendered_after.sanitized.len(),
    );
    if reverse_projected != patched_original {
        return write_conflict_and_bail(
            layout,
            conn,
            options,
            patch_text,
            &render_file_patch(&original_file_patch),
            files,
            format!(
                "{rel_string}: reverse projection of patched mirror does not reproduce patched real file"
            ),
        );
    }

    original_patch.push_str(&render_file_patch(&original_file_patch));
    files.push(rel_string);
    planned.push(PlannedFileApply {
        rel,
        before: Some(real_content),
        op: PlannedOp::Write(patched_original),
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
    let rel = normalize_patch_file_path(&file_patch.new_path, root, layout)
        .with_context(|| format!("create target is not inside repo: {}", file_patch.new_path))?;
    let rel_string = normalize_rel_path(&rel);
    let real_path = root.join(&rel);

    if real_path.exists() {
        return write_conflict_and_bail(
            layout,
            conn,
            options,
            patch_text,
            original_patch,
            files,
            format!("{rel_string}: create target already exists; send a modify patch instead"),
        );
    }

    let created = created_content_from_patch(file_patch)
        .with_context(|| format!("build created content for {rel_string}"))?;
    // A created file's patch is written against the mirror, so the added lines
    // become the real file directly. The real repo stays the source of truth,
    // so the new file must already be neutral: sanitize(real) must equal the
    // content the agent sent, otherwise what they see after create would drift.
    let fresh_protected = collect_protected_identifiers(&created);
    let union_after = stored_protected_union_with_override(conn, &rel_string, &fresh_protected)?;
    let rendered = sanitize_content(&rel, &created, config, &union_after)
        .with_context(|| format!("sanitize created {rel_string}"))?;
    if rendered.sanitized != created {
        return write_conflict_and_bail(
            layout,
            conn,
            options,
            patch_text,
            original_patch,
            files,
            format!(
                "{rel_string}: created file contains sanitizable text; create already-neutral content or rename after create"
            ),
        );
    }

    original_patch.push_str(&render_file_patch(file_patch));
    files.push(rel_string);
    planned.push(PlannedFileApply {
        rel,
        before: None,
        op: PlannedOp::Write(created),
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
    let rel = normalize_patch_file_path(&file_patch.old_path, root, layout)
        .with_context(|| format!("delete target is not inside repo: {}", file_patch.old_path))?;
    let rel_string = normalize_rel_path(&rel);
    let real_path = root.join(&rel);
    let mirror_path = layout.mirror_dir.join(&rel);
    let map_path = layout.map_path(&rel);

    let span_map = load_span_map(&map_path)
        .with_context(|| format!("load span map {}; run index first", map_path.display()))?;
    let (db_original_hash, db_sanitized_hash) = db::file_hashes(conn, &rel_string)?
        .ok_or_else(|| anyhow!("{rel_string}: file is not tracked; run index first"))?;
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
            format!("{rel_string}: real file drifted since last index; run `code-sanity sync`"),
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
                "{rel_string}: sanitized mirror drifted since last index; run `code-sanity sync`"
            ),
        );
    }

    let patched_mirror = apply_file_patch_to_content(&mirror_content, file_patch)
        .with_context(|| format!("apply delete patch to {rel_string}"))?;
    if !patched_mirror.is_empty() {
        return write_conflict_and_bail(
            layout,
            conn,
            options,
            patch_text,
            original_patch,
            files,
            format!("{rel_string}: delete patch must remove the entire file"),
        );
    }

    original_patch.push_str(&whole_file_delete_patch(&rel_string, &real_content));
    files.push(rel_string);
    planned.push(PlannedFileApply {
        rel,
        before: Some(real_content),
        op: PlannedOp::Delete,
    });
    Ok(())
}

pub fn write_sanitized_content(
    root: &Path,
    rel_path: &Path,
    sanitized_content: &str,
) -> Result<ApplyReport> {
    let rel_path = normalize_sanitized_rel_path(rel_path)?;
    let layout = Layout::new(root);
    let mirror_path = layout.mirror_dir.join(&rel_path);
    ensure_existing_path_inside(&mirror_path, &layout.mirror_dir, &rel_path)?;
    let current = fs::read_to_string(&mirror_path).with_context(|| {
        format!(
            "read current sanitized file {}; run `code-sanity index` first",
            rel_path.display()
        )
    })?;
    if current == sanitized_content {
        let layout = init_workspace(root)?;
        let entry = JournalEntry {
            id: new_journal_id(),
            status: JournalStatus::Success,
            session_id: None,
            agent: None,
            files: vec![normalize_rel_path(&rel_path)],
            sanitized_patch: String::new(),
            original_patch: String::new(),
            error: None,
            created_at: Utc::now().to_rfc3339(),
            pending: None,
        };
        let journal_path = write_journal(&layout, &entry)?;
        return Ok(ApplyReport {
            files: entry.files,
            journal_path,
        });
    }
    let patch = whole_file_patch(&rel_path, &current, sanitized_content);
    apply_patch_text(root, &patch)
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
    let rel = normalize_sanitized_rel_path(rel_path)?;
    let layout = Layout::new(root);
    let mirror_path = layout.mirror_dir.join(&rel);
    ensure_existing_path_inside(&mirror_path, &layout.mirror_dir, &rel)?;
    let new_mirror = fs::read_to_string(&mirror_path)
        .with_context(|| format!("read edited mirror {}", rel.display()))?;

    let real_path = root.join(&rel);
    if !real_path.exists() {
        // The agent created a new mirror file; route through a create patch so
        // the standard "must already be neutral" create checks apply.
        let rel_string = normalize_rel_path(&rel);
        let line_count = new_mirror.lines().count().max(1);
        let mut patch = format!("--- /dev/null\n+++ b/{rel_string}\n@@ -0,0 +1,{line_count} @@\n");
        for line in new_mirror.lines() {
            patch.push_str(&format!("+{line}\n"));
        }
        return apply_patch_text_with_options(root, &patch, options);
    }

    // Refresh the baseline: reindex real so the mirror on disk and the db both
    // hold sanitize(real) again. `new_mirror` was captured first, so the agent's
    // edit is preserved.
    index_single_file(root, &rel)?;
    let baseline = fs::read_to_string(&mirror_path)
        .with_context(|| format!("read refreshed mirror {}", rel.display()))?;
    if baseline == new_mirror {
        return write_sanitized_content(root, &rel, &new_mirror);
    }
    let patch = whole_file_patch(&rel, &baseline, &new_mirror);
    apply_patch_text_with_options(root, &patch, options)
}

#[derive(Debug, Clone)]
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

    let layout = init_workspace(root)?;
    let conn = db::connect(&layout)?;
    db::init_schema(&conn)?;
    let _lock = WorkspaceLock::acquire(&layout)?;

    let rel = normalize_sanitized_rel_path(rel_path)?;
    let rel_string = normalize_rel_path(&rel);
    let real_path = root.join(&rel);
    let mirror_path = layout.mirror_dir.join(&rel);
    let map_path = layout.map_path(&rel);

    let span_map = load_span_map(&map_path)
        .with_context(|| format!("load span map {}; run index first", map_path.display()))?;
    let (db_original_hash, db_sanitized_hash) = db::file_hashes(&conn, &rel_string)?
        .ok_or_else(|| anyhow!("{rel_string}: file is not tracked; run index first"))?;
    let real_content = fs::read_to_string(&real_path)
        .with_context(|| format!("read real file {}", real_path.display()))?;
    let mirror_content = fs::read_to_string(&mirror_path)
        .with_context(|| format!("read mirror file {}", mirror_path.display()))?;
    if sha256_hex(real_content.as_bytes()) != db_original_hash
        || sha256_hex(mirror_content.as_bytes()) != db_sanitized_hash
    {
        bail!("{rel_string}: real or mirror drifted since last index; run `code-sanity sync`");
    }

    let (from_start, from_end) = find_whole_word(&mirror_content, from)
        .ok_or_else(|| anyhow!("alias {from:?} not found as a whole word in {rel_string}"))?;
    let real_from = reverse_sanitized_region(&span_map, &mirror_content, from_start, from_end);
    if real_from == to {
        bail!("{rel_string}: alias {from:?} already maps to real identifier {to:?}");
    }

    let (next_real, occurrences) = replace_whole_word(&real_content, &real_from, to);
    if occurrences == 0 {
        bail!("{rel_string}: could not locate real identifier {real_from:?} for alias {from:?}");
    }

    let original_patch = whole_file_patch(&rel, &real_content, &next_real);
    let note = format!("rename alias {from} -> {to} (real {real_from} -> {to})");
    let planned = vec![PlannedFileApply {
        rel: rel.clone(),
        before: Some(real_content),
        op: PlannedOp::Write(next_real),
    }];
    let journal_path = commit_planned_apply(
        root,
        &layout,
        &conn,
        &options,
        &note,
        &original_patch,
        std::slice::from_ref(&rel_string),
        &planned,
        None,
    )?;

    let sanitized_after = fs::read_to_string(&mirror_path).unwrap_or_default();
    let sanitized_to = find_whole_word(&sanitized_after, to)
        .map(|_| to.to_string())
        .unwrap_or_else(|| "<re-aliased>".to_string());

    Ok(RenameReport {
        apply: ApplyReport {
            files: vec![rel_string],
            journal_path,
        },
        real_from,
        sanitized_to,
        occurrences,
    })
}

#[derive(Debug, Clone, Default)]
pub struct RecoverReport {
    pub recovered: Vec<String>,
    pub rolled_back: bool,
}

/// Finish or undo any apply that was interrupted after its `applying` journal
/// entry was written but before it reached a terminal state. By default the
/// apply is replayed to its recorded `after` state (roll-forward). With
/// `rollback`, every touched file is restored to its `before` state instead.
pub fn recover_workspace(root: &Path, rollback: bool) -> Result<RecoverReport> {
    let layout = init_workspace(root)?;
    let conn = db::connect(&layout)?;
    db::init_schema(&conn)?;
    // flock is released by the kernel when the crashed process died, so a
    // leftover lock file is harmless; recover just takes the lock normally.
    let _lock = WorkspaceLock::acquire(&layout)?;

    let mut report = RecoverReport {
        rolled_back: rollback,
        ..RecoverReport::default()
    };
    let mut protected_drift = false;
    for (path, mut entry) in list_journal_entries(&layout)? {
        if entry.status != JournalStatus::Applying {
            continue;
        }
        let Some(pending) = entry.pending.clone() else {
            continue;
        };
        for pending_file in &pending {
            let rel = PathBuf::from(&pending_file.rel);
            let target = if rollback {
                pending_file.before.as_deref()
            } else {
                pending_file.after.as_deref()
            };
            protected_drift |= set_file_state(root, &layout, &conn, &rel, target)
                .with_context(|| format!("recover {}", pending_file.rel))?;
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
        index_workspace_locked(root, &layout)
            .context("reindex after recovered protected symbol change")?;
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
    fail_after_writes_for_test: Option<usize>,
) -> Result<PathBuf> {
    // Record the full intent (before/after per file) BEFORE touching any real
    // file. If the process dies mid-apply, this durable `applying` entry lets
    // `code-sanity recover` replay or roll back the half-finished apply.
    let pending: Vec<PendingFile> = planned
        .iter()
        .map(|planned_file| PendingFile {
            rel: normalize_rel_path(&planned_file.rel),
            before: planned_file.before.clone(),
            after: planned_file.after().map(ToOwned::to_owned),
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
            protected_drift |=
                set_file_state(root, layout, conn, &planned_file.rel, planned_file.after())
                    .with_context(|| format!("apply {}", planned_file.rel.display()))?;
            applied.push(idx);
            if fail_after_writes_for_test == Some(idx + 1) {
                bail!("simulated apply failure after {} write(s)", idx + 1);
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
                index_workspace_locked(root, layout)
                    .context("reindex after protected symbol change")?;
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
                    )?;
                }
                if protected_drift {
                    index_workspace_locked(root, layout)
                        .context("reindex after rolled-back protected symbol change")?;
                }
                Ok(())
            })();
            rollback.with_context(|| format!("apply failed ({err}); rollback failed"))?;
            entry.status = JournalStatus::RolledBack;
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

/// Drive `rel` to a target state: `Some(content)` writes the real file and
/// reindexes its mirror/map/db; `None` deletes the real file plus its mirror,
/// map, and db row. This is the single primitive shared by apply, rollback,
/// and recover so every path is create/delete/modify aware. The caller must
/// hold the workspace lock. Returns whether the repo-wide protected symbol
/// set changed (the caller then owes a full reindex).
fn set_file_state(
    root: &Path,
    layout: &Layout,
    conn: &rusqlite::Connection,
    rel: &Path,
    target: Option<&str>,
) -> Result<bool> {
    let real_path = root.join(rel);
    match target {
        Some(content) => {
            atomic_write(&real_path, content)
                .with_context(|| format!("write {}", real_path.display()))?;
            let (_, _, protected_changed) = index_single_file_locked(root, layout, rel, true)
                .with_context(|| format!("reindex {}", rel.display()))?;
            Ok(protected_changed)
        }
        None => {
            let rel_string = normalize_rel_path(rel);
            let had_protected = db::all_index_states(conn)?
                .iter()
                .any(|state| state.rel_path == rel_string && !state.protected().is_empty());
            remove_file_if_exists(&real_path)?;
            remove_file_if_exists(&layout.mirror_dir.join(rel))?;
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

fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let nonce = Utc::now().timestamp_nanos_opt().map_or_else(
        || Utc::now().timestamp_micros().to_string(),
        |value| value.to_string(),
    );
    let tmp_path = parent.join(format!(
        ".{file_name}.code-sanity-tmp-{}-{nonce}",
        std::process::id()
    ));
    let write_result = fs::write(&tmp_path, content)
        .and_then(|()| fs::rename(&tmp_path, path))
        .with_context(|| format!("atomic write {}", path.display()));
    if write_result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    write_result
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

fn parse_unified_patch(input: &str) -> Result<UnifiedPatch> {
    let mut lines = input.lines().peekable();
    let mut files = Vec::new();
    let hunk_re = Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@").unwrap();

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
            let mut hunk_lines = Vec::new();
            while let Some(peek) = lines.peek().copied() {
                if peek.starts_with("@@ ") || peek.starts_with("--- ") {
                    break;
                }
                let hunk_line = lines.next().unwrap();
                if hunk_line.starts_with('\\') {
                    continue;
                }
                let Some(prefix) = hunk_line.as_bytes().first().copied() else {
                    bail!("empty hunk line");
                };
                let content = hunk_line[1..].to_string();
                match prefix {
                    b' ' => hunk_lines.push(HunkLine::Context(content)),
                    b'+' => hunk_lines.push(HunkLine::Add(content)),
                    b'-' => hunk_lines.push(HunkLine::Remove(content)),
                    other => bail!("invalid hunk line prefix {}", other as char),
                }
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

fn parse_patch_path(line: &str, prefix: &str) -> Result<String> {
    let path = line
        .strip_prefix(prefix)
        .unwrap_or(line)
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("empty patch path line: {line}"))?;
    Ok(path.to_string())
}

fn normalize_patch_file_path(path: &str, root: &Path, layout: &Layout) -> Result<PathBuf> {
    if path == "/dev/null" {
        bail!("create/delete patches are not supported in MVP");
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
    if let Some(Component::Normal(first)) = components.next()
        && (first == "a" || first == "b")
    {
        candidate = components.as_path().to_path_buf();
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

fn changed_ranges(content: &str, file_patch: &FilePatch) -> Result<Vec<(usize, usize)>> {
    let line_starts = line_starts(content);
    let mut ranges = Vec::new();
    for hunk in &file_patch.hunks {
        let start = byte_for_line(&line_starts, content.len(), hunk.old_start);
        let end = byte_after_lines(&line_starts, content.len(), hunk.old_start, hunk.old_count);
        let old_region = &content[start..end];
        let new_region = hunk_new_region(hunk);
        let (local_start, local_end) = common_changed_range(old_region, &new_region);
        ranges.push((start + local_start, start + local_end));
    }
    Ok(ranges)
}

fn apply_file_patch_to_content(content: &str, file_patch: &FilePatch) -> Result<String> {
    let lines = split_lines(content);
    let mut out = Vec::<String>::new();
    let mut cursor = 0usize;

    for hunk in &file_patch.hunks {
        let start_idx = if hunk.old_start == 0 {
            0
        } else {
            hunk.old_start - 1
        };
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
                    if line_body(actual) != expected {
                        bail!(
                            "context mismatch at line {}: expected {:?}, got {:?}",
                            cursor + 1,
                            expected,
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
                    if line_body(actual) != expected {
                        bail!(
                            "remove mismatch at line {}: expected {:?}, got {:?}",
                            cursor + 1,
                            expected,
                            line_body(actual)
                        );
                    }
                    cursor += 1;
                }
                HunkLine::Add(content) => out.push(format!("{content}\n")),
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

fn reverse_alias_table(span_map: &SpanMap, config: &Config) -> ReverseAliases {
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
fn reverse_map_new_text(
    text: &str,
    reverse: &ReverseAliases,
    terms: &[Term],
    protected: &BTreeSet<String>,
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
        if protected.contains(run) {
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

fn translate_file_patch(
    file_patch: &FilePatch,
    span_map: &SpanMap,
    sanitized_content: &str,
    config: &Config,
    protected: &BTreeSet<String>,
) -> Result<FilePatch> {
    let starts = line_starts(sanitized_content);
    let reverse = reverse_alias_table(span_map, config);
    let terms = term_table(config);
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
            )
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(FilePatch {
        old_path: file_patch.old_path.clone(),
        new_path: file_patch.new_path.clone(),
        hunks,
    })
}

#[allow(clippy::too_many_arguments)]
fn translate_hunk(
    hunk: &Hunk,
    span_map: &SpanMap,
    sanitized_content: &str,
    line_starts: &[usize],
    reverse: &ReverseAliases,
    terms: &[Term],
    protected: &BTreeSet<String>,
) -> Result<Hunk> {
    let old_region_start = byte_for_line(line_starts, sanitized_content.len(), hunk.old_start);
    let old_region_end = byte_after_lines(
        line_starts,
        sanitized_content.len(),
        hunk.old_start,
        hunk.old_count,
    );
    let old_region = &sanitized_content[old_region_start..old_region_end];
    let new_region = hunk_new_region(hunk);
    let old_alias_ranges = alias_ranges_for_region(span_map, old_region_start, old_region_end);
    let new_alias_ranges =
        project_alias_ranges_to_new_region(&old_alias_ranges, old_region, &new_region)?;

    let mut old_cursor = 0usize;
    let mut new_cursor = 0usize;
    let mut lines = Vec::with_capacity(hunk.lines.len());
    for line in &hunk.lines {
        match line {
            HunkLine::Context(text) => {
                let translated = translate_known_alias_ranges(text, old_cursor, &old_alias_ranges)?;
                old_cursor += text.len() + 1;
                new_cursor += text.len() + 1;
                lines.push(HunkLine::Context(translated));
            }
            HunkLine::Remove(text) => {
                let translated = translate_known_alias_ranges(text, old_cursor, &old_alias_ranges)?;
                old_cursor += text.len() + 1;
                lines.push(HunkLine::Remove(translated));
            }
            HunkLine::Add(text) => {
                let translated = translate_known_alias_ranges(text, new_cursor, &new_alias_ranges)?;
                // Newly added text may use aliases the agent saw in the mirror
                // (whole words or inside identifiers); map them back to the
                // real names so the real file stays semantically coherent.
                let translated = reverse_map_new_text(&translated, reverse, terms, protected)?;
                new_cursor += text.len() + 1;
                lines.push(HunkLine::Add(translated));
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

fn alias_ranges_for_region(span_map: &SpanMap, start: usize, end: usize) -> Vec<AliasRange> {
    span_map
        .replacements
        .iter()
        .filter(|replacement| {
            replacement.sanitized_start >= start && replacement.sanitized_end <= end
        })
        .map(|replacement| AliasRange {
            start: replacement.sanitized_start - start,
            end: replacement.sanitized_end - start,
            sanitized_text: replacement.sanitized_text.clone(),
            original_text: replacement.original_text.clone(),
        })
        .collect()
}

fn project_alias_ranges_to_new_region(
    old_ranges: &[AliasRange],
    old_region: &str,
    new_region: &str,
) -> Result<Vec<AliasRange>> {
    let (changed_start, changed_old_end) = common_changed_range(old_region, new_region);
    let mut projected = Vec::with_capacity(old_ranges.len());
    for range in old_ranges {
        let (start, end) = if range.end <= changed_start {
            (range.start, range.end)
        } else if range.start >= changed_old_end {
            (
                new_region.len() - (old_region.len() - range.start),
                new_region.len() - (old_region.len() - range.end),
            )
        } else {
            bail!("patch changes sanitized replacement span");
        };
        if start > end || end > new_region.len() {
            bail!("projected alias range is outside patched hunk");
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

fn translate_known_alias_ranges(
    text: &str,
    line_start_in_region: usize,
    ranges: &[AliasRange],
) -> Result<String> {
    let line_end = line_start_in_region + text.len();
    let mut cursor = 0usize;
    let mut out = String::with_capacity(text.len());
    for range in ranges
        .iter()
        .filter(|range| range.start >= line_start_in_region && range.end <= line_end)
    {
        let local_start = range.start - line_start_in_region;
        let local_end = range.end - line_start_in_region;
        if !text.is_char_boundary(local_start) || !text.is_char_boundary(local_end) {
            bail!("replacement span is not on UTF-8 boundaries");
        }
        let actual = &text[local_start..local_end];
        if actual != range.sanitized_text {
            bail!(
                "replacement span mismatch: expected {:?}, got {:?}",
                range.sanitized_text,
                actual
            );
        }
        out.push_str(&text[cursor..local_start]);
        out.push_str(&range.original_text);
        cursor = local_end;
    }
    out.push_str(&text[cursor..]);
    Ok(out)
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

fn whole_file_patch(rel_path: &Path, old: &str, new: &str) -> String {
    let old_lines = old.lines().count();
    let new_lines = new.lines().count();
    let old_start = if old_lines == 0 { 0 } else { 1 };
    let new_start = if new_lines == 0 { 0 } else { 1 };
    let rel = normalize_rel_path(rel_path);
    let mut out = String::new();
    out.push_str(&format!("--- a/{rel}\n"));
    out.push_str(&format!("+++ b/{rel}\n"));
    out.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        old_start, old_lines, new_start, new_lines
    ));
    for line in old.lines() {
        out.push_str(&format!("-{line}\n"));
    }
    for line in new.lines() {
        out.push_str(&format!("+{line}\n"));
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
fn reverse_sanitized_region(span_map: &SpanMap, mirror: &str, start: usize, end: usize) -> String {
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
        out.push_str(&mirror[cursor..replacement.sanitized_start]);
        out.push_str(&replacement.original_text);
        cursor = replacement.sanitized_end;
    }
    out.push_str(&mirror[cursor..end]);
    out
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

fn hunk_new_region(hunk: &Hunk) -> String {
    let mut out = String::new();
    for line in &hunk.lines {
        match line {
            HunkLine::Context(text) | HunkLine::Add(text) => {
                out.push_str(text);
                out.push('\n');
            }
            HunkLine::Remove(_) => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
