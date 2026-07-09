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
/// create and delete respectively).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingFile {
    pub rel: String,
    pub before: Option<String>,
    pub after: Option<String>,
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
    crate::fsutil::atomic_write_sync(&path, &raw)
        .with_context(|| format!("persist journal entry {}", entry.id))?;
    let marker = inflight_marker(layout, &entry.id);
    if entry.status == JournalStatus::Applying {
        crate::fsutil::atomic_write_sync(&marker, "")
            .with_context(|| format!("persist in-flight marker for {}", entry.id))?;
    } else {
        crate::fsutil::remove_file_sync(&marker)
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
