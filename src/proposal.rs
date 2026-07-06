//! Model-based sanitizer pipeline: proposals, validation, review queue, and the
//! deterministic alias registry.
//!
//! The model never writes the mirror. `propose-sanitize` runs an offline/local
//! provider that emits schema-validated [`Proposal`]s; surviving proposals go
//! into a review queue under `.code-sanity/review/`. Approving a proposal records
//! a deterministic alias in the config registry, and the deterministic engine
//! (dictionary + registry) applies it at index time. So `index`/`verify` stay
//! deterministic and the model stays out of the write path.

use crate::config::{Config, Layout, ProviderConfig};
use crate::db;
use crate::index::index_single_file;
use crate::map::load_span_map;
use crate::sanitize::public_declaration_identifiers;
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// A single sanitization proposal. This is the model-facing schema: a provider
/// returns these, the engine validates and (on approval) records them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    pub category: String,
    pub original_text: String,
    pub sanitized_text: String,
    #[serde(default)]
    pub confidence: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
}

/// What an external provider returns on stdout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposalBatch {
    #[serde(default)]
    pub file: Option<String>,
    pub proposals: Vec<Proposal>,
}

/// A queued proposal awaiting human review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewItem {
    pub id: String,
    pub file: String,
    pub proposal: Proposal,
    pub status: ReviewStatus,
    /// Why it was queued: "clean" or a flag reason (low confidence, public API…).
    pub flag: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewStatus {
    Pending,
    Approved,
    Rejected,
}

#[derive(Debug, Clone, Default)]
pub struct ProposeReport {
    pub proposed: usize,
    pub queued: usize,
    pub rejected: Vec<String>,
}

/// A provider of sanitization proposals (the model interface).
pub trait ProposalProvider {
    fn propose(&self, rel: &Path, content: &str, config: &Config) -> Result<Vec<Proposal>>;
}

/// Offline/local model provider: `command` is invoked with `{rel, content}` JSON
/// on stdin and must emit a `ProposalBatch` (or a bare proposal array) on stdout.
pub struct ExternalProposalProvider {
    pub command: Vec<String>,
}

impl ProposalProvider for ExternalProposalProvider {
    fn propose(&self, rel: &Path, content: &str, _config: &Config) -> Result<Vec<Proposal>> {
        let (program, args) = self
            .command
            .split_first()
            .ok_or_else(|| anyhow!("external provider command is empty"))?;
        let payload = serde_json::to_string(&serde_json::json!({
            "rel": crate::config::normalize_rel_path(rel),
            "content": content,
        }))?;
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn external provider {program}"))?;
        child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("external provider stdin unavailable"))?
            .write_all(payload.as_bytes())
            .context("write external provider stdin")?;
        let output = child
            .wait_with_output()
            .context("wait for external provider")?;
        if !output.status.success() {
            bail!(
                "external provider failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        parse_proposals(&String::from_utf8_lossy(&output.stdout))
    }
}

/// Deterministic offline provider used when no external model is configured: it
/// proposes neutral aliases for denylisted terms that appear in the file but are
/// not yet covered by the dictionary or registry. Fully local and reproducible.
pub struct HeuristicProposalProvider;

impl ProposalProvider for HeuristicProposalProvider {
    fn propose(&self, _rel: &Path, content: &str, config: &Config) -> Result<Vec<Proposal>> {
        let mut seen = BTreeSet::new();
        let mut proposals = Vec::new();
        for term in &config.sanitizer.denylist {
            let lower = term.to_lowercase();
            if config.sanitizer.dictionary.keys().any(|k| k.eq_ignore_ascii_case(term))
                || config.sanitizer.alias_registry.contains_key(term)
                || !seen.insert(lower.clone())
            {
                continue;
            }
            if !contains_whole_word(content, term) {
                continue;
            }
            proposals.push(Proposal {
                category: "identifier".to_string(),
                original_text: term.clone(),
                sanitized_text: derive_alias(&config.salt, term),
                confidence: 0.6,
                rationale: Some("denylisted term without a mapping".to_string()),
            });
        }
        Ok(proposals)
    }
}

fn parse_proposals(raw: &str) -> Result<Vec<Proposal>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if let Ok(batch) = serde_json::from_str::<ProposalBatch>(trimmed) {
        return Ok(batch.proposals);
    }
    serde_json::from_str::<Vec<Proposal>>(trimmed)
        .context("parse proposals (expected a ProposalBatch or a proposal array)")
}

fn provider_for(config: &Config) -> Box<dyn ProposalProvider> {
    match &config.sanitizer.provider {
        ProviderConfig::External { command } => Box::new(ExternalProposalProvider {
            command: command.clone(),
        }),
        _ => Box::new(HeuristicProposalProvider),
    }
}

/// Run the configured provider over one file (or all tracked files) and enqueue
/// surviving, validated proposals for review. Nothing is applied here.
pub fn propose_sanitize(root: &Path, rel: Option<&Path>) -> Result<ProposeReport> {
    let layout = crate::index::init_workspace(root)?;
    let config = Config::load_or_default(&layout)?;
    let provider = provider_for(&config);

    let files = match rel {
        Some(rel) => vec![crate::config::normalize_rel_path(rel)],
        None => {
            let conn = db::connect(&layout)?;
            db::init_schema(&conn)?;
            db::tracked_files(&conn)?
        }
    };

    let mut report = ProposeReport::default();
    for file in files {
        let real = std::fs::read_to_string(root.join(&file))
            .with_context(|| format!("read {file}"))?;
        let proposals = provider.propose(Path::new(&file), &real, &config)?;
        for proposal in proposals {
            report.proposed += 1;
            match validate_proposal(&proposal, &real, &config) {
                Ok(flag) => {
                    enqueue_review(&layout, &file, &proposal, &flag)?;
                    report.queued += 1;
                }
                Err(reason) => report
                    .rejected
                    .push(format!("{}: {reason}", proposal.original_text)),
            }
        }
    }
    Ok(report)
}

/// Validate one proposal against the policy. `Ok(flag)` means it may be queued
/// (flag is "clean" or a human-review reason); `Err(reason)` means it is rejected
/// outright and never reaches the queue.
pub fn validate_proposal(
    proposal: &Proposal,
    content: &str,
    config: &Config,
) -> std::result::Result<String, String> {
    if proposal.original_text.is_empty() {
        return Err("empty original text".to_string());
    }
    if proposal.sanitized_text == proposal.original_text {
        return Err("alias equals the original".to_string());
    }
    if !content.contains(&proposal.original_text) {
        return Err("original text does not appear in the file".to_string());
    }
    if config
        .sanitizer
        .allowlist
        .iter()
        .any(|item| item.eq_ignore_ascii_case(&proposal.original_text))
    {
        return Err("term is allowlisted; must not be replaced".to_string());
    }
    if proposal.sanitized_text.contains('\n') {
        return Err("alias introduces a newline".to_string());
    }
    if proposal.category == "identifier" && !is_valid_identifier(&proposal.sanitized_text) {
        return Err("alias is not a valid identifier".to_string());
    }
    let alias_lower = proposal.sanitized_text.to_lowercase();
    if config
        .sanitizer
        .denylist
        .iter()
        .any(|term| alias_lower.contains(&term.to_lowercase()))
    {
        return Err("alias still contains a denylisted term".to_string());
    }

    if public_declaration_identifiers(content).contains(&proposal.original_text) {
        return Ok("touches a public API name; needs review".to_string());
    }
    if proposal.confidence < config.sanitizer.confidence_threshold {
        return Ok(format!(
            "confidence {:.2} below threshold {:.2}; needs review",
            proposal.confidence, config.sanitizer.confidence_threshold
        ));
    }
    Ok("clean".to_string())
}

fn enqueue_review(layout: &Layout, file: &str, proposal: &Proposal, flag: &str) -> Result<()> {
    std::fs::create_dir_all(&layout.review_dir)
        .with_context(|| format!("create {}", layout.review_dir.display()))?;
    let id = format!(
        "{}-{}",
        Utc::now().format("%Y-%m-%dT%H-%M-%S%.9fZ"),
        short_hash(&format!("{file}:{}", proposal.original_text))
    );
    let item = ReviewItem {
        id: id.clone(),
        file: file.to_string(),
        proposal: proposal.clone(),
        status: ReviewStatus::Pending,
        flag: flag.to_string(),
        created_at: Utc::now().to_rfc3339(),
    };
    let path = layout.review_dir.join(format!("{id}.json"));
    let raw = serde_json::to_string_pretty(&item).context("serialize review item")?;
    std::fs::write(&path, raw).with_context(|| format!("write {}", path.display()))
}

pub fn list_review(root: &Path, include_resolved: bool) -> Result<Vec<ReviewItem>> {
    let layout = Layout::new(root);
    let mut items = Vec::new();
    let read_dir = match std::fs::read_dir(&layout.review_dir) {
        Ok(read_dir) => read_dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(items),
        Err(err) => return Err(err).with_context(|| format!("read {}", layout.review_dir.display())),
    };
    for entry in read_dir {
        let path = entry.context("read review dir entry")?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let raw = std::fs::read_to_string(&path)?;
        let item: ReviewItem = serde_json::from_str(&raw)
            .with_context(|| format!("parse {}", path.display()))?;
        if include_resolved || item.status == ReviewStatus::Pending {
            items.push(item);
        }
    }
    items.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(items)
}

/// Approve or reject a queued proposal. Approving records the alias in the config
/// registry (deterministic) and reindexes the affected file so the deterministic
/// engine applies it; rejecting just marks the item.
pub fn resolve_review(root: &Path, id: &str, approve: bool) -> Result<ReviewItem> {
    let layout = crate::index::init_workspace(root)?;
    let path = layout.review_dir.join(format!("{id}.json"));
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("review item {id} not found ({})", path.display()))?;
    let mut item: ReviewItem = serde_json::from_str(&raw)
        .with_context(|| format!("parse {}", path.display()))?;
    if item.status != ReviewStatus::Pending {
        bail!("review item {id} is already {:?}", item.status);
    }

    if approve {
        let mut config = Config::load_or_default(&layout)?;
        // Re-validate at approval time so a stale queue can't apply an unsafe alias.
        let real = std::fs::read_to_string(root.join(&item.file))
            .with_context(|| format!("read {}", item.file))?;
        validate_proposal(&item.proposal, &real, &config)
            .map_err(|reason| anyhow!("proposal no longer valid: {reason}"))?;
        config.sanitizer.alias_registry.insert(
            item.proposal.original_text.clone(),
            item.proposal.sanitized_text.clone(),
        );
        config.save(&layout)?;
        index_single_file(root, Path::new(&item.file))
            .with_context(|| format!("reindex {} after approval", item.file))?;
        item.status = ReviewStatus::Approved;
    } else {
        item.status = ReviewStatus::Rejected;
    }
    let updated = serde_json::to_string_pretty(&item).context("serialize review item")?;
    std::fs::write(&path, updated).with_context(|| format!("write {}", path.display()))?;
    Ok(item)
}

/// One applied replacement, for the audit report.
#[derive(Debug, Clone, Serialize)]
pub struct AuditRow {
    pub file: String,
    pub category: String,
    pub original_text: String,
    pub sanitized_text: String,
    pub policy_source: String,
    pub confidence: f64,
    pub original_line: usize,
}

/// Audit report of every applied replacement, read from the span maps.
pub fn audit_replacements(root: &Path, rel: Option<&Path>) -> Result<Vec<AuditRow>> {
    let layout = Layout::new(root);
    let conn = db::connect(&layout)?;
    db::init_schema(&conn)?;
    let files = match rel {
        Some(rel) => vec![crate::config::normalize_rel_path(rel)],
        None => db::tracked_files(&conn)?,
    };
    let mut rows = Vec::new();
    for file in files {
        let map_path = layout.map_path(Path::new(&file));
        let Ok(span_map) = load_span_map(&map_path) else {
            continue;
        };
        for replacement in &span_map.replacements {
            rows.push(AuditRow {
                file: file.clone(),
                category: replacement.category.clone(),
                original_text: replacement.original_text.clone(),
                sanitized_text: replacement.sanitized_text.clone(),
                policy_source: replacement.policy_source.clone(),
                confidence: replacement.confidence,
                original_line: replacement.original_line_start,
            });
        }
    }
    Ok(rows)
}

fn is_valid_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) if first == '_' || first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn contains_whole_word(content: &str, word: &str) -> bool {
    let is_ident = |ch: char| ch == '_' || ch.is_ascii_alphanumeric();
    let mut from = 0usize;
    while let Some(rel) = content[from..].find(word) {
        let start = from + rel;
        let end = start + word.len();
        let before = content[..start].chars().next_back().is_none_or(|ch| !is_ident(ch));
        let after = content[end..].chars().next().is_none_or(|ch| !is_ident(ch));
        if before && after {
            return true;
        }
        from = end;
    }
    false
}

fn derive_alias(salt: &str, original: &str) -> String {
    format!("sym_{}", short_hash(&format!("{salt}:{original}")))
}

fn short_hash(input: &str) -> String {
    crate::map::sha256_hex(input.as_bytes())[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_denylist(terms: &[&str]) -> Config {
        let mut config = Config::default();
        config.sanitizer.denylist = terms.iter().map(|term| term.to_string()).collect();
        config
    }

    #[test]
    fn rejects_invalid_identifier_alias() {
        let config = Config::default();
        let proposal = Proposal {
            category: "identifier".to_string(),
            original_text: "helper".to_string(),
            sanitized_text: "1bad-name".to_string(),
            confidence: 1.0,
            rationale: None,
        };
        let verdict = validate_proposal(&proposal, "fn helper() {}", &config);
        assert!(verdict.is_err());
    }

    #[test]
    fn rejects_alias_containing_denylisted_term() {
        let config = config_with_denylist(&["secret"]);
        let proposal = Proposal {
            category: "comment".to_string(),
            original_text: "widget".to_string(),
            sanitized_text: "secret_widget".to_string(),
            confidence: 1.0,
            rationale: None,
        };
        assert!(validate_proposal(&proposal, "// widget here", &config).is_err());
    }

    #[test]
    fn low_confidence_is_queued_with_flag_not_rejected() {
        let config = Config::default();
        let proposal = Proposal {
            category: "identifier".to_string(),
            original_text: "helper".to_string(),
            sanitized_text: "assistant".to_string(),
            confidence: 0.3,
            rationale: None,
        };
        let flag = validate_proposal(&proposal, "fn helper() {}", &config).unwrap();
        assert!(flag.contains("confidence"));
    }
}
