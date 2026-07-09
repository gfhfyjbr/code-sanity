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
use crate::index::reconverge_workspace;
use crate::lock::WorkspaceLock;
use crate::map::load_span_map;
use crate::sanitize::{collect_protected_identifiers, derive_alias};
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
/// stdin is written from a dedicated thread while stdout/stderr are drained
/// concurrently (no pipe deadlock on large files), and the child is killed if
/// it exceeds the timeout.
pub struct ExternalProposalProvider {
    pub command: Vec<String>,
    pub timeout: std::time::Duration,
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

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("external provider stdin unavailable"))?;
        let payload_bytes = payload.into_bytes();
        let writer = std::thread::spawn(move || -> std::io::Result<()> {
            stdin.write_all(&payload_bytes)
            // stdin drops (closes) here so the child sees EOF.
        });
        let mut stdout_pipe = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("external provider stdout unavailable"))?;
        let stdout_reader = std::thread::spawn(move || -> std::io::Result<String> {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut stdout_pipe, &mut buf)?;
            Ok(buf)
        });
        let mut stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("external provider stderr unavailable"))?;
        let stderr_reader = std::thread::spawn(move || -> std::io::Result<String> {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut stderr_pipe, &mut buf)?;
            Ok(buf)
        });

        let deadline = std::time::Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait().context("poll external provider")? {
                Some(status) => break status,
                None if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!(
                        "external provider timed out after {:?}: {program}",
                        self.timeout
                    );
                }
                None => std::thread::sleep(std::time::Duration::from_millis(25)),
            }
        };

        // The writer may fail with EPIPE if the child exited without reading
        // all input; only surface that when the child itself failed.
        let write_result = writer
            .join()
            .map_err(|_| anyhow!("external provider stdin writer panicked"))?;
        let stdout = stdout_reader
            .join()
            .map_err(|_| anyhow!("external provider stdout reader panicked"))?
            .context("read external provider stdout")?;
        let stderr = stderr_reader
            .join()
            .map_err(|_| anyhow!("external provider stderr reader panicked"))?
            .context("read external provider stderr")?;
        if !status.success() {
            bail!("external provider failed: {}", stderr.trim());
        }
        if let Err(err) = write_result
            && err.kind() != std::io::ErrorKind::BrokenPipe
        {
            return Err(err).context("write external provider stdin");
        }
        parse_proposals(&stdout)
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
            if config
                .sanitizer
                .dictionary
                .keys()
                .any(|k| k.eq_ignore_ascii_case(term))
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

/// OpenAI-compatible chat provider (e.g. a local kou-router gateway). The model
/// receives the real file plus the current policy (deny/allow lists, terms that
/// already have mappings) and must answer with a strict-JSON [`ProposalBatch`].
/// Its output goes through the same validation and review queue as any other
/// provider — it never touches the mirror.
pub struct LlmProposalProvider {
    pub client: crate::llm::OpenAiClient,
    pub model: String,
}

const LLM_SYSTEM_PROMPT: &str = "You review source code for terms that could \
trigger false-positive safety refusals in coding agents (e.g. words that sound \
like malware, attacks, or exfiltration but are benign here) or that leak \
private naming (internal company, product, or customer names). Propose neutral \
replacement aliases.\n\
Rules:\n\
- only propose terms that literally appear in the file;\n\
- never propose terms from `allowlist`, and never re-propose keys from `already_mapped`;\n\
- an alias for category \"identifier\" must be a valid ASCII identifier;\n\
- aliases must not contain newlines and must not contain any `denylist` term;\n\
- do not propose renames that change behavior-bearing text (imports, protocol \
strings, SQL, shell commands, public API names).\n\
Respond with strict JSON only, no prose and no markdown fences:\n\
{\"proposals\":[{\"category\":\"identifier|comment|string\",\
\"original_text\":\"...\",\"sanitized_text\":\"...\",\"confidence\":0.0,\
\"rationale\":\"...\"}]}\n\
If nothing needs sanitizing, respond {\"proposals\":[]}.";

impl ProposalProvider for LlmProposalProvider {
    fn propose(&self, rel: &Path, content: &str, config: &Config) -> Result<Vec<Proposal>> {
        let already_mapped: Vec<&String> = config
            .sanitizer
            .dictionary
            .keys()
            .chain(config.sanitizer.alias_registry.keys())
            .collect();
        let user = serde_json::to_string(&serde_json::json!({
            "rel": crate::config::normalize_rel_path(rel),
            "content": content,
            "denylist": config.sanitizer.denylist,
            "allowlist": config.sanitizer.allowlist,
            "already_mapped": already_mapped,
        }))?;
        let reply = self.client.chat(&self.model, LLM_SYSTEM_PROMPT, &user)?;
        parse_proposals(strip_code_fences(&reply))
    }
}

/// Models often wrap JSON in ```json fences despite instructions — sometimes
/// with prose around the fence ("Here is the JSON: ... Hope this helps").
/// Extract the first fenced block wherever it sits; without a closed fence,
/// fall back to the trimmed reply.
fn strip_code_fences(reply: &str) -> &str {
    let trimmed = reply.trim();
    let Some(open) = trimmed.find("```") else {
        return trimmed;
    };
    let after_open = &trimmed[open + 3..];
    // Drop an optional language tag on the opening fence line, unless the
    // payload starts right on it.
    let body_start = match after_open.find('\n') {
        Some(newline) if !after_open[..newline].trim_start().starts_with(['{', '[']) => newline + 1,
        _ => 0,
    };
    let body = &after_open[body_start..];
    let Some(close) = body.find("```") else {
        return trimmed;
    };
    body[..close].trim()
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

/// Explicit human confirmations for providers that leave the process: External
/// executes a repo-supplied command, Llm posts real file content to a
/// repo-configured endpoint. Both default to refused.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProviderAllow {
    pub command: bool,
    pub endpoint: bool,
}

fn provider_for(config: &Config, allow: ProviderAllow) -> Result<Box<dyn ProposalProvider>> {
    // All OpenAI-compatible chat kinds (llm / openrouter / kou-router) share
    // one path and one confirmation gate: the endpoint is repo-configured
    // (loopback is only a preset default) and receives real file content.
    if let Some(endpoint) = config.sanitizer.provider.llm_endpoint() {
        if !allow.endpoint {
            bail!(
                "the configured provider sends real file content to {}; \
                 re-run with --allow-provider-endpoint after reviewing it",
                endpoint.base_url
            );
        }
        return Ok(Box::new(LlmProposalProvider {
            client: crate::llm::OpenAiClient::new(
                &endpoint.base_url,
                &endpoint.api_key_env,
                endpoint.timeout_secs,
            )?,
            model: endpoint.model,
        }));
    }
    match &config.sanitizer.provider {
        ProviderConfig::External {
            command,
            timeout_secs,
        } => {
            if !allow.command {
                bail!(
                    "the configured provider runs a repo-supplied command ({:?}); \
                     re-run with --allow-provider-command after reviewing it",
                    command.join(" ")
                );
            }
            Ok(Box::new(ExternalProposalProvider {
                command: command.clone(),
                timeout: std::time::Duration::from_secs(timeout_secs.unwrap_or(60)),
            }))
        }
        _ => Ok(Box::new(HeuristicProposalProvider)),
    }
}

/// Run the configured provider over one file (or all tracked files) and enqueue
/// surviving, validated proposals for review. Nothing is applied here.
/// `allow` carries the explicit human confirmations required to execute a
/// repo-supplied provider command or post to a repo-configured endpoint.
pub fn propose_sanitize(
    root: &Path,
    rel: Option<&Path>,
    allow: ProviderAllow,
) -> Result<ProposeReport> {
    // Plain init (the wrapper drops the exclusive lock): provider calls may
    // block on HTTP or a child process and must not starve workspace writers.
    let layout = crate::index::init_workspace(root)?;
    let config = Config::load_or_default(&layout)?;
    let provider = provider_for(&config, allow)?;

    let files = match rel {
        // The path is repo-config-adjacent input and the file's REAL content
        // goes to a provider: never allow it to point outside the repo.
        Some(rel) => vec![crate::config::normalize_rel_path(
            &crate::config::normalize_safe_rel_path(rel, "repo")?,
        )],
        None => {
            // Short shared lock just for the tracked-file snapshot.
            let _lock = WorkspaceLock::acquire_shared(&layout)?;
            let conn = db::connect(&layout)?;
            db::check_schema(&conn)?;
            db::tracked_files(&conn)?
        }
    };

    let mut report = ProposeReport::default();
    for file in files {
        let real =
            std::fs::read_to_string(root.join(&file)).with_context(|| format!("read {file}"))?;
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
    use crate::sanitize::{matchability_error, normalize_term, term_table, word_runs};

    if proposal.original_text.is_empty() {
        return Err("empty original text".to_string());
    }
    // The sanitizer can only match word-run terms; a proposal outside that
    // class would be recorded and silently never fire.
    if let Some(reason) = matchability_error(&proposal.original_text) {
        return Err(reason);
    }
    if let Some(reason) = matchability_error(&proposal.sanitized_text) {
        return Err(format!("alias {reason}"));
    }
    // Normalized equality: `Acme` -> `ACME` (or `a_cme`) is still the same
    // term to the matcher, so it sanitizes nothing.
    if normalize_term(&proposal.sanitized_text) == normalize_term(&proposal.original_text) {
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
    // The alias must be clean against the WHOLE term set (registry +
    // dictionary + denylist), not just the denylist: containing any term
    // makes the sanitizer's own output sanitizable, and reusing another
    // term's alias makes the mirror non-injective.
    let alias_normalized = normalize_term(&proposal.sanitized_text);
    for term in term_table(config) {
        if term.raw == proposal.original_text {
            continue;
        }
        if alias_normalized.contains(term.normalized.as_str()) {
            return Err(format!(
                "alias still contains sanitizable term {:?}",
                term.raw
            ));
        }
        if normalize_term(&term.replacement) == alias_normalized {
            return Err(format!("alias is already the alias of {:?}", term.raw));
        }
    }
    // Alias-collision guard for this file: a natural word spelled like the
    // alias would make the rendered mirror ambiguous.
    for (start, end) in word_runs(content) {
        if normalize_term(&content[start..end]) == alias_normalized {
            return Err(format!(
                "alias already occurs in the file as {:?} (byte {start}); pick a different alias",
                &content[start..end]
            ));
        }
    }

    if collect_protected_identifiers(content).contains(&proposal.original_text) {
        return Ok("touches a protected name (public API or import); needs review".to_string());
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
    // Atomic: a crash mid-write must not leave a truncated item that breaks
    // `list_review` for the whole queue.
    crate::fsutil::atomic_write(&path, &raw).with_context(|| format!("write {}", path.display()))
}

pub fn list_review(root: &Path, include_resolved: bool) -> Result<Vec<ReviewItem>> {
    let layout = Layout::new(root);
    let mut items = Vec::new();
    let read_dir = match std::fs::read_dir(&layout.review_dir) {
        Ok(read_dir) => read_dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(items),
        Err(err) => {
            return Err(err).with_context(|| format!("read {}", layout.review_dir.display()));
        }
    };
    for entry in read_dir {
        let path = entry.context("read review dir entry")?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let raw = std::fs::read_to_string(&path)?;
        let item: ReviewItem =
            serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
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
    // Approval is a read-modify-write of the config registry plus a reindex;
    // hold the exclusive lock for the whole sequence so concurrent approvals
    // cannot lose registry entries.
    let (layout, _lock) = crate::index::init_workspace_locked(root)?;
    crate::journal::ensure_no_interrupted_apply(&layout)?;
    let path = layout.review_dir.join(format!("{id}.json"));
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("review item {id} not found ({})", path.display()))?;
    let mut item: ReviewItem =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
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
        // Repo-wide alias-collision scan BEFORE the registry is persisted:
        // config.save + reconverge must not be able to fail after the alias
        // landed (that would strand a registry entry no file can render).
        // Under the already-held exclusive lock.
        let candidate_terms = [crate::sanitize::Term {
            raw: item.proposal.original_text.clone(),
            normalized: crate::sanitize::normalize_term(&item.proposal.original_text),
            replacement: item.proposal.sanitized_text.clone(),
            policy_source: "alias-registry",
        }];
        let conn = db::connect(&layout)?;
        db::check_schema(&conn)?;
        for rel in db::tracked_files(&conn)? {
            let Ok(content) = std::fs::read_to_string(root.join(&rel)) else {
                continue;
            };
            if let Some(collision) =
                crate::sanitize::alias_collisions(&content, &candidate_terms).first()
            {
                bail!(
                    "proposal alias {:?} occurs in {rel} at byte {} as {:?}; approval \
                     refused — pick a different alias",
                    item.proposal.sanitized_text,
                    collision.offset,
                    collision.word
                );
            }
        }
        config.sanitizer.alias_registry.insert(
            item.proposal.original_text.clone(),
            item.proposal.sanitized_text.clone(),
        );
        config.save(&layout)?;
        // A registry change alters the rendering policy for the whole repo,
        // not just the proposal's file: reconverge everything before agents
        // (MCP readers) can observe a half-registered term.
        reconverge_workspace(root, &layout)
            .with_context(|| format!("reindex after approving {}", item.id))?;
        item.status = ReviewStatus::Approved;
    } else {
        item.status = ReviewStatus::Rejected;
    }
    let updated = serde_json::to_string_pretty(&item).context("serialize review item")?;
    crate::fsutil::atomic_write(&path, &updated)
        .with_context(|| format!("write {}", path.display()))?;
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
    layout.require_initialized()?;
    let _lock = WorkspaceLock::acquire_shared(&layout)?;
    let conn = db::connect(&layout)?;
    db::check_schema(&conn)?;
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
        let before = content[..start]
            .chars()
            .next_back()
            .is_none_or(|ch| !is_ident(ch));
        let after = content[end..].chars().next().is_none_or(|ch| !is_ident(ch));
        if before && after {
            return true;
        }
        from = end;
    }
    false
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
    fn strip_code_fences_unwraps_fenced_json() {
        let fenced = "```json\n{\"proposals\":[]}\n```";
        assert_eq!(strip_code_fences(fenced), "{\"proposals\":[]}");
        let bare = "{\"proposals\":[]}";
        assert_eq!(strip_code_fences(bare), bare);
        let no_tag = "```\n{\"proposals\":[]}\n```";
        assert_eq!(strip_code_fences(no_tag), "{\"proposals\":[]}");
    }

    #[test]
    fn strip_code_fences_extracts_block_from_surrounding_prose() {
        let prose =
            "Here is the JSON you asked for:\n```json\n{\"proposals\":[]}\n```\nHope this helps!";
        assert_eq!(strip_code_fences(prose), "{\"proposals\":[]}");
        let inline = "Sure! ```{\"proposals\":[]}``` — done.";
        assert_eq!(strip_code_fences(inline), "{\"proposals\":[]}");
        let array = "```json\n[{\"category\":\"identifier\"}]\n```";
        assert_eq!(strip_code_fences(array), "[{\"category\":\"identifier\"}]");
        // An unterminated fence falls back to the trimmed reply.
        let unterminated = "```json\n{\"proposals\":[]}";
        assert_eq!(strip_code_fences(unterminated), unterminated);
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
