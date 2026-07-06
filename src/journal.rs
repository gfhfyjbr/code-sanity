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

/// Durably persist a journal entry (atomic write + file/dir fsync). An
/// `applying` entry is the crash-recovery record, so it must be on disk for
/// real before any real file is touched.
pub fn write_journal(layout: &Layout, entry: &JournalEntry) -> Result<PathBuf> {
    let path = layout.journal_dir.join(format!("{}.patch.json", entry.id));
    let raw = serde_json::to_string_pretty(entry).context("serialize journal entry")?;
    crate::fsutil::atomic_write_sync(&path, &raw)
        .with_context(|| format!("persist journal entry {}", entry.id))?;
    Ok(path)
}

pub fn read_journal(path: &Path) -> Result<JournalEntry> {
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

/// All journal entries in id order (id is a sortable UTC timestamp).
///
/// A corrupt entry is quarantined (renamed to `<name>.corrupt`, logged) and
/// skipped instead of blocking the listing — `recover` runs exactly when the
/// last session ended badly, so one damaged file must not wedge it.
pub fn list_journal_entries(layout: &Layout) -> Result<Vec<(PathBuf, JournalEntry)>> {
    let mut out = Vec::new();
    let read_dir = match fs::read_dir(&layout.journal_dir) {
        Ok(read_dir) => read_dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(out),
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
            Ok(parsed) => out.push((path, parsed)),
            Err(err) => {
                let quarantined = path.with_extension("json.corrupt");
                log::warn!(
                    "journal entry {} is corrupt ({err:#}); quarantining as {}",
                    path.display(),
                    quarantined.display()
                );
                let _ = fs::rename(&path, &quarantined);
            }
        }
    }
    out.sort_by(|a, b| a.1.id.cmp(&b.1.id));
    Ok(out)
}

/// Entries stuck in `Applying`: an apply was interrupted mid-write and awaits
/// `recover`.
pub fn find_interrupted(layout: &Layout) -> Result<Vec<(PathBuf, JournalEntry)>> {
    Ok(list_journal_entries(layout)?
        .into_iter()
        .filter(|(_, entry)| entry.status == JournalStatus::Applying)
        .collect())
}

/// Refuse to proceed while an interrupted apply awaits recovery: any command
/// that reads or mutates workspace state would otherwise build on top of
/// half-written real files.
pub fn ensure_no_interrupted_apply(layout: &Layout) -> Result<()> {
    let interrupted = find_interrupted(layout)?;
    if let Some((_, entry)) = interrupted.first() {
        bail!(
            "interrupted apply {} found ({} pending); run `code-sanity recover` to replay \
             it or `code-sanity recover --rollback` to undo it",
            entry.id,
            interrupted.len()
        );
    }
    Ok(())
}
