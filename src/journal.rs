use crate::config::Layout;
use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub id: String,
    pub status: JournalStatus,
    pub session_id: Option<String>,
    pub agent: Option<String>,
    pub files: Vec<String>,
    pub sanitized_patch: String,
    pub original_patch: String,
    pub error: Option<String>,
    pub created_at: String,
    /// Per-file before/after snapshots recorded *before* any real file is
    /// touched, so an interrupted apply can be replayed or rolled back by
    /// `code-sanity recover`. `None` for terminal entries that never entered
    /// the applying state (e.g. conflicts detected before planning).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Vec<PendingFile>>,
}

/// One file's transition captured for crash recovery. `before`/`after` are the
/// full file contents (`None` means the file did not / must not exist, i.e.
/// create and delete respectively). `before_mode`/`after_mode` carry the
/// file's permission bits so a rollback or roll-forward that must re-CREATE a
/// file (nothing on disk to preserve from) restores them too; entries written
/// by older binaries deserialize with `None` and fall back to the default
/// create mode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingFile {
    pub rel: String,
    pub before: Option<String>,
    pub after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_mode: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum JournalStatus {
    /// Intent recorded; real files are being written. A durable entry with this
    /// status means an apply was interrupted and `recover` should finish it.
    Applying,
    Success,
    Conflict,
    /// Apply failed after writes started and the real files were restored.
    RolledBack,
}

pub fn new_journal_id() -> String {
    Utc::now().format("%Y-%m-%dT%H-%M-%S%.9fZ").to_string()
}

/// In-flight markers: one empty-ish file per `Applying` entry under
/// `journal/inflight/`. The hot-path interrupted-apply check reads this
/// (almost always empty) directory instead of parsing the entire journal
/// history on every apply/sync/index/embed run.
///
/// Lifecycle (all durable): Applying — entry file first, then marker, so a
/// marker always references a fully written entry; terminal — entry rewritten
/// first, then marker removed, so a crash in between leaves a marker whose
/// entry parses as terminal (self-healed on the next check).
fn inflight_dir(layout: &Layout) -> PathBuf {
    layout.journal_dir.join("inflight")
}

fn inflight_marker(layout: &Layout, id: &str) -> PathBuf {
    inflight_dir(layout).join(id)
}

/// Durably persist a journal entry (atomic write + file/dir fsync) and keep
/// its in-flight marker in step with the status. An `applying` entry is the
/// crash-recovery record, so it must be on disk for real before any real file
/// is touched.
pub fn write_journal(layout: &Layout, entry: &JournalEntry) -> Result<PathBuf> {
    let path = layout.journal_dir.join(format!("{}.patch.json", entry.id));
    let raw = serde_json::to_string_pretty(entry).context("serialize journal entry")?;
    if entry.status == JournalStatus::Applying {
        // Barrier (macOS F_FULLFSYNC): the applying entry and its marker are
        // the recovery record — they must reach the physical medium before
        // any real file changes, or a power loss can leave a torn file with
        // nothing to recover from. Terminal states below are self-healing and
        // keep the plain durable tier.
        crate::fsutil::atomic_write_sync_barrier(&path, &raw)
            .with_context(|| format!("persist journal entry {}", entry.id))?;
        crate::fsutil::atomic_write_sync_barrier(&inflight_marker(layout, &entry.id), "")
            .with_context(|| format!("persist in-flight marker for {}", entry.id))?;
    } else {
        crate::fsutil::atomic_write_sync(&path, &raw)
            .with_context(|| format!("persist journal entry {}", entry.id))?;
        crate::fsutil::remove_file_sync(&inflight_marker(layout, &entry.id))
            .with_context(|| format!("clear in-flight marker for {}", entry.id))?;
    }
    Ok(path)
}

pub fn read_journal(path: &Path) -> Result<JournalEntry> {
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

/// Full journal listing. Corrupt entries are surfaced, never quarantined or
/// silently skipped: an unparseable file might be the sole record of an
/// interrupted apply, and renaming it away would unblock a torn workspace.
pub struct JournalListing {
    /// Parseable entries in id order (id is a sortable UTC timestamp).
    pub entries: Vec<(PathBuf, JournalEntry)>,
    /// Entries that exist but cannot be parsed, with the parse error.
    pub corrupt: Vec<(PathBuf, String)>,
}

pub fn list_journal_entries(layout: &Layout) -> Result<JournalListing> {
    let mut listing = JournalListing {
        entries: Vec::new(),
        corrupt: Vec::new(),
    };
    let read_dir = match fs::read_dir(&layout.journal_dir) {
        Ok(read_dir) => read_dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(listing),
        Err(err) => {
            return Err(err).with_context(|| format!("read {}", layout.journal_dir.display()));
        }
    };
    for entry in read_dir {
        let entry = entry.context("read journal dir entry")?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        match read_journal(&path) {
            Ok(parsed) => listing.entries.push((path, parsed)),
            Err(err) => listing.corrupt.push((path, format!("{err:#}"))),
        }
    }
    listing.entries.sort_by(|a, b| a.1.id.cmp(&b.1.id));
    Ok(listing)
}

/// Entries stuck in `Applying`: an apply was interrupted mid-write and awaits
/// `recover`. Full scan — recover is rare; the hot path uses the marker dir.
pub fn find_interrupted(layout: &Layout) -> Result<Vec<(PathBuf, JournalEntry)>> {
    Ok(list_journal_entries(layout)?
        .entries
        .into_iter()
        .filter(|(_, entry)| entry.status == JournalStatus::Applying)
        .collect())
}

/// Refuse to proceed while an interrupted apply awaits recovery: any command
/// that reads or mutates workspace state would otherwise build on top of
/// half-written real files. The caller must hold the exclusive workspace lock
/// (every current caller does): the check may create the marker dir and
/// self-heal stale markers.
///
/// Cost: one `read_dir` of the (almost always empty) `journal/inflight/` dir.
/// Terminal history is never parsed here.
pub fn ensure_no_interrupted_apply(layout: &Layout) -> Result<()> {
    let markers = match fs::read_dir(inflight_dir(layout)) {
        Ok(markers) => markers,
        // Legacy workspace (journal predates the marker dir): one full scan,
        // then create the dir so every later call is O(empty read_dir). Old
        // binaries ignore it (the `.json` extension filter skips a directory).
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let listing = list_journal_entries(layout)?;
            if let Some((path, reason)) = listing.corrupt.first() {
                bail_corrupt_journal(path, reason)?;
            }
            let interrupted: Vec<_> = listing
                .entries
                .iter()
                .filter(|(_, entry)| entry.status == JournalStatus::Applying)
                .collect();
            if let Some((_, entry)) = interrupted.first() {
                bail_interrupted(&entry.id, interrupted.len())?;
            }
            crate::fsutil::create_dir_all_synced(&inflight_dir(layout))?;
            return Ok(());
        }
        Err(err) => {
            return Err(err).with_context(|| format!("read {}", inflight_dir(layout).display()));
        }
    };
    let mut interrupted: Vec<String> = Vec::new();
    for marker in markers {
        let marker = marker.context("read in-flight marker dir entry")?;
        let id = marker.file_name().to_string_lossy().into_owned();
        if crate::fsutil::is_stale_temp_file(&id) {
            continue;
        }
        let entry_path = layout.journal_dir.join(format!("{id}.patch.json"));
        match read_journal(&entry_path) {
            Ok(entry) if entry.status == JournalStatus::Applying => interrupted.push(entry.id),
            // Crash landed between the terminal entry write and the marker
            // removal: the entry is authoritative, the marker is stale.
            Ok(_) => {
                crate::fsutil::remove_file_sync(&marker.path())?;
            }
            // The marker says an apply may be in flight but its record cannot
            // be read: BLOCK. Quarantining here would silently unblock a
            // workspace that may hold half-applied real files.
            Err(err) => bail_corrupt_journal(&entry_path, &format!("{err:#}"))?,
        }
    }
    if let Some(first) = interrupted.first() {
        bail_interrupted(first, interrupted.len())?;
    }
    Ok(())
}

/// Delete the oldest TERMINAL journal entries beyond `keep` (0 = unlimited).
/// Returns how many were removed. `Applying` entries (the crash-recovery
/// record) and unparseable files (possibly the sole record of an interrupted
/// apply) are never touched; terminal entries have no in-flight marker, so no
/// marker bookkeeping is needed. The caller must hold the exclusive workspace
/// lock — pruning races a concurrent apply's own journal writes otherwise.
pub fn prune_terminal_entries(layout: &Layout, keep: u64) -> Result<usize> {
    if keep == 0 {
        return Ok(0);
    }
    let listing = list_journal_entries(layout)?;
    // `entries` is sorted by id (a sortable UTC timestamp): oldest first.
    let terminal: Vec<&(PathBuf, JournalEntry)> = listing
        .entries
        .iter()
        .filter(|(_, entry)| entry.status != JournalStatus::Applying)
        .collect();
    let excess = terminal.len().saturating_sub(keep as usize);
    let mut removed = 0usize;
    for (path, _) in terminal.into_iter().take(excess) {
        crate::fsutil::remove_file_sync(path)
            .with_context(|| format!("prune journal entry {}", path.display()))?;
        removed += 1;
    }
    Ok(removed)
}

/// Delete the oldest `journal/discarded/<id>/` stash directories beyond
/// `keep` (0 = unlimited). Stash ids are the sortable UTC journal ids, so
/// lexicographic order is age order. Governed by the same
/// `journal.max_entries` knob as entries: stashes are recovery copies of
/// force-reset mirror edits and grow without bound on a busy workspace.
pub fn prune_discarded_stashes(layout: &Layout, keep: u64) -> Result<usize> {
    if keep == 0 {
        return Ok(0);
    }
    let dir = layout.journal_dir.join("discarded");
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err).with_context(|| format!("read {}", dir.display())),
    };
    let mut stashes: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("read entry in {}", dir.display()))?;
        if entry.file_type().is_ok_and(|ty| ty.is_dir()) {
            stashes.push(entry.path());
        }
    }
    stashes.sort();
    let excess = stashes.len().saturating_sub(keep as usize);
    let mut removed = 0usize;
    for stash in stashes.into_iter().take(excess) {
        fs::remove_dir_all(&stash)
            .with_context(|| format!("prune discarded stash {}", stash.display()))?;
        removed += 1;
    }
    Ok(removed)
}

fn bail_interrupted(id: &str, count: usize) -> Result<()> {
    bail!(
        "interrupted apply {id} found ({count} pending); run `code-sanity recover` to replay \
         it or `code-sanity recover --rollback` to undo it"
    )
}

fn bail_corrupt_journal(path: &Path, reason: &str) -> Result<()> {
    bail!(
        "journal entry {} may record an in-flight apply but cannot be parsed ({reason}); \
         the workspace may hold half-applied files. Run `code-sanity verify`; if it \
         passes, move the entry (and its journal/inflight marker, if any) aside \
         manually and re-run",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, status: JournalStatus) -> JournalEntry {
        JournalEntry {
            id: id.to_string(),
            status,
            session_id: None,
            agent: None,
            files: Vec::new(),
            sanitized_patch: String::new(),
            original_patch: String::new(),
            error: None,
            created_at: Utc::now().to_rfc3339(),
            pending: None,
        }
    }

    #[test]
    fn prune_keeps_newest_terminals_and_never_touches_applying_or_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        layout.ensure_dirs().unwrap();
        // Sortable ids: 01..04 terminal (oldest first), 05 in-flight.
        for id in ["01", "02", "03", "04"] {
            write_journal(&layout, &entry(id, JournalStatus::Success)).unwrap();
        }
        write_journal(&layout, &entry("05", JournalStatus::Applying)).unwrap();
        let corrupt = layout.journal_dir.join("00garbage.patch.json");
        fs::write(&corrupt, "{ not json").unwrap();

        assert_eq!(prune_terminal_entries(&layout, 2).unwrap(), 2);
        assert!(!layout.journal_dir.join("01.patch.json").exists());
        assert!(!layout.journal_dir.join("02.patch.json").exists());
        assert!(layout.journal_dir.join("03.patch.json").exists());
        assert!(layout.journal_dir.join("04.patch.json").exists());
        assert!(
            layout.journal_dir.join("05.patch.json").exists(),
            "applying entries are the crash-recovery record; never pruned"
        );
        assert!(corrupt.exists(), "unparseable files are never pruned");

        // keep=0 disables pruning entirely.
        assert_eq!(prune_terminal_entries(&layout, 0).unwrap(), 0);
        // Under the limit: nothing to do.
        assert_eq!(prune_terminal_entries(&layout, 10).unwrap(), 0);
    }

    #[test]
    fn prune_discarded_stashes_keeps_newest_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        layout.ensure_dirs().unwrap();
        // No discarded/ dir yet: a clean no-op, not an error.
        assert_eq!(prune_discarded_stashes(&layout, 2).unwrap(), 0);
        for id in ["01", "02", "03", "04"] {
            let stash = layout.journal_dir.join("discarded").join(id).join("a.rs");
            crate::fsutil::atomic_write_sync(&stash, "stashed edit").unwrap();
        }
        assert_eq!(prune_discarded_stashes(&layout, 0).unwrap(), 0);
        assert_eq!(prune_discarded_stashes(&layout, 2).unwrap(), 2);
        let discarded = layout.journal_dir.join("discarded");
        assert!(!discarded.join("01").exists());
        assert!(!discarded.join("02").exists());
        assert!(discarded.join("03/a.rs").exists());
        assert!(discarded.join("04/a.rs").exists());
    }
}
