use crate::config::Layout;
use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum JournalStatus {
    Success,
    Conflict,
}

pub fn new_journal_id() -> String {
    Utc::now().format("%Y-%m-%dT%H-%M-%S%.9fZ").to_string()
}

pub fn write_journal(layout: &Layout, entry: &JournalEntry) -> Result<PathBuf> {
    fs::create_dir_all(&layout.journal_dir)
        .with_context(|| format!("create {}", layout.journal_dir.display()))?;
    let path = layout.journal_dir.join(format!("{}.patch.json", entry.id));
    let raw = serde_json::to_string_pretty(entry).context("serialize journal entry")?;
    fs::write(&path, raw).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}
