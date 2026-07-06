use crate::config::Layout;
use anyhow::{Context, Result};
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

/// Durably persist a journal entry: temp file + fsync + rename + directory
/// fsync. An `applying` entry is the crash-recovery record, so it must be on
/// disk for real before any real file is touched.
pub fn write_journal(layout: &Layout, entry: &JournalEntry) -> Result<PathBuf> {
    use std::io::Write as _;
    fs::create_dir_all(&layout.journal_dir)
        .with_context(|| format!("create {}", layout.journal_dir.display()))?;
    let path = layout.journal_dir.join(format!("{}.patch.json", entry.id));
    let raw = serde_json::to_string_pretty(entry).context("serialize journal entry")?;

    let tmp = layout
        .journal_dir
        .join(format!(".{}.patch.json.tmp", entry.id));
    let result = (|| -> Result<()> {
        let mut file =
            fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        file.write_all(raw.as_bytes())
            .with_context(|| format!("write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("fsync {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("rename into {}", path.display()))?;
        fs::File::open(&layout.journal_dir)
            .and_then(|dir| dir.sync_all())
            .with_context(|| format!("fsync {}", layout.journal_dir.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result?;
    Ok(path)
}

pub fn read_journal(path: &Path) -> Result<JournalEntry> {
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

/// All journal entries in id order (id is a sortable UTC timestamp).
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
        let parsed = read_journal(&path)?;
        out.push((path, parsed));
    }
    out.sort_by(|a, b| a.1.id.cmp(&b.1.id));
    Ok(out)
}
