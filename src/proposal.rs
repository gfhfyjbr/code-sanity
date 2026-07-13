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
use crate::sanitize::{collect_protected_identifiers, derive_alias, normalize_term};
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, mpsc};
use std::time::Instant;

/// A single sanitization proposal. This is the model-facing schema: a provider
/// returns these, the engine validates and (on approval) records them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    /// Stable semantic target. LLM v2 proposals must provide it; legacy local
    /// providers are resolved to one exact owned symbol before review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ProposalTarget>,
    pub category: String,
    pub original_text: String,
    pub sanitized_text: String,
    /// The provider's self-reported confidence in [0, 1]. Untrusted by
    /// design: it only decides whether the review item is flagged for extra
    /// scrutiny (below `sanitizer.confidence_threshold`), never whether a
    /// proposal is applied — every proposal goes through human review.
    #[serde(default)]
    pub confidence: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ProposalTarget {
    Semantic(SemanticProposalTarget),
    FilePath(FilePathProposalTarget),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticProposalTarget {
    pub symbol_id: String,
    pub occurrence_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilePathProposalTarget {
    pub path_id: String,
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
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalProgress {
    pub detail: String,
    pub completed: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReviewDecision {
    status: ReviewStatus,
    original_text: String,
    sanitized_text: String,
    updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ReviewDecisionLedger {
    decisions: BTreeMap<String, ReviewDecision>,
}

fn decision_ledger_path(layout: &Layout) -> std::path::PathBuf {
    layout.state_dir.join("review-decisions.json")
}

fn load_decision_ledger(layout: &Layout) -> Result<ReviewDecisionLedger> {
    let path = decision_ledger_path(layout);
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw)
            .with_context(|| format!("parse review decision ledger {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(ReviewDecisionLedger::default())
        }
        Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
    }
}

fn record_review_decision(layout: &Layout, item: &ReviewItem) -> Result<()> {
    record_review_decisions(layout, std::slice::from_ref(item))
}

fn record_review_decisions(layout: &Layout, items: &[ReviewItem]) -> Result<()> {
    if items
        .iter()
        .all(|item| item.status == ReviewStatus::Pending || item.status == ReviewStatus::Stale)
    {
        return Ok(());
    }
    let mut ledger = load_decision_ledger(layout)?;
    let updated_at = Utc::now().to_rfc3339();
    for item in items {
        if item.status == ReviewStatus::Pending || item.status == ReviewStatus::Stale {
            continue;
        }
        ledger.decisions.insert(
            proposal_identity(&item.proposal),
            ReviewDecision {
                status: item.status.clone(),
                original_text: item.proposal.original_text.clone(),
                sanitized_text: item.proposal.sanitized_text.clone(),
                updated_at: updated_at.clone(),
            },
        );
    }
    let raw = serde_json::to_string_pretty(&ledger).context("serialize review decision ledger")?;
    crate::fsutil::atomic_write(&decision_ledger_path(layout), &raw)
        .context("write review decision ledger")
}

pub(crate) fn forget_quarantined_alias_decisions(
    layout: &Layout,
    aliases: &[crate::semantic_store::QuarantinedSemanticAlias],
) -> Result<()> {
    if aliases.is_empty() {
        return Ok(());
    }
    let path = decision_ledger_path(layout);
    if !path.exists() {
        return Ok(());
    }
    let symbol_ids = aliases
        .iter()
        .map(|alias| alias.symbol_id.as_str())
        .collect::<BTreeSet<_>>();
    let mappings = aliases
        .iter()
        .map(|alias| {
            (
                normalize_term(&alias.original),
                normalize_term(&alias.alias),
            )
        })
        .collect::<BTreeSet<_>>();
    let mut ledger = load_decision_ledger(layout)?;
    let before = ledger.decisions.len();
    ledger.decisions.retain(|identity, decision| {
        !symbol_ids.contains(identity.as_str())
            && !mappings.contains(&(
                normalize_term(&decision.original_text),
                normalize_term(&decision.sanitized_text),
            ))
    });
    if ledger.decisions.len() == before {
        return Ok(());
    }
    let raw = serde_json::to_string_pretty(&ledger)
        .context("serialize repaired review decision ledger")?;
    crate::fsutil::atomic_write(&path, &raw).context("repair review decision ledger")
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ProposeReport {
    pub proposed: usize,
    pub queued: usize,
    /// Valid proposals already represented by a pending review item.
    pub duplicates: usize,
    pub rejected: Vec<String>,
    /// Per-file or path-batch failures: the run continues while at least one
    /// provider request succeeds; an all-requests-failed run is a hard error.
    pub errors: Vec<String>,
    /// Source bodies larger than `sanitizer.propose_max_file_bytes`. Their
    /// contents and semantic candidates are not sent, while their path
    /// components remain in the separate deduplicated path-only inventory.
    pub skipped: Vec<String>,
    /// Why indexed targets did or did not reach the provider. Counts are
    /// intentionally visible so recall regressions cannot masquerade as a
    /// model returning an empty answer.
    pub eligibility: ProposalEligibility,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ProposalEligibility {
    pub owned_symbols: usize,
    pub eligible_symbols: usize,
    pub sent_symbol_candidates: usize,
    pub compiler_resolvable_symbols: usize,
    pub excluded_unresolved: usize,
    pub excluded_api_boundary: usize,
    pub excluded_existing_alias: usize,
    pub pending_symbol_decisions: usize,
    pub unique_path_candidates: usize,
}

/// Live, non-sensitive progress from a proposal run. Events name repo-relative
/// files and counts, but never include file content, model replies, or keys.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum ProposeProgress {
    Started {
        total: usize,
        jobs: usize,
        requests: usize,
    },
    FileStarted {
        position: usize,
        total: usize,
        file: String,
        chunks: usize,
    },
    ChunkStarted {
        file: String,
        chunk: usize,
        chunks: usize,
    },
    ChunkFinished {
        completed: usize,
        total: usize,
        file: String,
        chunk: usize,
        chunks: usize,
        elapsed_ms: u64,
        outcome: ProposeChunkOutcome,
    },
    FileFinished {
        completed: usize,
        total: usize,
        file: String,
        elapsed_ms: u64,
        outcome: ProposeFileOutcome,
        proposed: usize,
        queued: usize,
        duplicates: usize,
        rejected: usize,
    },
    Finished {
        total: usize,
        requests: usize,
        elapsed_ms: u64,
        proposed: usize,
        queued: usize,
        duplicates: usize,
        rejected: usize,
        skipped: usize,
        errors: usize,
    },
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProposeFileOutcome {
    Completed,
    Skipped,
    Error,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProposeChunkOutcome {
    Completed,
    Error,
}

/// Location of the current request inside the complete source file. Indexes
/// and line numbers are one-based and inclusive.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct ProposalChunkMeta {
    pub index: usize,
    pub total: usize,
    pub start_line: usize,
    pub end_line: usize,
    /// First line owned by this chunk. Earlier lines are overlap context only.
    pub core_start_line: usize,
    /// Last line owned by this chunk (inclusive).
    pub core_end_line: usize,
}

impl ProposalChunkMeta {
    fn single(content: &str) -> Self {
        Self {
            index: 1,
            total: 1,
            start_line: 1,
            end_line: content.lines().count().max(1),
            core_start_line: 1,
            core_end_line: content.lines().count().max(1),
        }
    }
}

/// File-local findings that a later chunk should not propose again.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct ProposalRequestContext {
    pub already_proposed_originals: Vec<String>,
    pub already_decided_symbol_ids: Vec<String>,
    pub already_decided_path_terms: Vec<String>,
    /// External framework, SDK, package, and `extern` identifiers derived by
    /// the repository index and relevant to this source chunk.
    pub indexed_external_identifiers: Vec<String>,
    /// Existing owned symbols are the only legal identifier proposal targets.
    pub semantic_candidates: Vec<SemanticCandidate>,
    /// Directory components and filename stems that remain visible after all
    /// currently-known path aliases have been applied. A file-path proposal
    /// must copy one exact `path_id` from this list.
    pub path_candidates: Vec<FilePathCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SemanticCandidate {
    pub symbol_id: String,
    pub occurrence_id: String,
    pub name: String,
    pub kind: String,
    pub qualified_name: String,
    pub declaration_line: usize,
    pub reference_count: usize,
    pub references_complete: bool,
    /// A compiler/LSP provider can close this non-local symbol during the
    /// approval preflight even though syntax alone cannot prove completeness.
    #[serde(default)]
    pub compiler_resolvable: bool,
    pub occurrence_lines: Vec<usize>,
    pub call_lines: Vec<usize>,
    pub signature: String,
    pub enclosing_code: String,
    pub api_boundary: bool,
    #[serde(default)]
    pub lexically_closed: bool,
    pub origin: String,
    pub existing_alias: Option<String>,
}

fn semantic_candidate_is_resolvable(candidate: &SemanticCandidate) -> bool {
    candidate.references_complete || candidate.compiler_resolvable
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FilePathCandidate {
    pub path_id: String,
    pub path: String,
    pub component_index: usize,
    pub kind: String,
    pub value: String,
}

/// A provider of sanitization proposals (the model interface).
pub trait ProposalProvider: Sync {
    fn propose(&self, rel: &Path, content: &str, config: &Config) -> Result<Vec<Proposal>>;

    fn propose_chunk(
        &self,
        rel: &Path,
        content: &str,
        config: &Config,
        _chunk: ProposalChunkMeta,
    ) -> Result<Vec<Proposal>> {
        self.propose(rel, content, config)
    }

    fn propose_chunk_with_context(
        &self,
        rel: &Path,
        content: &str,
        config: &Config,
        chunk: ProposalChunkMeta,
        _context: &ProposalRequestContext,
    ) -> Result<Vec<Proposal>> {
        self.propose_chunk(rel, content, config, chunk)
    }
}

/// Offline/local model provider: `command` is invoked with `{rel, content}` JSON
/// on stdin and must emit a `ProposalBatch` (or a bare proposal array) on stdout.
/// stdin is written from a dedicated thread while stdout/stderr are drained
/// concurrently (no pipe deadlock on large files), and the child is killed if
/// it exceeds the timeout.
pub struct ExternalProposalProvider {
    pub command: Vec<String>,
    pub timeout: std::time::Duration,
    pub semantic_aliases: Vec<(String, String)>,
}

impl ExternalProposalProvider {
    fn run(
        &self,
        rel: &Path,
        content: &str,
        request: Option<(ProposalChunkMeta, &ProposalRequestContext)>,
    ) -> Result<Vec<Proposal>> {
        let (program, args) = self
            .command
            .split_first()
            .ok_or_else(|| anyhow!("external provider command is empty"))?;
        let payload = match request {
            Some((chunk, context)) => serde_json::json!({
                "request_mode": if content.is_empty()
                    && context.semantic_candidates.is_empty()
                    && !context.path_candidates.is_empty()
                {
                    "path-only"
                } else {
                    "source"
                },
                "rel": crate::config::normalize_rel_path(rel),
                "content": content,
                "chunk": chunk,
                "context": context,
            }),
            None => serde_json::json!({
                "rel": crate::config::normalize_rel_path(rel),
                "content": content,
            }),
        };
        let payload = serde_json::to_string(&payload)?;
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
        if let Err(err) = write_result {
            if err.kind() != std::io::ErrorKind::BrokenPipe {
                return Err(err).context("write external provider stdin");
            }
        }
        parse_proposals(&stdout)
    }
}

impl ProposalProvider for ExternalProposalProvider {
    fn propose(&self, rel: &Path, content: &str, config: &Config) -> Result<Vec<Proposal>> {
        let mut provider_config = config.clone();
        provider_config.sanitizer.denylist.clear();
        let content = crate::redact::Redactor::terms_only(&provider_config)
            .with_alias_pairs(self.semantic_aliases.clone())
            .redact(content);
        self.run(rel, &content, None)
    }

    fn propose_chunk_with_context(
        &self,
        rel: &Path,
        content: &str,
        config: &Config,
        chunk: ProposalChunkMeta,
        context: &ProposalRequestContext,
    ) -> Result<Vec<Proposal>> {
        let mut provider_config = config.clone();
        provider_config.sanitizer.denylist.clear();
        let content = crate::redact::Redactor::terms_only(&provider_config)
            .with_alias_pairs(self.semantic_aliases.clone())
            .redact(content);
        self.run(rel, &content, Some((chunk, context)))
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
                target: None,
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
/// receives the file after known mappings are pre-redacted and must answer with
/// a strict-JSON [`ProposalBatch`].
/// Its output goes through the same validation and review queue as any other
/// provider — it never touches the mirror.
pub struct LlmProposalProvider {
    pub client: crate::llm::OpenAiClient,
    pub model: String,
    /// Request strict-JSON output from the endpoint (`response_format`);
    /// opt-in via the provider config's `json_mode` key.
    pub json_mode: bool,
    pub semantic_aliases: Vec<(String, String)>,
}

// Some OpenAI-compatible gateways replace or heavily prefix the system prompt.
// Keep the authoritative task contract in the structured user message below;
// this short system instruction is only a first line of defense.
const LLM_SYSTEM_PROMPT: &str = "Return only valid JSON matching the schema provided by the user.";

impl ProposalProvider for LlmProposalProvider {
    fn propose(&self, rel: &Path, content: &str, config: &Config) -> Result<Vec<Proposal>> {
        self.propose_chunk_with_context(
            rel,
            content,
            config,
            ProposalChunkMeta::single(content),
            &ProposalRequestContext::default(),
        )
    }

    fn propose_chunk(
        &self,
        rel: &Path,
        content: &str,
        config: &Config,
        chunk: ProposalChunkMeta,
    ) -> Result<Vec<Proposal>> {
        self.propose_chunk_with_context(
            rel,
            content,
            config,
            chunk,
            &ProposalRequestContext::default(),
        )
    }

    fn propose_chunk_with_context(
        &self,
        rel: &Path,
        content: &str,
        config: &Config,
        chunk: ProposalChunkMeta,
        context: &ProposalRequestContext,
    ) -> Result<Vec<Proposal>> {
        // Known dictionary/registry terms need no model judgment. Redact them
        // before the remote boundary instead of sending a trigger-heavy
        // `already_mapped` list alongside the still-real occurrences. Keep the
        // denylist visible: unmapped denylisted terms are exactly what the
        // proposer must help name.
        let comment_free_content = mask_comments_for_proposal(rel, content);
        let mut provider_config = config.clone();
        provider_config.sanitizer.denylist.clear();
        let provider_redactor = crate::redact::Redactor::terms_only(&provider_config)
            .with_alias_pairs(self.semantic_aliases.clone());
        let provider_content = provider_redactor.redact(&comment_free_content);
        let core_offset = proposal_core_offset(&provider_content, chunk);
        let (context_before, provider_content) = provider_content.split_at(core_offset);
        let user = serde_json::to_string(&serde_json::json!({
            "request_mode": if content.is_empty() && context.semantic_candidates.is_empty() && !context.path_candidates.is_empty() { "path-only" } else { "source" },
            "task": "Recall-first review of owned symbols and path metadata. Propose every plausible private repository-owned name or security/abuse-adjacent spelling that could cause a false positive; use lower confidence instead of silently omitting an uncertain candidate. Human review, not the model, makes the final decision.",
            "rules": [
                "An identifier proposal must copy target.symbol_id and target.occurrence_id from one context.semantic_candidates entry; IDs not present in that list are forbidden",
                "A file_path proposal must copy target.path_id from one context.path_candidates entry; IDs not present in that list are forbidden",
                "For identifier proposals, original_text must equal the semantic candidate's complete name byte-for-byte and occur in file.content",
                "For file_path proposals, original_text must be an exact case-sensitive substring of target.value and target may only be a directory component or filename stem; extensions are out of scope",
                "Never change original_text capitalization, spelling, inflection, plurality, or separators; never join source tokens separated by whitespace or punctuation",
                "Never invent a label for a concept merely implied by the code; if the exact candidate is not present, omit it",
                "For security- or abuse-adjacent vocabulary, include terms associated with offensive-security behavior, credential or device identity abuse, evasion, injection, unauthorized automation, harmful payloads, or similar risk-loaded concepts when the file uses them benignly",
                "Include ordinary technical compounds when their complete spelling could plausibly be misclassified out of context; express uncertainty through confidence and rationale rather than omission",
                "A named entity is private only when it is non-public and owned by this repository or its operator; public third-party companies, products, services, integrations, standards, and vendor names are not private candidates",
                "Never propose a semantic candidate whose api_boundary is true",
                "Never propose a semantic candidate when both references_complete and compiler_resolvable are false; compiler_resolvable means approval will require a successful compiler/LSP reference closure",
                "Never invent targets for operating-system components, framework or library APIs, imported packages, SDK symbols, browser names, hardware vendor names, or protocols; every identifier target must still be an owned semantic candidate",
                "Treat context.indexed_external_identifiers as file-local ownership evidence. An owned target sharing a generic token with one of them is uncertain, not automatically forbidden; lower confidence and explain the overlap",
                "Do not suggest changes to imports, public APIs, protocol constants, SQL, or shell syntax",
                "Source comments are masked from file.content and are out of scope; never propose comment vocabulary",
                "The deterministic content allowlist is not a proposal exclusion; only policy.proposal_allowlist and policy.path_allowlist suppress their respective target surfaces",
                "Do not suggest identifier terms listed in policy.proposal_allowlist",
                "Do not suggest file_path terms listed in policy.path_allowlist",
                "Only identifier and file_path proposals are allowed; comments, strings, imports, external APIs, and partial source-identifier substrings are forbidden",
                "Both original_text and sanitized_text must each be exactly one ASCII word run matching [A-Za-z0-9_]+",
                "For identifier proposals, treat file.context_before only as read-only context; original_text must occur in file.content, which is the current chunk's owned analysis region",
                "Do not return a symbol listed in context.already_decided_symbol_ids",
                "Do not return a path term listed in context.already_decided_path_terms",
                "When request_mode is path-only, return only file_path proposals and inspect every supplied path candidate",
                "When category is identifier, sanitized_text must additionally match [A-Za-z_][A-Za-z0-9_]*",
                "When category is file_path, sanitized_text must be one neutral ASCII word-run and must not contain '/', '.', or whitespace",
                "Replacement text must not contain a term listed in policy.denylist",
                "Return strict JSON only, without prose or markdown"
            ],
            "required_output_preflight": [
                "For every identifier proposal, verify file.content contains original_text using an exact case-sensitive substring check",
                "For every file_path proposal, verify the selected path candidate value contains original_text using an exact case-sensitive substring check",
                "Remove every proposal that fails its corresponding exact substring check",
                "For identifier proposals, remove every proposal whose exact symbol_id is in context.already_decided_symbol_ids; same-spelling symbols with different IDs are independent",
                "For file_path proposals, remove every proposal already represented in context.already_decided_path_terms",
                "Remove every proposal whose original_text or sanitized_text fails the single ASCII word-run rule"
            ],
            "output_schema": {
                "proposals": [{
                    "target": "for identifier: {symbol_id, occurrence_id}; for file_path: {path_id}",
                    "category": "identifier or file_path",
                    "original_text": "string",
                    "sanitized_text": "string",
                    "confidence": "number from 0 to 1",
                    "rationale": "state whether this is a non-public repository-owned name or security/abuse-adjacent candidate, and give concrete evidence; never label a merely public product/vendor name private"
                }]
            },
            "empty_response": { "proposals": [] },
            "context": context,
            "policy": {
                "denylist": config.sanitizer.denylist,
                "proposal_allowlist": config.sanitizer.proposal_allowlist,
                "path_allowlist": config.sanitizer.path_allowlist,
                "known_terms_redacted": true
            },
            "file": {
                "rel": crate::config::normalize_rel_path(rel),
                "chunk": chunk,
                "context_before": context_before,
                "content": provider_content
            }
        }))?;
        let reply = self
            .client
            .chat(&self.model, LLM_SYSTEM_PROMPT, &user, self.json_mode)?;
        match parse_proposals(strip_code_fences(&reply)) {
            Ok(proposals) => Ok(proposals),
            Err(first_error) => {
                // A single malformed object or broken JSON response must not
                // permanently erase this chunk from a recall-oriented scan.
                // Re-run the same bounded request once with an explicit schema
                // correction. Transport/output-limit failures occur in
                // `chat` above and keep their existing retry/error behavior.
                let mut retry_task: serde_json::Value = serde_json::from_str(&user)?;
                retry_task["retry_instruction"] = serde_json::Value::String(
                    "The previous answer was invalid JSON or did not match the target schema. Re-evaluate this same request and return exactly one strict JSON object with a proposals array; copy target IDs exactly from context and omit any proposal whose target cannot be represented."
                        .to_string(),
                );
                let retry_user = serde_json::to_string(&retry_task)?;
                let retry_reply = self.client.chat(
                    &self.model,
                    LLM_SYSTEM_PROMPT,
                    &retry_user,
                    self.json_mode,
                )?;
                parse_proposals(strip_code_fences(&retry_reply)).map_err(|second_error| {
                    anyhow!(
                        "provider returned invalid proposal JSON/schema twice; first attempt: \
                         {first_error}; retry: {second_error}"
                    )
                })
            }
        }
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
        bail!("provider returned an empty response instead of a ProposalBatch");
    }
    if !trimmed.starts_with(['{', '[']) {
        let lower = trimmed.to_ascii_lowercase();
        if lower.contains("can't")
            || lower.contains("cannot")
            || lower.contains("won't")
            || lower.contains("refus")
        {
            bail!(
                "provider refused the proposal request instead of returning JSON; \
                 try another model/provider or revise the proposal task prompt"
            );
        }
        bail!(
            "provider returned non-JSON text instead of a ProposalBatch; \
             enable provider json_mode or use a model that follows the JSON schema"
        );
    }
    // Model gateways occasionally emit the same object key twice. Parsing via
    // Value normalizes that common JSON interoperability wart (last value
    // wins) before typed deserialization; every resulting proposal still goes
    // through the real-content and policy validation below.
    let value: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|err| anyhow!("parse proposals JSON: {err}"))?;
    let proposal_values = match value {
        serde_json::Value::Object(mut object) => match object.remove("proposals") {
            Some(serde_json::Value::Array(proposals)) => proposals,
            Some(_) => bail!("parse proposals: ProposalBatch.proposals must be an array"),
            None => bail!(
                "parse proposals: JSON object is not a ProposalBatch because it has no proposals array"
            ),
        },
        serde_json::Value::Array(proposals) => proposals,
        _ => bail!("parse proposals: expected a ProposalBatch object or proposal array"),
    };
    if proposal_values.is_empty() {
        return Ok(Vec::new());
    }

    let total = proposal_values.len();
    let mut proposals = Vec::with_capacity(total);
    let mut invalid = Vec::new();
    for (index, mut value) in proposal_values.into_iter().enumerate() {
        normalize_typed_target(&mut value);
        match serde_json::from_value::<Proposal>(value) {
            Ok(proposal) => proposals.push(proposal),
            Err(err) => invalid.push(format!("proposal {}: {err}", index + 1)),
        }
    }
    if proposals.is_empty() {
        bail!(
            "parse proposals: all {total} proposal object(s) failed the schema; first: {}",
            invalid[0]
        );
    }
    if !invalid.is_empty() {
        log::warn!(
            "ignored {} malformed proposal object(s) while preserving {} valid object(s): {}",
            invalid.len(),
            proposals.len(),
            invalid[0]
        );
    }
    Ok(proposals)
}

fn normalize_typed_target(proposal: &mut serde_json::Value) {
    let Some(target) = proposal
        .as_object_mut()
        .and_then(|proposal| proposal.get_mut("target"))
    else {
        return;
    };
    let Some(object) = target.as_object() else {
        return;
    };
    let semantic = object
        .get("symbol_id")
        .and_then(serde_json::Value::as_str)
        .zip(
            object
                .get("occurrence_id")
                .and_then(serde_json::Value::as_str),
        );
    if let Some((symbol_id, occurrence_id)) = semantic {
        *target = serde_json::json!({
            "symbol_id": symbol_id,
            "occurrence_id": occurrence_id,
        });
    } else if let Some(path_id) = object.get("path_id").and_then(serde_json::Value::as_str) {
        *target = serde_json::json!({ "path_id": path_id });
    }
}

/// Explicit human confirmations for providers that leave the process: External
/// executes a repo-supplied command, Llm posts real file content to a
/// repo-configured endpoint. Both default to refused.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProviderAllow {
    pub command: bool,
    pub endpoint: bool,
}

fn provider_for(
    root: &Path,
    config: &Config,
    allow: ProviderAllow,
) -> Result<Box<dyn ProposalProvider>> {
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
        let semantic_aliases = {
            let layout = Layout::new(root);
            let conn = db::connect(&layout)?;
            db::check_schema(&conn)?;
            crate::semantic_store::accepted_alias_pairs(&conn)?
                .into_iter()
                .map(|pair| (pair.original, pair.alias))
                .collect()
        };
        return Ok(Box::new(LlmProposalProvider {
            client: crate::llm::OpenAiClient::new(
                &endpoint.base_url,
                &endpoint.api_key_env,
                endpoint.timeout_secs,
            )?,
            model: endpoint.model,
            json_mode: endpoint.json_mode,
            semantic_aliases,
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
                // Same floor as the LLM client: a configured 0 must not
                // become a zero-duration deadline that kills every child.
                timeout: std::time::Duration::from_secs(timeout_secs.unwrap_or(60).max(1)),
                semantic_aliases: {
                    let layout = Layout::new(root);
                    let conn = db::connect(&layout)?;
                    db::check_schema(&conn)?;
                    crate::semantic_store::accepted_alias_pairs(&conn)?
                        .into_iter()
                        .map(|pair| (pair.original, pair.alias))
                        .collect()
                },
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
    propose_sanitize_with_progress(root, rel, allow, None, |_| {})
}

#[derive(Debug, Clone)]
struct ProposalChunk {
    meta: ProposalChunkMeta,
    content: String,
}

struct WorkItem {
    file_index: usize,
    file: String,
    projected_file: String,
    chunk: ProposalChunk,
}

struct FileState {
    position: usize,
    file: String,
    display_file: String,
    real: String,
    chunks: usize,
    remaining: usize,
    started_at: Option<Instant>,
    proposals: Vec<Proposal>,
    proposed: usize,
    duplicates: usize,
    rejected: Vec<String>,
    errors: Vec<String>,
    immediate: Option<ImmediateFileResult>,
}

enum ImmediateFileResult {
    Error(String),
    Skipped,
}

#[derive(Debug, Clone)]
struct PathProposalBatch {
    meta: ProposalChunkMeta,
    candidates: Vec<FilePathCandidate>,
}

enum PathWorkerEvent {
    Finished {
        wave_index: usize,
        meta: ProposalChunkMeta,
        elapsed_ms: u64,
        result: WorkerResult,
    },
}

enum WorkerEvent {
    ChunkFinished {
        wave_index: usize,
        file_index: usize,
        file: String,
        meta: ProposalChunkMeta,
        elapsed_ms: u64,
        result: WorkerResult,
    },
}

enum WorkerResult {
    Proposals(Vec<Proposal>),
    Error(String),
}

struct FileCompletion {
    outcome: ProposeFileOutcome,
    proposed: usize,
    queued: usize,
    duplicates: usize,
    rejected: usize,
    /// Only locally valid decisions suppress later requests. Rejected aliases
    /// remain retryable so a later chunk/model answer can propose a safe one.
    decided: Vec<(String, String)>,
}

struct FileProviderOutput {
    proposals: Vec<Proposal>,
    proposed: usize,
    duplicates: usize,
    rejected: Vec<String>,
    errors: Vec<String>,
}

struct ProposalPolicyContext<'a> {
    config: &'a Config,
    indexed_external: &'a BTreeMap<String, BTreeSet<String>>,
    indexed_words: &'a BTreeMap<String, (String, String)>,
    semantic_candidates: &'a BTreeMap<String, Vec<SemanticCandidate>>,
    semantic_alias_representatives: &'a BTreeMap<String, String>,
    path_candidates: &'a BTreeMap<String, Vec<FilePathCandidate>>,
    tracked_files: &'a [String],
}

#[derive(Debug, Clone)]
struct ProposalAliasReservation {
    identity: String,
    original: String,
    source: String,
    semantic: bool,
}

fn reserve_proposal_alias(
    reservations: &mut BTreeMap<String, ProposalAliasReservation>,
    proposal: &Proposal,
    representatives: &BTreeMap<String, String>,
    source: &str,
) -> std::result::Result<Option<String>, String> {
    let alias = normalize_term(&proposal.sanitized_text);
    let identity = proposal_run_identity(proposal, representatives);
    let semantic = matches!(proposal.target.as_ref(), Some(ProposalTarget::Semantic(_)));
    if let Some(existing) = reservations.get(&alias) {
        if existing.identity == identity {
            return Ok(None);
        }
        let same_semantic_spelling = semantic
            && existing.semantic
            && normalize_term(&existing.original) == normalize_term(&proposal.original_text);
        if same_semantic_spelling {
            return Ok(Some(format!(
                "alias is also reserved for the same spelling by {}; compiler identity will be verified during approval",
                existing.source
            )));
        }
        return Err(format!(
            "alias {:?} is already reserved for {:?} by {}; pick a workspace-unique alias",
            proposal.sanitized_text, existing.original, existing.source
        ));
    }
    reservations.insert(
        alias,
        ProposalAliasReservation {
            identity,
            original: proposal.original_text.clone(),
            source: source.to_string(),
            semantic,
        },
    );
    Ok(None)
}

/// Run the proposal provider with bounded concurrency and publish progress from
/// the coordinating thread. `jobs = None` uses
/// `sanitizer.propose_concurrency`; callers can override it for one run.
pub fn propose_sanitize_with_progress(
    root: &Path,
    rel: Option<&Path>,
    allow: ProviderAllow,
    jobs: Option<usize>,
    mut progress: impl FnMut(ProposeProgress),
) -> Result<ProposeReport> {
    // Plain init (the wrapper drops the exclusive lock): provider calls may
    // block on HTTP or a child process and must not starve workspace writers.
    let layout = crate::index::init_workspace(root)?;
    let config = Config::load_or_default(&layout)?;
    let provider = provider_for(root, &config, allow)?;

    // `propose-sanitize` is a complete entry point, including from a fresh
    // TUI workspace. Populate derived state before scope resolution when no
    // files have ever been indexed; provider permission/key preflight above
    // still happens before this mutation.
    let needs_initial_index = {
        let _lock = WorkspaceLock::acquire_shared(&layout)?;
        let conn = db::connect(&layout)?;
        db::check_schema(&conn)?;
        db::tracked_files(&conn)?.is_empty()
    };
    if needs_initial_index {
        crate::index::index_workspace(root).context("initial index before proposal scan")?;
    }

    // One short shared-lock snapshot supplies both the file set and the
    // ownership evidence produced by `index`. Provider calls happen after the
    // lock is dropped.
    let (
        tracked_files,
        index_states,
        semantic_candidates,
        path_candidates,
        path_projection,
        accepted_aliases,
        semantic_alias_representatives,
    ) = {
        let _lock = WorkspaceLock::acquire_shared(&layout)?;
        let conn = db::connect(&layout)?;
        db::check_schema(&conn)?;
        let files = db::tracked_files(&conn)?;
        let states = db::all_index_states(&conn)?;
        let projection = crate::path_projection::PathProjection::from_connection(&config, &conn)?;
        let candidates = semantic_candidates_by_file(root, &conn, &projection)?;
        let path_candidates = file_path_candidates_by_file(&files, &projection)?;
        let accepted_aliases = crate::semantic_store::accepted_alias_bindings(&conn)?;
        let semantic_alias_representatives = semantic_alias_representatives(root, &conn, &[])?;
        (
            files,
            states,
            candidates,
            path_candidates,
            projection,
            accepted_aliases,
            semantic_alias_representatives,
        )
    };
    let indexed_words = indexed_word_owners(root, &tracked_files, &path_projection)?;
    let files = match rel {
        // The path is repo-config-adjacent input and the file's REAL content
        // goes to a provider: never allow it to point outside the repo.
        Some(rel) => {
            let safe = crate::config::normalize_safe_rel_path(rel, "repo")?;
            // Projected spelling wins over the raw fallback. Otherwise an
            // unrelated skipped file/directory whose real name happens to
            // equal a tracked projected path could redirect the scan.
            let real_scope = path_projection.real_for_agent(&safe)?;
            let normalized = crate::config::normalize_rel_path(&real_scope);
            if root.join(&real_scope).is_dir() {
                let prefix = format!("{}/", normalized.trim_end_matches('/'));
                let selected = tracked_files
                    .iter()
                    .filter(|file| file.starts_with(&prefix))
                    .cloned()
                    .collect::<Vec<_>>();
                if selected.is_empty() {
                    bail!("no indexed files under {normalized}; run `code-sanity index`");
                }
                selected
            } else {
                vec![normalized]
            }
        }
        None => tracked_files.clone(),
    };
    let selected_files = files.iter().cloned().collect::<BTreeSet<_>>();
    let semantic_candidate_owners = semantic_candidate_owners(
        &selected_files,
        &semantic_candidates,
        &semantic_alias_representatives,
    );
    let indexed_external = index_states
        .into_iter()
        .filter(|state| selected_files.contains(&state.rel_path))
        .map(|state| {
            let external = state.external();
            (state.rel_path, external)
        })
        .collect::<BTreeMap<_, _>>();

    let started_at = Instant::now();
    let pending_items = list_review(root, false)?;
    let mut alias_reservations = BTreeMap::<String, ProposalAliasReservation>::new();
    for pair in accepted_aliases {
        alias_reservations
            .entry(normalize_term(&pair.alias))
            .or_insert(ProposalAliasReservation {
                identity: semantic_run_identity(&pair.symbol_id, &semantic_alias_representatives),
                original: pair.original,
                source: "an accepted semantic alias".to_string(),
                semantic: true,
            });
    }
    for item in &pending_items {
        alias_reservations
            .entry(normalize_term(&item.proposal.sanitized_text))
            .or_insert(ProposalAliasReservation {
                identity: proposal_run_identity(&item.proposal, &semantic_alias_representatives),
                original: item.proposal.original_text.clone(),
                source: format!("pending proposal {}", item.id),
                semantic: matches!(
                    item.proposal.target.as_ref(),
                    Some(ProposalTarget::Semantic(_))
                ),
            });
    }
    let mut pending_keys: BTreeSet<String> = pending_items
        .iter()
        .map(|item| proposal_run_identity(&item.proposal, &semantic_alias_representatives))
        .collect();
    let mut already_proposed = BTreeMap::<String, String>::new();
    for item in pending_items {
        already_proposed
            .entry(proposal_run_identity(
                &item.proposal,
                &semantic_alias_representatives,
            ))
            .or_insert(item.proposal.original_text);
    }
    for (identity, decision) in load_decision_ledger(&layout)?.decisions {
        let identity = semantic_run_identity(&identity, &semantic_alias_representatives);
        pending_keys.insert(identity.clone());
        already_proposed
            .entry(identity)
            .or_insert(decision.original_text);
    }
    let eligibility = proposal_eligibility(
        &selected_files,
        &semantic_candidates,
        &path_candidates,
        &already_proposed,
        &semantic_alias_representatives,
        &semantic_candidate_owners,
    );
    let mut report = ProposeReport {
        eligibility,
        ..ProposeReport::default()
    };
    let (path_inventory, path_owners) = unique_path_inventory(&selected_files, &path_candidates);
    let path_batches =
        path_proposal_batches(&path_inventory, config.sanitizer.propose_path_batch_size);
    let total = files.len();
    let chunk_llm_files = config.sanitizer.provider.llm_endpoint().is_some();
    let mut file_states = Vec::with_capacity(total);
    let mut work_items = Vec::new();
    for (file_index, file) in files.into_iter().enumerate() {
        let display_file = path_projection.projected_string_for_real(&file)?;
        let position = file_index + 1;
        let read = std::fs::read_to_string(root.join(&file));
        let (real, chunks, immediate, content_skip) = match read {
            Err(err) => (
                String::new(),
                Vec::new(),
                Some(ImmediateFileResult::Error(format!("read failed: {err}"))),
                None,
            ),
            Ok(real) if real.len() as u64 > config.sanitizer.propose_max_file_bytes => {
                let reason = format!(
                    "{} bytes exceeds sanitizer.propose_max_file_bytes ({})",
                    real.len(),
                    config.sanitizer.propose_max_file_bytes
                );
                (
                    String::new(),
                    Vec::new(),
                    Some(ImmediateFileResult::Skipped),
                    Some(reason),
                )
            }
            Ok(real) => {
                let chunks = if chunk_llm_files {
                    let proposal_source = mask_comments_for_proposal(Path::new(&file), &real);
                    split_proposal_chunks(
                        &proposal_source,
                        config.sanitizer.propose_chunk_bytes,
                        config.sanitizer.propose_chunk_overlap_lines,
                    )
                } else {
                    vec![ProposalChunk {
                        meta: ProposalChunkMeta::single(&real),
                        content: real.clone(),
                    }]
                };
                (real, chunks, None, None)
            }
        };
        if let Some(reason) = content_skip {
            report.skipped.push(format!(
                "{display_file}: source content skipped ({reason}); path metadata still proposed"
            ));
        }
        let chunk_count = chunks.len();
        for chunk in chunks {
            work_items.push(WorkItem {
                file_index,
                file: file.clone(),
                projected_file: display_file.clone(),
                chunk,
            });
        }
        file_states.push(FileState {
            position,
            file,
            display_file,
            real,
            chunks: chunk_count,
            remaining: chunk_count,
            started_at: None,
            proposals: Vec::new(),
            proposed: 0,
            duplicates: 0,
            rejected: Vec::new(),
            errors: Vec::new(),
            immediate,
        });
    }

    report.eligibility.sent_symbol_candidates = work_items
        .iter()
        .flat_map(|item| {
            semantic_candidates
                .get(&item.file)
                .into_iter()
                .flatten()
                .filter(|candidate| {
                    semantic_candidate_owners.contains(&candidate.symbol_id)
                        && !candidate.api_boundary
                        && semantic_candidate_is_resolvable(candidate)
                        && candidate.existing_alias.is_none()
                        && !already_proposed.contains_key(&semantic_run_identity(
                            &candidate.symbol_id,
                            &semantic_alias_representatives,
                        ))
                        && candidate.occurrence_lines.iter().any(|line| {
                            *line >= item.chunk.meta.start_line && *line <= item.chunk.meta.end_line
                        })
                })
                .map(|candidate| candidate.symbol_id.as_str())
        })
        .collect::<BTreeSet<_>>()
        .len();

    let request_total = work_items.len() + path_batches.len();
    let jobs = jobs
        .unwrap_or(config.sanitizer.propose_concurrency)
        .min(32)
        .clamp(1, request_total.max(1));
    progress(ProposeProgress::Started {
        total,
        jobs,
        requests: request_total,
    });

    let mut completed_files = 0usize;
    for state in &mut file_states {
        let Some(immediate) = state.immediate.take() else {
            continue;
        };
        progress(ProposeProgress::FileStarted {
            position: state.position,
            total,
            file: state.display_file.clone(),
            chunks: 0,
        });
        let outcome = match immediate {
            ImmediateFileResult::Error(reason) => {
                report
                    .errors
                    .push(format!("{}: {reason}", state.display_file));
                ProposeFileOutcome::Error
            }
            ImmediateFileResult::Skipped => ProposeFileOutcome::Skipped,
        };
        completed_files += 1;
        progress(ProposeProgress::FileFinished {
            completed: completed_files,
            total,
            file: state.display_file.clone(),
            elapsed_ms: 0,
            outcome,
            proposed: 0,
            queued: 0,
            duplicates: 0,
            rejected: 0,
        });
    }

    let mut completed_requests = 0usize;
    let mut successful_requests = 0usize;
    run_path_proposal_batches(
        provider.as_ref(),
        &config,
        &layout,
        &path_batches,
        &path_owners,
        &tracked_files,
        jobs,
        request_total,
        &mut completed_requests,
        &mut successful_requests,
        &mut already_proposed,
        &mut pending_keys,
        &mut alias_reservations,
        &semantic_alias_representatives,
        &mut report,
        &mut progress,
    )?;
    for wave in work_items.chunks(jobs) {
        let contexts = wave
            .iter()
            .map(|item| ProposalRequestContext {
                // Semantic/content and path decisions are deliberately
                // independent: the same spelling may need a symbol-scoped
                // alias, a path-only alias, or both.
                already_proposed_originals: already_proposed
                    .iter()
                    .filter(|(key, _)| key.starts_with("legacy:"))
                    .map(|(_, original)| original.clone())
                    .collect(),
                already_decided_symbol_ids: already_proposed
                    .keys()
                    .filter(|key| !key.starts_with("path-term:") && !key.starts_with("legacy:"))
                    .cloned()
                    .collect(),
                already_decided_path_terms: already_proposed
                    .iter()
                    .filter(|(key, _)| key.starts_with("path-term:"))
                    .map(|(_, original)| original.clone())
                    .collect(),
                indexed_external_identifiers: indexed_external
                    .get(&item.file)
                    .into_iter()
                    .flat_map(|identifiers| identifiers.iter().take(128).cloned())
                    .collect(),
                semantic_candidates: semantic_candidates
                    .get(&item.file)
                    .into_iter()
                    .flatten()
                    .filter(|candidate| {
                        semantic_candidate_owners.contains(&candidate.symbol_id)
                            && !candidate.api_boundary
                            && semantic_candidate_is_resolvable(candidate)
                            && candidate.existing_alias.is_none()
                            && !already_proposed.contains_key(&semantic_run_identity(
                                &candidate.symbol_id,
                                &semantic_alias_representatives,
                            ))
                            && candidate.occurrence_lines.iter().any(|line| {
                                *line >= item.chunk.meta.start_line
                                    && *line <= item.chunk.meta.end_line
                            })
                    })
                    .cloned()
                    .collect(),
                // Path metadata has its own deduplicated batched pass and no
                // longer competes with source-symbol analysis for attention.
                path_candidates: Vec::new(),
            })
            .collect::<Vec<_>>();

        for item in wave {
            let state = &mut file_states[item.file_index];
            if state.started_at.is_none() {
                state.started_at = Some(Instant::now());
                progress(ProposeProgress::FileStarted {
                    position: state.position,
                    total,
                    file: item.projected_file.clone(),
                    chunks: state.chunks,
                });
            }
            progress(ProposeProgress::ChunkStarted {
                file: item.projected_file.clone(),
                chunk: item.chunk.meta.index,
                chunks: item.chunk.meta.total,
            });
        }

        let (tx, rx) = mpsc::channel();
        let mut wave_results = Vec::with_capacity(wave.len());
        std::thread::scope(|scope| {
            for (wave_index, item) in wave.iter().enumerate() {
                let tx = tx.clone();
                let provider = &provider;
                let config = &config;
                let context = &contexts[wave_index];
                scope.spawn(move || {
                    let request_started = Instant::now();
                    let result = match provider.propose_chunk_with_context(
                        Path::new(&item.projected_file),
                        &item.chunk.content,
                        config,
                        item.chunk.meta,
                        context,
                    ) {
                        Ok(proposals) => WorkerResult::Proposals(proposals),
                        Err(err) => WorkerResult::Error(format!("{err:#}")),
                    };
                    let _ = tx.send(WorkerEvent::ChunkFinished {
                        wave_index,
                        file_index: item.file_index,
                        file: item.projected_file.clone(),
                        meta: item.chunk.meta,
                        elapsed_ms: elapsed_ms(request_started),
                        result,
                    });
                });
            }
            drop(tx);

            for event in rx {
                match &event {
                    WorkerEvent::ChunkFinished {
                        file,
                        meta,
                        elapsed_ms: request_elapsed_ms,
                        result,
                        ..
                    } => {
                        completed_requests += 1;
                        let chunk_outcome = if matches!(result, WorkerResult::Error(_)) {
                            ProposeChunkOutcome::Error
                        } else {
                            ProposeChunkOutcome::Completed
                        };
                        progress(ProposeProgress::ChunkFinished {
                            completed: completed_requests,
                            total: request_total,
                            file: file.clone(),
                            chunk: meta.index,
                            chunks: meta.total,
                            elapsed_ms: *request_elapsed_ms,
                            outcome: chunk_outcome,
                        });
                    }
                }
                wave_results.push(event);
            }
        });

        wave_results.sort_by_key(|event| match event {
            WorkerEvent::ChunkFinished { wave_index, .. } => *wave_index,
        });
        for event in wave_results {
            let WorkerEvent::ChunkFinished {
                wave_index,
                file_index,
                meta,
                result,
                ..
            } = event;
            let state = &mut file_states[file_index];
            match result {
                WorkerResult::Proposals(proposals) => {
                    successful_requests += 1;
                    state.proposed += proposals.len();
                    let context_keys = contexts[wave_index]
                        .already_decided_symbol_ids
                        .iter()
                        .cloned()
                        .chain(
                            contexts[wave_index]
                                .already_decided_path_terms
                                .iter()
                                .map(|term| format!("path-term:{}", normalize_term(term))),
                        )
                        .collect::<BTreeSet<_>>();
                    let chunk = &wave[wave_index].chunk;
                    let core_content = proposal_core_content(&chunk.content, meta);
                    for mut proposal in proposals {
                        if let Err(reason) = attach_proposal_target(
                            &mut proposal,
                            &contexts[wave_index].semantic_candidates,
                            &contexts[wave_index].path_candidates,
                        ) {
                            state
                                .rejected
                                .push(format!("{}: {reason}", proposal.original_text));
                            continue;
                        }
                        let identity =
                            proposal_run_identity(&proposal, &semantic_alias_representatives);
                        let is_file_path = proposal.category == "file_path";
                        if context_keys.contains(&identity)
                            || (!is_file_path
                                && !core_content.contains(&proposal.original_text)
                                && chunk.content.contains(&proposal.original_text))
                        {
                            state.duplicates += 1;
                            continue;
                        }
                        if !is_file_path && !core_content.contains(&proposal.original_text) {
                            state.rejected.push(format!(
                                "{}: original text does not appear in the chunk's owned content",
                                proposal.original_text
                            ));
                            continue;
                        }
                        state.proposals.push(proposal);
                    }
                }
                WorkerResult::Error(reason) => state.errors.push(format!(
                    "chunk {}/{} (lines {}-{}): {reason}",
                    meta.index, meta.total, meta.start_line, meta.end_line
                )),
            }
            state.remaining -= 1;
            if state.remaining != 0 {
                continue;
            }

            let file = state.file.clone();
            let real = std::mem::take(&mut state.real);
            let provider_output = FileProviderOutput {
                proposals: std::mem::take(&mut state.proposals),
                proposed: state.proposed,
                duplicates: state.duplicates,
                rejected: std::mem::take(&mut state.rejected),
                errors: std::mem::take(&mut state.errors),
            };
            let file_elapsed = state.started_at.map(elapsed_ms).unwrap_or(0);
            let completion = commit_file_proposals(
                &layout,
                &file,
                &real,
                provider_output,
                &ProposalPolicyContext {
                    config: &config,
                    indexed_external: &indexed_external,
                    indexed_words: &indexed_words,
                    semantic_candidates: &semantic_candidates,
                    semantic_alias_representatives: &semantic_alias_representatives,
                    path_candidates: &path_candidates,
                    tracked_files: &tracked_files,
                },
                &mut ProposalCommitState {
                    pending_keys: &mut pending_keys,
                    alias_reservations: &mut alias_reservations,
                    report: &mut report,
                },
            )?;
            for (identity, original) in &completion.decided {
                already_proposed
                    .entry(identity.clone())
                    .or_insert_with(|| original.clone());
            }
            completed_files += 1;
            progress(ProposeProgress::FileFinished {
                completed: completed_files,
                total,
                file: state.display_file.clone(),
                elapsed_ms: file_elapsed,
                outcome: completion.outcome,
                proposed: completion.proposed,
                queued: completion.queued,
                duplicates: completion.duplicates,
                rejected: completion.rejected,
            });
        }
    }

    report.rejected.sort();
    report.errors.sort();
    report.skipped.sort();
    progress(ProposeProgress::Finished {
        total,
        requests: request_total,
        elapsed_ms: elapsed_ms(started_at),
        proposed: report.proposed,
        queued: report.queued,
        duplicates: report.duplicates,
        rejected: report.rejected.len(),
        skipped: report.skipped.len(),
        errors: report.errors.len(),
    });
    if request_total > 0 && successful_requests == 0 {
        bail!(
            "provider failed for all {request_total} request(s); first: {}",
            report.errors[0]
        );
    }
    Ok(report)
}

fn unique_path_inventory(
    selected_files: &BTreeSet<String>,
    candidates_by_file: &BTreeMap<String, Vec<FilePathCandidate>>,
) -> (Vec<FilePathCandidate>, BTreeMap<String, String>) {
    let mut candidates = BTreeMap::<String, FilePathCandidate>::new();
    let mut owners = BTreeMap::<String, String>::new();
    for file in selected_files {
        for candidate in candidates_by_file.get(file).into_iter().flatten() {
            candidates
                .entry(candidate.path_id.clone())
                .or_insert_with(|| candidate.clone());
            owners
                .entry(candidate.path_id.clone())
                .or_insert_with(|| file.clone());
        }
    }
    let mut candidates = candidates.into_values().collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.component_index.cmp(&right.component_index))
            .then_with(|| left.path_id.cmp(&right.path_id))
    });
    (candidates, owners)
}

fn path_proposal_batches(
    inventory: &[FilePathCandidate],
    batch_size: usize,
) -> Vec<PathProposalBatch> {
    if inventory.is_empty() {
        return Vec::new();
    }
    let batch_size = batch_size.max(1);
    let total = inventory.len().div_ceil(batch_size);
    inventory
        .chunks(batch_size)
        .enumerate()
        .map(|(index, candidates)| PathProposalBatch {
            meta: ProposalChunkMeta {
                index: index + 1,
                total,
                start_line: 1,
                end_line: 1,
                core_start_line: 1,
                core_end_line: 1,
            },
            candidates: candidates.to_vec(),
        })
        .collect()
}

struct PathProposalPass<'a> {
    provider: &'a dyn ProposalProvider,
    config: &'a Config,
    layout: &'a Layout,
    owners: &'a BTreeMap<String, String>,
    tracked_files: &'a [String],
    jobs: usize,
    request_total: usize,
    completed_requests: &'a mut usize,
    successful_requests: &'a mut usize,
    already_proposed: &'a mut BTreeMap<String, String>,
    pending_keys: &'a mut BTreeSet<String>,
    alias_reservations: &'a mut BTreeMap<String, ProposalAliasReservation>,
    semantic_alias_representatives: &'a BTreeMap<String, String>,
    report: &'a mut ProposeReport,
}

impl PathProposalPass<'_> {
    fn run(
        &mut self,
        batches: &[PathProposalBatch],
        progress: &mut impl FnMut(ProposeProgress),
    ) -> Result<()> {
        for wave in batches.chunks(self.jobs) {
            let contexts = wave
                .iter()
                .map(|batch| ProposalRequestContext {
                    already_proposed_originals: Vec::new(),
                    already_decided_symbol_ids: Vec::new(),
                    already_decided_path_terms: self
                        .already_proposed
                        .iter()
                        .filter(|(key, _)| key.starts_with("path-term:"))
                        .map(|(_, original)| original.clone())
                        .collect(),
                    indexed_external_identifiers: Vec::new(),
                    semantic_candidates: Vec::new(),
                    path_candidates: batch.candidates.clone(),
                })
                .collect::<Vec<_>>();
            for batch in wave {
                progress(ProposeProgress::ChunkStarted {
                    file: "path inventory".to_string(),
                    chunk: batch.meta.index,
                    chunks: batch.meta.total,
                });
            }

            let (tx, rx) = mpsc::channel();
            let mut results = Vec::with_capacity(wave.len());
            std::thread::scope(|scope| {
                for (wave_index, batch) in wave.iter().enumerate() {
                    let tx = tx.clone();
                    let context = &contexts[wave_index];
                    let provider = self.provider;
                    let config = self.config;
                    scope.spawn(move || {
                        let started = Instant::now();
                        let result = match provider.propose_chunk_with_context(
                            Path::new("path-inventory"),
                            "",
                            config,
                            batch.meta,
                            context,
                        ) {
                            Ok(proposals) => WorkerResult::Proposals(proposals),
                            Err(err) => WorkerResult::Error(format!("{err:#}")),
                        };
                        let _ = tx.send(PathWorkerEvent::Finished {
                            wave_index,
                            meta: batch.meta,
                            elapsed_ms: elapsed_ms(started),
                            result,
                        });
                    });
                }
                drop(tx);
                for event in rx {
                    let PathWorkerEvent::Finished {
                        meta,
                        elapsed_ms: request_elapsed_ms,
                        ref result,
                        ..
                    } = event;
                    *self.completed_requests += 1;
                    progress(ProposeProgress::ChunkFinished {
                        completed: *self.completed_requests,
                        total: self.request_total,
                        file: "path inventory".to_string(),
                        chunk: meta.index,
                        chunks: meta.total,
                        elapsed_ms: request_elapsed_ms,
                        outcome: if matches!(result, WorkerResult::Error(_)) {
                            ProposeChunkOutcome::Error
                        } else {
                            ProposeChunkOutcome::Completed
                        },
                    });
                    results.push(event);
                }
            });
            results.sort_by_key(|event| match event {
                PathWorkerEvent::Finished { wave_index, .. } => *wave_index,
            });
            for event in results {
                let PathWorkerEvent::Finished { meta, result, .. } = event;
                let batch = &batches[meta.index - 1];
                match result {
                    WorkerResult::Proposals(proposals) => {
                        *self.successful_requests += 1;
                        self.commit_batch(&batch.candidates, proposals)?;
                    }
                    WorkerResult::Error(reason) => self.report.errors.push(format!(
                        "path inventory batch {}/{}: {reason}",
                        meta.index, meta.total
                    )),
                }
            }
        }
        Ok(())
    }

    fn commit_batch(
        &mut self,
        candidates: &[FilePathCandidate],
        proposals: Vec<Proposal>,
    ) -> Result<()> {
        self.report.proposed += proposals.len();
        let mut unique = BTreeMap::<(String, String), Proposal>::new();
        for mut proposal in proposals {
            if let Err(reason) = attach_proposal_target(&mut proposal, &[], candidates) {
                self.report
                    .rejected
                    .push(format!("{}: {reason}", proposal.original_text));
                continue;
            }
            let key = (
                proposal_identity(&proposal),
                normalize_term(&proposal.sanitized_text),
            );
            match unique.entry(key) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(proposal);
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    self.report.duplicates += 1;
                    if proposal.confidence > entry.get().confidence {
                        entry.insert(proposal);
                    }
                }
            }
        }
        let mut proposals = unique.into_values().collect::<Vec<_>>();
        proposals.sort_by(|left, right| {
            proposal_identity(left)
                .cmp(&proposal_identity(right))
                .then_with(|| right.confidence.total_cmp(&left.confidence))
                .then_with(|| left.sanitized_text.cmp(&right.sanitized_text))
        });
        let mut decided_in_batch = BTreeSet::new();
        for proposal in proposals {
            let identity = proposal_identity(&proposal);
            if self.pending_keys.contains(&identity) || decided_in_batch.contains(&identity) {
                self.report.duplicates += 1;
                continue;
            }
            let mut flag = match validate_file_path_proposal(
                &proposal,
                candidates,
                self.config,
                self.tracked_files,
            ) {
                Ok(flag) => flag,
                Err(reason) => {
                    self.report
                        .rejected
                        .push(format!("{}: {reason}", proposal.original_text));
                    continue;
                }
            };
            let Some(ProposalTarget::FilePath(target)) = proposal.target.as_ref() else {
                continue;
            };
            if let Some(warning) = match reserve_proposal_alias(
                self.alias_reservations,
                &proposal,
                self.semantic_alias_representatives,
                "another proposal in this queue",
            ) {
                Ok(warning) => warning,
                Err(reason) => {
                    self.report
                        .rejected
                        .push(format!("{}: {reason}", proposal.original_text));
                    continue;
                }
            } {
                flag = combine_review_flags(&flag, &warning);
            }
            let owner = self
                .owners
                .get(&target.path_id)
                .ok_or_else(|| anyhow!("path proposal target has no owner file"))?;
            enqueue_review(self.layout, owner, &proposal, &flag)?;
            self.pending_keys.insert(identity.clone());
            self.already_proposed
                .insert(identity.clone(), proposal.original_text.clone());
            decided_in_batch.insert(identity);
            self.report.queued += 1;
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn run_path_proposal_batches(
    provider: &dyn ProposalProvider,
    config: &Config,
    layout: &Layout,
    batches: &[PathProposalBatch],
    owners: &BTreeMap<String, String>,
    tracked_files: &[String],
    jobs: usize,
    request_total: usize,
    completed_requests: &mut usize,
    successful_requests: &mut usize,
    already_proposed: &mut BTreeMap<String, String>,
    pending_keys: &mut BTreeSet<String>,
    alias_reservations: &mut BTreeMap<String, ProposalAliasReservation>,
    semantic_alias_representatives: &BTreeMap<String, String>,
    report: &mut ProposeReport,
    progress: &mut impl FnMut(ProposeProgress),
) -> Result<()> {
    PathProposalPass {
        provider,
        config,
        layout,
        owners,
        tracked_files,
        jobs,
        request_total,
        completed_requests,
        successful_requests,
        already_proposed,
        pending_keys,
        alias_reservations,
        semantic_alias_representatives,
        report,
    }
    .run(batches, progress)
}

struct ProposalCommitState<'a> {
    pending_keys: &'a mut BTreeSet<String>,
    alias_reservations: &'a mut BTreeMap<String, ProposalAliasReservation>,
    report: &'a mut ProposeReport,
}

fn commit_file_proposals(
    layout: &Layout,
    file: &str,
    real: &str,
    provider_output: FileProviderOutput,
    policy: &ProposalPolicyContext<'_>,
    state: &mut ProposalCommitState<'_>,
) -> Result<FileCompletion> {
    let ProposalCommitState {
        pending_keys,
        alias_reservations,
        report,
    } = state;
    let FileProviderOutput {
        proposals,
        proposed,
        duplicates: mut file_duplicates,
        rejected: pre_rejected,
        errors: provider_errors,
    } = provider_output;
    report.proposed += proposed;
    let mut unique = std::collections::BTreeMap::<(String, String), Proposal>::new();
    for proposal in proposals {
        let key = (
            proposal_run_identity(&proposal, policy.semantic_alias_representatives),
            normalize_term(&proposal.sanitized_text),
        );
        match unique.entry(key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(proposal);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                file_duplicates += 1;
                let current = entry.get();
                if proposal.confidence > current.confidence
                    || (proposal.confidence == current.confidence
                        && proposal.sanitized_text < current.sanitized_text)
                {
                    entry.insert(proposal);
                }
            }
        }
    }
    report.duplicates += file_duplicates;

    let mut file_queued = 0usize;
    let mut file_rejected = pre_rejected.len();
    let mut decided = BTreeMap::<String, String>::new();
    report.rejected.extend(pre_rejected);
    for mut proposal in unique.into_values() {
        let candidates = policy
            .semantic_candidates
            .get(file)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let path_candidates = policy
            .path_candidates
            .get(file)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if let Err(reason) = attach_proposal_target(&mut proposal, candidates, path_candidates) {
            report
                .rejected
                .push(format!("{}: {reason}", proposal.original_text));
            file_rejected += 1;
            continue;
        }
        match validate_proposal_with_index(
            Path::new(file),
            &proposal,
            real,
            path_candidates,
            policy,
        ) {
            Ok(mut flag) => {
                let key = proposal_run_identity(&proposal, policy.semantic_alias_representatives);
                if pending_keys.contains(&key) {
                    report.duplicates += 1;
                    file_duplicates += 1;
                } else {
                    let reservation_source = format!("proposal in {file}");
                    if let Some(warning) = match reserve_proposal_alias(
                        alias_reservations,
                        &proposal,
                        policy.semantic_alias_representatives,
                        &reservation_source,
                    ) {
                        Ok(warning) => warning,
                        Err(reason) => {
                            report
                                .rejected
                                .push(format!("{}: {reason}", proposal.original_text));
                            file_rejected += 1;
                            continue;
                        }
                    } {
                        flag = combine_review_flags(&flag, &warning);
                    }
                    pending_keys.insert(key);
                    enqueue_review(layout, file, &proposal, &flag)?;
                    decided
                        .entry(proposal_run_identity(
                            &proposal,
                            policy.semantic_alias_representatives,
                        ))
                        .or_insert_with(|| proposal.original_text.clone());
                    report.queued += 1;
                    file_queued += 1;
                }
            }
            Err(reason) => {
                report
                    .rejected
                    .push(format!("{}: {reason}", proposal.original_text));
                file_rejected += 1;
            }
        }
    }

    let outcome = if provider_errors.is_empty() {
        ProposeFileOutcome::Completed
    } else {
        let display_file = crate::path_projection::project_rel_path(Path::new(file), policy.config)
            .map(|path| crate::config::normalize_rel_path(&path))
            .unwrap_or_else(|_| file.to_string());
        report
            .errors
            .push(format!("{display_file}: {}", provider_errors.join("; ")));
        ProposeFileOutcome::Error
    };
    Ok(FileCompletion {
        outcome,
        proposed,
        queued: file_queued,
        duplicates: file_duplicates,
        rejected: file_rejected,
        decided: decided.into_iter().collect(),
    })
}

fn split_proposal_chunks(
    content: &str,
    target_bytes: usize,
    overlap_lines: usize,
) -> Vec<ProposalChunk> {
    if content.is_empty() || content.len() <= target_bytes {
        return vec![ProposalChunk {
            meta: ProposalChunkMeta::single(content),
            content: content.to_string(),
        }];
    }

    let mut lines = Vec::new();
    let mut offset = 0usize;
    for line in content.split_inclusive('\n') {
        let end = offset + line.len();
        lines.push((offset, end));
        offset = end;
    }
    if offset < content.len() {
        lines.push((offset, content.len()));
    }
    if lines.is_empty() {
        lines.push((0, content.len()));
    }

    let mut ranges = Vec::new();
    let mut start = 0usize;
    while start < lines.len() {
        let start_byte = lines[start].0;
        let mut end = start;
        while end < lines.len() {
            let prospective = lines[end].1 - start_byte;
            if end > start && prospective > target_bytes {
                break;
            }
            end += 1;
            if prospective >= target_bytes {
                break;
            }
        }
        ranges.push((start, end));
        if end == lines.len() {
            break;
        }
        let chunk_lines = end - start;
        let overlap = overlap_lines.min(chunk_lines / 2);
        start = end - overlap;
    }

    let total = ranges.len();
    ranges
        .iter()
        .copied()
        .enumerate()
        .map(|(index, (start, end))| ProposalChunk {
            meta: ProposalChunkMeta {
                index: index + 1,
                total,
                start_line: start + 1,
                end_line: end,
                core_start_line: if index == 0 {
                    start + 1
                } else {
                    ranges[index - 1].1 + 1
                },
                core_end_line: end,
            },
            content: content[lines[start].0..lines[end - 1].1].to_string(),
        })
        .collect()
}

fn proposal_core_offset(content: &str, meta: ProposalChunkMeta) -> usize {
    let context_lines = meta.core_start_line.saturating_sub(meta.start_line);
    if context_lines == 0 {
        return 0;
    }
    content
        .split_inclusive('\n')
        .take(context_lines)
        .map(str::len)
        .sum()
}

fn proposal_core_content(content: &str, meta: ProposalChunkMeta) -> &str {
    &content[proposal_core_offset(content, meta)..]
}

fn semantic_candidates_by_file(
    root: &Path,
    conn: &rusqlite::Connection,
    projection: &crate::path_projection::PathProjection,
) -> Result<BTreeMap<String, Vec<SemanticCandidate>>> {
    let mut statement = conn
        .prepare(
            r#"
            select s.rel_path, s.symbol_id, declaration.occurrence_id, s.name, s.kind,
                   s.qualified_name, declaration.start_line,
                   (select count(*) from semantic_occurrences refs
                    where refs.symbol_id = s.symbol_id and refs.role = 'reference'),
                   (select group_concat(ordered_occ.start_line, ',') from
                     (select all_occ.start_line from semantic_occurrences all_occ
                      where all_occ.symbol_id = s.symbol_id order by all_occ.start_line) ordered_occ),
                   not exists(select 1 from semantic_occurrences unresolved
                              where unresolved.role = 'unresolved'
                                and unresolved.rel_path = s.rel_path
                                and unresolved.name = s.name),
                   (select group_concat(call_occ.start_line, ',') from semantic_occurrences call_occ
                    join semantic_nodes call_node on call_node.node_id = call_occ.node_id
                    join semantic_nodes call_parent on call_parent.node_id = call_node.parent_node_id
                    where call_occ.symbol_id = s.symbol_id
                      and call_parent.kind in ('call_expression', 'macro_invocation')),
                   s.origin, a.sanitized_name
            from semantic_symbols s
            join semantic_occurrences declaration
              on declaration.symbol_id = s.symbol_id
             and declaration.role = 'declaration'
             and declaration.node_id = s.node_id
            left join semantic_aliases a
              on a.symbol_id = s.symbol_id and a.status in ('accepted', 'stale')
            where s.origin = 'owned'
            order by s.rel_path, declaration.start_byte
            "#,
        )
        .context("prepare semantic proposal candidates")?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                SemanticCandidate {
                    symbol_id: row.get(1)?,
                    occurrence_id: row.get(2)?,
                    name: row.get(3)?,
                    kind: row.get(4)?,
                    qualified_name: row.get(5)?,
                    declaration_line: row.get::<_, i64>(6)? as usize,
                    reference_count: row.get::<_, i64>(7)? as usize,
                    references_complete: row.get::<_, i64>(9)? != 0,
                    compiler_resolvable: false,
                    occurrence_lines: row
                        .get::<_, Option<String>>(8)?
                        .unwrap_or_default()
                        .split(',')
                        .filter_map(|line| line.parse().ok())
                        .collect(),
                    call_lines: row
                        .get::<_, Option<String>>(10)?
                        .unwrap_or_default()
                        .split(',')
                        .filter_map(|line| line.parse().ok())
                        .collect(),
                    signature: String::new(),
                    enclosing_code: String::new(),
                    api_boundary: false,
                    lexically_closed: false,
                    origin: row.get(11)?,
                    existing_alias: row.get(12)?,
                },
            ))
        })
        .context("query semantic proposal candidates")?;
    let mut by_file = BTreeMap::<String, Vec<SemanticCandidate>>::new();
    for row in rows {
        let (file, candidate) = row.context("read semantic proposal candidate")?;
        by_file.entry(file).or_default().push(candidate);
    }
    for (file, candidates) in &mut by_file {
        let display_file = projection.projected_string_for_real(file)?;
        let source = std::fs::read_to_string(root.join(file.as_str()))
            .with_context(|| format!("read semantic proposal source {display_file}"))?;
        let model_source = mask_comments_for_proposal(Path::new(file.as_str()), &source);
        let lines = model_source.lines().collect::<Vec<_>>();
        let protected = collect_protected_identifiers(Path::new(file.as_str()), &source);
        let semantic_provider = crate::semantic_store::load_document(conn, file)?
            .and_then(|document| document.capabilities.semantic_provider);
        for candidate in candidates {
            candidate.lexically_closed =
                crate::semantic_store::symbol_is_lexically_closed(conn, &candidate.symbol_id)?;
            candidate.signature = lines
                .get(candidate.declaration_line.saturating_sub(1))
                .map(|line| line.trim().to_string())
                .unwrap_or_default();
            let start = candidate
                .declaration_line
                .saturating_sub(2)
                .min(lines.len());
            let end = (candidate.declaration_line + 1).min(lines.len());
            candidate.enclosing_code = lines[start..end].join("\n");
            candidate.api_boundary = protected.contains(&candidate.name);
            if candidate.lexically_closed {
                candidate.references_complete =
                    crate::semantic_store::lexical_symbol_references_complete(
                        conn,
                        &candidate.symbol_id,
                    )?;
                candidate.compiler_resolvable = false;
            } else {
                candidate.references_complete = false;
                candidate.compiler_resolvable = semantic_provider.is_some();
            }
        }
    }
    Ok(by_file)
}

fn indexed_word_owners(
    root: &Path,
    files: &[String],
    projection: &crate::path_projection::PathProjection,
) -> Result<BTreeMap<String, (String, String)>> {
    let mut words = BTreeMap::new();
    for rel in files {
        let Ok(content) = std::fs::read_to_string(root.join(rel)) else {
            continue;
        };
        let display_file = projection.projected_string_for_real(rel)?;
        for (start, end) in crate::sanitize::word_runs(&content) {
            let word = &content[start..end];
            words
                .entry(normalize_term(word))
                .or_insert_with(|| (display_file.clone(), word.to_string()));
        }
    }
    Ok(words)
}

fn validate_proposal_with_index(
    rel_path: &Path,
    proposal: &Proposal,
    content: &str,
    path_candidates: &[FilePathCandidate],
    policy: &ProposalPolicyContext<'_>,
) -> std::result::Result<String, String> {
    if proposal.category == "file_path" {
        return validate_file_path_proposal(
            proposal,
            path_candidates,
            policy.config,
            policy.tracked_files,
        );
    }
    let mut flag = validate_proposal(rel_path, proposal, content, policy.config)?;
    if let Some(owner) = policy
        .indexed_external
        .get(&crate::config::normalize_rel_path(rel_path))
        .and_then(|identifiers| external_api_owner(&proposal.original_text, identifiers))
    {
        flag = combine_review_flags(
            &flag,
            &format!(
                "owned target overlaps file-local external identifier {owner:?}; verify ownership"
            ),
        );
    }
    let alias = normalize_term(&proposal.sanitized_text);
    if let Some((owner_file, existing)) = policy.indexed_words.get(&alias) {
        return Err(format!(
            "alias already occurs in indexed file {owner_file} as {existing:?}; pick a different alias"
        ));
    }
    Ok(flag)
}

fn attach_proposal_target(
    proposal: &mut Proposal,
    semantic_candidates: &[SemanticCandidate],
    path_candidates: &[FilePathCandidate],
) -> std::result::Result<(), String> {
    // The target shape is authoritative and unambiguous. Recover common model
    // schema drift (`category: "string"`, `"filename"`, etc.) instead of
    // throwing away an otherwise exact typed target.
    match proposal.target.as_ref() {
        Some(ProposalTarget::Semantic(_)) => proposal.category = "identifier".to_string(),
        Some(ProposalTarget::FilePath(_)) => proposal.category = "file_path".to_string(),
        None => {}
    }
    match proposal.category.as_str() {
        "identifier" => attach_semantic_target(proposal, semantic_candidates),
        "file_path" => attach_file_path_target(proposal, path_candidates),
        _ => Err("proposal category must be identifier or file_path".to_string()),
    }
}

fn attach_semantic_target(
    proposal: &mut Proposal,
    candidates: &[SemanticCandidate],
) -> std::result::Result<(), String> {
    let candidate = match &proposal.target {
        Some(ProposalTarget::Semantic(target)) => candidates.iter().find(|candidate| {
            candidate.symbol_id == target.symbol_id
                && candidate.occurrence_id == target.occurrence_id
        }),
        Some(ProposalTarget::FilePath(_)) => {
            return Err("identifier proposal has a file-path target".to_string());
        }
        None => {
            let mut matching = candidates
                .iter()
                .filter(|candidate| candidate.name == proposal.original_text);
            let first = matching.next();
            if matching.next().is_some() {
                return Err(
                    "multiple semantic symbols share this name; provider must return exact target IDs"
                        .to_string(),
                );
            }
            first
        }
    }
    .ok_or_else(|| "target IDs do not identify an existing owned symbol".to_string())?;
    if candidate.name != proposal.original_text {
        return Err("original_text must equal the target symbol's complete name".to_string());
    }
    if candidate.existing_alias.is_some() {
        return Err("target symbol already has an accepted alias".to_string());
    }
    if candidate.api_boundary {
        return Err("target is a public/API boundary and is not eligible for sanitization".into());
    }
    if !semantic_candidate_is_resolvable(candidate) {
        return Err("target has unresolved references and no compiler closure is available".into());
    }
    proposal.target = Some(ProposalTarget::Semantic(SemanticProposalTarget {
        symbol_id: candidate.symbol_id.clone(),
        occurrence_id: candidate.occurrence_id.clone(),
    }));
    Ok(())
}

fn attach_file_path_target(
    proposal: &mut Proposal,
    candidates: &[FilePathCandidate],
) -> std::result::Result<(), String> {
    let candidate = match &proposal.target {
        Some(ProposalTarget::FilePath(target)) => candidates
            .iter()
            .find(|candidate| candidate.path_id == target.path_id),
        Some(ProposalTarget::Semantic(_)) => {
            return Err("file_path proposal has a semantic-symbol target".to_string());
        }
        None => {
            let mut matching = candidates
                .iter()
                .filter(|candidate| candidate.value.contains(&proposal.original_text));
            let first = matching.next();
            if matching.next().is_some() {
                return Err(
                    "multiple path components contain this term; provider must return exact path_id"
                        .to_string(),
                );
            }
            first
        }
    }
    .ok_or_else(|| "path_id does not identify a current path candidate".to_string())?;
    if !candidate.value.contains(&proposal.original_text) {
        return Err(
            "original_text must be an exact case-sensitive substring of target.value".to_string(),
        );
    }
    proposal.target = Some(ProposalTarget::FilePath(FilePathProposalTarget {
        path_id: candidate.path_id.clone(),
    }));
    Ok(())
}

fn validate_file_path_proposal(
    proposal: &Proposal,
    candidates: &[FilePathCandidate],
    config: &Config,
    tracked_files: &[String],
) -> std::result::Result<String, String> {
    use crate::sanitize::{matchability_error, path_term_table};

    let target = match proposal.target.as_ref() {
        Some(ProposalTarget::FilePath(target)) => target,
        _ => return Err("file_path proposal is missing its path target".to_string()),
    };
    let candidate = candidates
        .iter()
        .find(|candidate| candidate.path_id == target.path_id)
        .ok_or_else(|| "path target is stale or no longer exists".to_string())?;
    if proposal.original_text.is_empty() {
        return Err("empty original text".to_string());
    }
    if let Some(reason) = matchability_error(&proposal.original_text) {
        return Err(reason);
    }
    if let Some(reason) = matchability_error(&proposal.sanitized_text) {
        return Err(format!("path alias {reason}"));
    }
    if !candidate.value.contains(&proposal.original_text) {
        return Err(
            "original text does not occur byte-for-byte in the targeted path component".to_string(),
        );
    }
    if normalize_term(&proposal.original_text) == normalize_term(&proposal.sanitized_text) {
        return Err("path alias equals the original".to_string());
    }
    let normalized = normalize_term(&proposal.original_text);
    if config
        .sanitizer
        .path_allowlist
        .iter()
        .any(|term| normalize_term(term) == normalized)
    {
        return Err("path term is allowlisted; must not be replaced".to_string());
    }
    if path_term_table(config)
        .iter()
        .any(|term| term.normalized == normalized)
    {
        return Err("path term already has a deterministic mapping".to_string());
    }

    let mut candidate_config = config.clone();
    candidate_config.sanitizer.path_alias_registry.insert(
        proposal.original_text.clone(),
        proposal.sanitized_text.clone(),
    );
    crate::sanitize::validate_sanitizer_config(&candidate_config)
        .map_err(|err| format!("path alias violates sanitizer policy: {err:#}"))?;
    crate::path_projection::PathProjection::build(&candidate_config, tracked_files.iter())
        .map_err(|_| {
            "path alias would collapse two tracked files or directories onto one projected path; pick a different alias"
                .to_string()
        })?;

    if proposal.confidence < config.sanitizer.confidence_threshold {
        return Ok(format!(
            "confidence {:.2} below threshold {:.2}; needs review",
            proposal.confidence, config.sanitizer.confidence_threshold
        ));
    }
    Ok("clean".to_string())
}

fn mask_comments_for_proposal(rel_path: &Path, content: &str) -> String {
    let language = crate::sanitize::detect_language(rel_path, content);
    if matches!(language.as_str(), "markdown" | "plaintext") {
        return content.to_string();
    }
    let strings = crate::sanitize::string_ranges(&language, content);
    let comments = crate::sanitize::comment_ranges(&language, content, &strings);
    if comments.is_empty() {
        return content.to_string();
    }

    let mut masked = content.as_bytes().to_vec();
    for range in comments {
        for byte in &mut masked[range.start..range.end] {
            if !matches!(*byte, b'\n' | b'\r') {
                *byte = b' ';
            }
        }
    }
    String::from_utf8(masked).expect("comment masking preserves valid UTF-8")
}

fn occurs_outside_comments(rel_path: &Path, content: &str, value: &str) -> bool {
    mask_comments_for_proposal(rel_path, content).contains(value)
}

fn external_api_owner<'a>(
    candidate: &str,
    indexed_external: &'a BTreeSet<String>,
) -> Option<&'a str> {
    let candidate_normalized = normalize_term(candidate);
    let candidate_tokens = identifier_tokens(candidate);
    indexed_external.iter().find_map(|external| {
        let external_normalized = normalize_term(external);
        let exact = candidate_normalized.len() >= 4 && candidate_normalized == external_normalized;
        let external_tokens = identifier_tokens(external);
        let distinctive_token = candidate_tokens.iter().any(|candidate_token| {
            candidate_token.len() >= 6 && external_tokens.contains(candidate_token)
        });
        if exact || distinctive_token {
            Some(external.as_str())
        } else {
            None
        }
    })
}

fn identifier_tokens(value: &str) -> BTreeSet<String> {
    let characters = value.chars().collect::<Vec<_>>();
    let mut tokens = BTreeSet::new();
    let mut current = String::new();
    for (index, character) in characters.iter().copied().enumerate() {
        if !character.is_ascii_alphanumeric() {
            if !current.is_empty() {
                tokens.insert(std::mem::take(&mut current));
            }
            continue;
        }
        let previous = index.checked_sub(1).and_then(|index| characters.get(index));
        let next = characters.get(index + 1);
        let camel_boundary = character.is_ascii_uppercase()
            && !current.is_empty()
            && (previous.is_some_and(|previous| previous.is_ascii_lowercase())
                || (previous.is_some_and(|previous| previous.is_ascii_uppercase())
                    && next.is_some_and(|next| next.is_ascii_lowercase())));
        if camel_boundary {
            tokens.insert(std::mem::take(&mut current));
        }
        current.push(character.to_ascii_lowercase());
    }
    if !current.is_empty() {
        tokens.insert(current);
    }
    tokens
}

fn combine_review_flags(existing: &str, added: &str) -> String {
    if existing == "clean" {
        added.to_string()
    } else if added == "clean" || existing.contains(added) {
        existing.to_string()
    } else {
        format!("{existing}; {added}")
    }
}

fn file_path_candidates_by_file(
    files: &[String],
    projection: &crate::path_projection::PathProjection,
) -> Result<BTreeMap<String, Vec<FilePathCandidate>>> {
    files
        .iter()
        .map(|file| Ok((file.clone(), file_path_candidates(file, projection)?)))
        .collect()
}

fn proposal_eligibility(
    selected_files: &BTreeSet<String>,
    semantic_candidates: &BTreeMap<String, Vec<SemanticCandidate>>,
    path_candidates: &BTreeMap<String, Vec<FilePathCandidate>>,
    already_proposed: &BTreeMap<String, String>,
    representatives: &BTreeMap<String, String>,
    candidate_owners: &BTreeSet<String>,
) -> ProposalEligibility {
    let mut report = ProposalEligibility::default();
    let mut path_ids = BTreeSet::new();
    for file in selected_files {
        for candidate in semantic_candidates.get(file).into_iter().flatten() {
            if !candidate_owners.contains(&candidate.symbol_id) {
                continue;
            }
            let resolvable = semantic_candidate_is_resolvable(candidate);
            report.owned_symbols += 1;
            report.compiler_resolvable_symbols += usize::from(candidate.compiler_resolvable);
            report.excluded_unresolved += usize::from(!resolvable);
            report.excluded_api_boundary += usize::from(candidate.api_boundary);
            report.excluded_existing_alias += usize::from(candidate.existing_alias.is_some());
            if resolvable && !candidate.api_boundary && candidate.existing_alias.is_none() {
                report.eligible_symbols += 1;
                report.pending_symbol_decisions += usize::from(already_proposed.contains_key(
                    &semantic_run_identity(&candidate.symbol_id, representatives),
                ));
            }
        }
        for candidate in path_candidates.get(file).into_iter().flatten() {
            path_ids.insert(candidate.path_id.as_str());
        }
    }
    report.unique_path_candidates = path_ids.len();
    report
}

fn file_path_candidates(
    real_file: &str,
    projection: &crate::path_projection::PathProjection,
) -> Result<Vec<FilePathCandidate>> {
    let real = crate::config::normalize_safe_rel_path(Path::new(real_file), "proposal path")?;
    let projected = projection.projected_for_real(&real)?;
    let projected_path = crate::config::normalize_rel_path(&projected);
    let real_parts = real
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let projected_parts = projected
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let mut real_prefix = std::path::PathBuf::new();
    let mut candidates = Vec::new();
    for (index, (real_part, projected_part)) in
        real_parts.iter().zip(projected_parts.iter()).enumerate()
    {
        real_prefix.push(real_part);
        let is_file = index + 1 == projected_parts.len();
        let (kind, value) = if is_file {
            (
                "filename_stem",
                Path::new(projected_part)
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or(projected_part),
            )
        } else {
            ("directory", projected_part.as_str())
        };
        if crate::sanitize::word_runs(value).is_empty() {
            continue;
        }
        let identity = format!(
            "{}\0{kind}",
            crate::config::normalize_rel_path(&real_prefix)
        );
        candidates.push(FilePathCandidate {
            path_id: format!(
                "path_{}",
                &crate::map::sha256_hex(identity.as_bytes())[..24]
            ),
            path: projected_path.clone(),
            component_index: index,
            kind: kind.to_string(),
            value: value.to_string(),
        });
    }
    Ok(candidates)
}

fn proposal_identity(proposal: &Proposal) -> String {
    match proposal.target.as_ref() {
        Some(ProposalTarget::Semantic(target)) => target.symbol_id.clone(),
        Some(ProposalTarget::FilePath(_)) => {
            format!("path-term:{}", normalize_term(&proposal.original_text))
        }
        None if proposal.category == "file_path" => {
            format!("path-term:{}", normalize_term(&proposal.original_text))
        }
        None => format!("legacy:{}", normalize_term(&proposal.original_text)),
    }
}

fn semantic_run_identity(symbol_id: &str, representatives: &BTreeMap<String, String>) -> String {
    representatives
        .get(symbol_id)
        .cloned()
        .unwrap_or_else(|| symbol_id.to_string())
}

fn proposal_run_identity(
    proposal: &Proposal,
    representatives: &BTreeMap<String, String>,
) -> String {
    match proposal.target.as_ref() {
        Some(ProposalTarget::Semantic(target)) => {
            semantic_run_identity(&target.symbol_id, representatives)
        }
        _ => proposal_identity(proposal),
    }
}

fn semantic_candidate_owners(
    selected_files: &BTreeSet<String>,
    candidates: &BTreeMap<String, Vec<SemanticCandidate>>,
    representatives: &BTreeMap<String, String>,
) -> BTreeSet<String> {
    let mut owners = BTreeMap::<String, (usize, String)>::new();
    for file in selected_files {
        let extension = Path::new(file)
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        // Definitions carry more behavioral context than declarations, so
        // prefer implementation files when both anchors are inside the scan.
        let rank = if matches!(extension.as_str(), "c" | "cc" | "cpp" | "cxx" | "m" | "mm") {
            0
        } else if matches!(extension.as_str(), "h" | "hh" | "hpp" | "hxx") {
            1
        } else {
            2
        };
        for candidate in candidates.get(file).into_iter().flatten() {
            let identity = semantic_run_identity(&candidate.symbol_id, representatives);
            let replacement = (rank, candidate.symbol_id.clone());
            if owners
                .get(&identity)
                .is_none_or(|current| replacement < *current)
            {
                owners.insert(identity, replacement);
            }
        }
    }
    owners
        .into_values()
        .map(|(_, symbol_id)| symbol_id)
        .collect()
}

fn elapsed_ms(started_at: Instant) -> u64 {
    started_at.elapsed().as_millis().min(u64::MAX as u128) as u64
}

/// Validate one proposal against the policy. `Ok(flag)` means it may be queued
/// (flag is "clean" or a human-review reason); `Err(reason)` means it is rejected
/// outright and never reaches the queue.
pub fn validate_proposal(
    rel_path: &Path,
    proposal: &Proposal,
    content: &str,
    config: &Config,
) -> std::result::Result<String, String> {
    validate_proposal_inner(rel_path, proposal, content, config, None)
}

fn validate_proposal_with_protected(
    rel_path: &Path,
    proposal: &Proposal,
    content: &str,
    config: &Config,
    protected: &BTreeSet<String>,
) -> std::result::Result<String, String> {
    validate_proposal_inner(rel_path, proposal, content, config, Some(protected))
}

fn validate_proposal_inner(
    rel_path: &Path,
    proposal: &Proposal,
    content: &str,
    config: &Config,
    protected: Option<&BTreeSet<String>>,
) -> std::result::Result<String, String> {
    use crate::sanitize::{matchability_error, normalize_term, term_table, word_runs};

    if proposal.original_text.is_empty() {
        return Err("empty original text".to_string());
    }
    if !matches!(proposal.category.as_str(), "identifier" | "string") {
        return Err("comment proposals are out of scope".to_string());
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
    if !occurs_outside_comments(rel_path, content, &proposal.original_text) {
        return Err("original text appears only inside comments".to_string());
    }
    let proposal_allowlisted = config
        .sanitizer
        .proposal_allowlist
        .iter()
        .any(|item| normalize_term(item) == normalize_term(&proposal.original_text));
    let legacy_content_allowlisted = proposal.target.is_none()
        && config
            .sanitizer
            .allowlist
            .iter()
            .any(|item| normalize_term(item) == normalize_term(&proposal.original_text));
    if proposal_allowlisted || legacy_content_allowlisted {
        return Err("term is allowlisted; must not be replaced".to_string());
    }
    if config
        .sanitizer
        .dictionary
        .keys()
        .chain(config.sanitizer.alias_registry.keys())
        .any(|term| term.eq_ignore_ascii_case(&proposal.original_text))
    {
        return Err("term already has a deterministic mapping".to_string());
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

    let touches_protected = protected
        .map(|protected| protected.contains(&proposal.original_text))
        .unwrap_or_else(|| {
            collect_protected_identifiers(rel_path, content).contains(&proposal.original_text)
        });
    if touches_protected {
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
        short_hash(&format!("{file}:{}", proposal_identity(proposal)))
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
    crate::fsutil::atomic_write(&path, &raw)
        .with_context(|| format!("write {}", path.display()))?;
    if let Some(ProposalTarget::Semantic(target)) = &proposal.target {
        let conn = db::connect(layout)?;
        db::check_schema(&conn)?;
        crate::semantic_store::record_proposal(
            &conn,
            &id,
            &target.symbol_id,
            &target.occurrence_id,
            &proposal.sanitized_text,
            &proposal.category,
            proposal.confidence,
            proposal.rationale.as_deref().unwrap_or(""),
            "pending",
            &item.created_at,
        )?;
    }
    Ok(())
}

pub fn list_review(root: &Path, include_resolved: bool) -> Result<Vec<ReviewItem>> {
    let layout = Layout::new(root);
    let config = Config::load_or_default_lenient(&layout).ok();
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
        let mut item: ReviewItem =
            serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        if let Some(config) = &config {
            if let Ok(projected) =
                crate::path_projection::project_rel_path(Path::new(&item.file), config)
            {
                item.file = crate::config::normalize_rel_path(&projected);
            }
        }
        if include_resolved || item.status == ReviewStatus::Pending {
            items.push(item);
        }
    }
    items.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(items)
}

/// Retire pending semantic reviews whose stable target disappeared after an
/// edit/delete/rename. Stale history remains inspectable, but no longer wedges
/// `verify` or blocks a fresh target identity.
pub(crate) fn reconcile_review_queue_locked(root: &Path, layout: &Layout) -> Result<usize> {
    let conn = db::connect(layout)?;
    db::check_schema(&conn)?;
    let read_dir = match std::fs::read_dir(&layout.review_dir) {
        Ok(read_dir) => read_dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => {
            return Err(err).with_context(|| format!("read {}", layout.review_dir.display()));
        }
    };
    let mut paths = read_dir
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.sort();
    let mut items = Vec::new();
    let mut target_seeds = Vec::<Option<PreparedCompilerProposalResolution>>::new();
    for path in paths {
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let raw = std::fs::read_to_string(&path)?;
        let Ok(item) = serde_json::from_str::<ReviewItem>(&raw) else {
            continue;
        };
        if item.status == ReviewStatus::Pending {
            if let Some(ProposalTarget::Semantic(target)) = item.proposal.target.as_ref() {
                target_seeds.push(Some(PreparedCompilerProposalResolution {
                    symbol_id: target.symbol_id.clone(),
                    document_fingerprint: String::new(),
                    provider: String::new(),
                    locations: Vec::new(),
                    equivalent_symbol_ids: BTreeSet::new(),
                }));
            }
        }
        items.push((path, item));
    }
    let representatives = semantic_alias_representatives(root, &conn, &target_seeds)?;
    let mut accepted_by_target =
        BTreeMap::<String, Vec<crate::semantic_store::SemanticAliasPair>>::new();
    for alias in crate::semantic_store::accepted_alias_bindings(&conn)? {
        accepted_by_target
            .entry(semantic_run_identity(&alias.symbol_id, &representatives))
            .or_default()
            .push(alias);
    }
    let mut retired_items = Vec::<(std::path::PathBuf, ReviewItem)>::new();
    let mut pending_by_target = BTreeMap::<String, Vec<(std::path::PathBuf, ReviewItem)>>::new();
    for (path, mut item) in items {
        if item.status != ReviewStatus::Pending {
            continue;
        }
        let Some(ProposalTarget::Semantic(target)) = item.proposal.target.as_ref() else {
            continue;
        };
        let valid = crate::semantic_store::load_symbol_with_path(&conn, &target.symbol_id)?
            .is_some_and(|(rel_path, symbol)| {
                rel_path == item.file && symbol.name == item.proposal.original_text
            })
            && crate::semantic_store::occurrences_for_symbol(&conn, &target.symbol_id)?
                .iter()
                .any(|(_, occurrence)| occurrence.occurrence_id == target.occurrence_id);
        if valid {
            let identity = semantic_run_identity(&target.symbol_id, &representatives);
            if let Some(accepted) = accepted_by_target.get(&identity) {
                item.status = ReviewStatus::Stale;
                if let Some(existing) = accepted
                    .iter()
                    .find(|existing| existing.alias == item.proposal.sanitized_text)
                {
                    item.flag = combine_review_flags(
                        &item.flag,
                        &format!(
                            "target already has accepted alias {:?}; this proposal is redundant",
                            existing.alias
                        ),
                    );
                } else {
                    let aliases = accepted
                        .iter()
                        .map(|existing| format!("{:?}", existing.alias))
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect::<Vec<_>>()
                        .join(", ");
                    item.flag = combine_review_flags(
                        &item.flag,
                        &format!(
                            "target already has accepted alias {aliases}; proposed alias {:?} is obsolete",
                            item.proposal.sanitized_text
                        ),
                    );
                }
                retired_items.push((path, item));
            } else {
                pending_by_target
                    .entry(identity)
                    .or_default()
                    .push((path, item));
            }
        } else {
            item.status = ReviewStatus::Stale;
            item.flag = combine_review_flags(
                &item.flag,
                "proposal target disappeared or no longer matches the indexed source",
            );
            retired_items.push((path, item));
        }
    }

    for alternatives in pending_by_target.values_mut() {
        if alternatives.len() < 2 {
            continue;
        }
        alternatives.sort_by(|left, right| {
            right
                .1
                .proposal
                .confidence
                .total_cmp(&left.1.proposal.confidence)
                .then_with(|| left.1.created_at.cmp(&right.1.created_at))
                .then_with(|| left.1.id.cmp(&right.1.id))
        });
        let canonical_id = alternatives[0].1.id.clone();
        let canonical_alias = alternatives[0].1.proposal.sanitized_text.clone();
        for (path, mut item) in alternatives.drain(1..) {
            item.status = ReviewStatus::Stale;
            item.flag = if item.proposal.sanitized_text == canonical_alias {
                combine_review_flags(
                    &item.flag,
                    &format!(
                        "duplicate declaration/definition proposal; canonical review is {canonical_id}"
                    ),
                )
            } else {
                combine_review_flags(
                    &item.flag,
                    &format!(
                        "alternative alias for the same semantic target; canonical review {canonical_id} proposes {canonical_alias:?} with higher confidence or earlier creation time"
                    ),
                )
            };
            retired_items.push((path, item));
        }
    }

    for (path, item) in &retired_items {
        let updated = serde_json::to_string_pretty(&item)?;
        crate::fsutil::atomic_write(path, &updated)
            .with_context(|| format!("retire stale review {}", item.id))?;
        crate::semantic_store::update_proposal_status(&conn, &item.id, "stale")?;
    }
    Ok(retired_items.len())
}

/// A language-server closure can prove two targets equivalent even when the
/// syntax-only queue reconciliation cannot (for example, platform-specific
/// implementations whose parameter names differ). Retire lower-quality
/// alternatives after those final representatives are available, before any
/// alias or review decision is committed.
fn retire_selected_target_alternatives(
    layout: &Layout,
    conn: &rusqlite::Connection,
    ids: &[String],
    representatives: &BTreeMap<String, String>,
) -> Result<BTreeSet<String>> {
    let mut accepted_by_target =
        BTreeMap::<String, Vec<crate::semantic_store::SemanticAliasPair>>::new();
    for alias in crate::semantic_store::accepted_alias_bindings(conn)? {
        accepted_by_target
            .entry(semantic_run_identity(&alias.symbol_id, representatives))
            .or_default()
            .push(alias);
    }
    let mut pending_by_target = BTreeMap::<String, Vec<(std::path::PathBuf, ReviewItem)>>::new();
    for id in ids {
        let path = layout.review_dir.join(format!("{id}.json"));
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read selected review item {id}"))?;
        let item: ReviewItem =
            serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        if item.status != ReviewStatus::Pending {
            continue;
        }
        let Some(ProposalTarget::Semantic(target)) = item.proposal.target.as_ref() else {
            continue;
        };
        pending_by_target
            .entry(semantic_run_identity(&target.symbol_id, representatives))
            .or_default()
            .push((path, item));
    }

    let mut retired = Vec::<(std::path::PathBuf, ReviewItem)>::new();
    for (identity, alternatives) in &mut pending_by_target {
        if let Some(accepted) = accepted_by_target.get(identity) {
            for (path, mut item) in alternatives.drain(..) {
                item.status = ReviewStatus::Stale;
                if let Some(existing) = accepted
                    .iter()
                    .find(|existing| existing.alias == item.proposal.sanitized_text)
                {
                    item.flag = combine_review_flags(
                        &item.flag,
                        &format!(
                            "target already has accepted alias {:?}; this proposal is redundant",
                            existing.alias
                        ),
                    );
                } else {
                    let aliases = accepted
                        .iter()
                        .map(|existing| format!("{:?}", existing.alias))
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect::<Vec<_>>()
                        .join(", ");
                    item.flag = combine_review_flags(
                        &item.flag,
                        &format!(
                            "target already has accepted alias {aliases}; proposed alias {:?} is obsolete",
                            item.proposal.sanitized_text
                        ),
                    );
                }
                retired.push((path, item));
            }
            continue;
        }
        if alternatives.len() < 2 {
            continue;
        }
        alternatives.sort_by(|left, right| {
            right
                .1
                .proposal
                .confidence
                .total_cmp(&left.1.proposal.confidence)
                .then_with(|| left.1.created_at.cmp(&right.1.created_at))
                .then_with(|| left.1.id.cmp(&right.1.id))
        });
        let canonical_id = alternatives[0].1.id.clone();
        let canonical_alias = alternatives[0].1.proposal.sanitized_text.clone();
        for (path, mut item) in alternatives.drain(1..) {
            item.status = ReviewStatus::Stale;
            item.flag = if item.proposal.sanitized_text == canonical_alias {
                combine_review_flags(
                    &item.flag,
                    &format!(
                        "duplicate compiler-equivalent proposal; canonical review is {canonical_id}"
                    ),
                )
            } else {
                combine_review_flags(
                    &item.flag,
                    &format!(
                        "alternative alias for the same compiler-proven target; canonical review {canonical_id} proposes {canonical_alias:?} with higher confidence or earlier creation time"
                    ),
                )
            };
            retired.push((path, item));
        }
    }

    let mut retired_ids = BTreeSet::new();
    for (path, item) in retired {
        let updated = serde_json::to_string_pretty(&item)?;
        crate::fsutil::atomic_write(&path, &updated)
            .with_context(|| format!("retire compiler-equivalent review {}", item.id))?;
        crate::semantic_store::update_proposal_status(conn, &item.id, "stale")?;
        retired_ids.insert(item.id);
    }
    Ok(retired_ids)
}

/// Resolve workspace alias collisions inside the explicit approval selection
/// before starting a language server. Reusing one alias for the same original
/// spelling is reversible and remains allowed; when different originals claim
/// the same normalized alias, keep the highest-confidence mapping and retire
/// the rest so Select All can make deterministic progress.
fn retire_selected_alias_collisions(layout: &Layout, ids: &[String]) -> Result<BTreeSet<String>> {
    let conn = db::connect(layout)?;
    db::check_schema(&conn)?;
    let mut reserved = BTreeMap::<String, BTreeSet<String>>::new();
    for pair in crate::semantic_store::accepted_alias_bindings(&conn)? {
        reserved
            .entry(normalize_term(&pair.alias))
            .or_default()
            .insert(normalize_term(&pair.original));
    }
    let config = Config::load_or_default(layout)?;
    for (original, alias) in &config.sanitizer.alias_registry {
        reserved
            .entry(normalize_term(alias))
            .or_default()
            .insert(normalize_term(original));
    }
    for (original, alias) in &config.sanitizer.path_alias_registry {
        reserved
            .entry(normalize_term(alias))
            .or_default()
            .insert(normalize_term(original));
    }
    {
        let mut statement = conn
            .prepare(
                r#"
                select distinct original_text, sanitized_text from replacements
                where policy_source != 'semantic-alias'
                "#,
            )
            .context("prepare selected lexical alias reservations")?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .context("query selected lexical alias reservations")?;
        for row in rows {
            let (original, alias) = row.context("read selected lexical alias reservation")?;
            reserved
                .entry(normalize_term(&alias))
                .or_default()
                .insert(normalize_term(&original));
        }
    }
    let mut natural_names = BTreeSet::new();
    {
        let mut statement = conn
            .prepare("select distinct name from semantic_symbols")
            .context("prepare selected natural-name reservations")?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .context("query selected natural-name reservations")?;
        for row in rows {
            let name = row.context("read selected natural-name reservation")?;
            natural_names.insert(normalize_term(&name));
        }
    }

    let mut candidates = BTreeMap::<String, Vec<(std::path::PathBuf, ReviewItem)>>::new();
    for id in ids {
        let path = layout.review_dir.join(format!("{id}.json"));
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read selected alias review {id}"))?;
        let item: ReviewItem =
            serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        if item.status == ReviewStatus::Pending {
            candidates
                .entry(normalize_term(&item.proposal.sanitized_text))
                .or_default()
                .push((path, item));
        }
    }

    let mut retired = Vec::<(std::path::PathBuf, ReviewItem)>::new();
    for (alias, mappings) in &mut candidates {
        let mut eligible = Vec::with_capacity(mappings.len());
        for (path, mut item) in mappings.drain(..) {
            let affects_source = !matches!(
                item.proposal.target.as_ref(),
                Some(ProposalTarget::FilePath(_))
            );
            if affects_source && natural_names.contains(alias) {
                item.status = ReviewStatus::Stale;
                item.flag = combine_review_flags(
                    &item.flag,
                    &format!(
                        "alias {:?} collides with an existing source symbol name",
                        item.proposal.sanitized_text
                    ),
                );
                retired.push((path, item));
            } else {
                eligible.push((path, item));
            }
        }
        *mappings = eligible;
        mappings.sort_by(|left, right| {
            right
                .1
                .proposal
                .confidence
                .total_cmp(&left.1.proposal.confidence)
                .then_with(|| left.1.created_at.cmp(&right.1.created_at))
                .then_with(|| left.1.id.cmp(&right.1.id))
        });
        if let Some(originals) = reserved.get(alias) {
            for (path, mut item) in mappings.drain(..) {
                let original = normalize_term(&item.proposal.original_text);
                if originals.contains(&original) {
                    continue;
                }
                item.status = ReviewStatus::Stale;
                item.flag = combine_review_flags(
                    &item.flag,
                    &format!(
                        "alias {:?} is already reserved for a different original spelling",
                        item.proposal.sanitized_text
                    ),
                );
                retired.push((path, item));
            }
            continue;
        }
        let Some((_, winner)) = mappings.first() else {
            continue;
        };
        let winning_original = normalize_term(&winner.proposal.original_text);
        let winning_id = winner.id.clone();
        let winning_spelling = winner.proposal.original_text.clone();
        for (path, mut item) in mappings.drain(..) {
            if normalize_term(&item.proposal.original_text) == winning_original {
                continue;
            }
            item.status = ReviewStatus::Stale;
            item.flag = combine_review_flags(
                &item.flag,
                &format!(
                    "alias {:?} is claimed by different originals; canonical review {winning_id} maps {winning_spelling:?} with higher confidence or earlier creation time",
                    item.proposal.sanitized_text
                ),
            );
            retired.push((path, item));
        }
    }

    let mut retired_ids = BTreeSet::new();
    for (path, item) in retired {
        let updated = serde_json::to_string_pretty(&item)?;
        crate::fsutil::atomic_write(&path, &updated)
            .with_context(|| format!("retire colliding selected review {}", item.id))?;
        crate::semantic_store::update_proposal_status(&conn, &item.id, "stale")?;
        retired_ids.insert(item.id);
    }
    Ok(retired_ids)
}

/// Re-run cheap deterministic validators before any compiler work. Queue
/// entries can outlive config/index changes, and an invalid path or identifier
/// alias should be retired instead of wasting an LSP pass and aborting every
/// otherwise-valid selected review.
fn retire_selected_invalid_proposals(
    root: &Path,
    layout: &Layout,
    ids: &[String],
) -> Result<BTreeSet<String>> {
    let conn = db::connect(layout)?;
    db::check_schema(&conn)?;
    let tracked = db::tracked_files(&conn)?;
    let mut candidate_config = Config::load_or_default(layout)?;
    let mut items = Vec::<(std::path::PathBuf, ReviewItem)>::new();
    for id in ids {
        let path = layout.review_dir.join(format!("{id}.json"));
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read selected deterministic review {id}"))?;
        let item: ReviewItem =
            serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        if item.status == ReviewStatus::Pending {
            items.push((path, item));
        }
    }
    items.sort_by(|left, right| {
        right
            .1
            .proposal
            .confidence
            .total_cmp(&left.1.proposal.confidence)
            .then_with(|| left.1.created_at.cmp(&right.1.created_at))
            .then_with(|| left.1.id.cmp(&right.1.id))
    });

    let mut source_cache = BTreeMap::<String, Arc<str>>::new();
    let mut protected_cache = BTreeMap::<String, BTreeSet<String>>::new();
    let mut retired = Vec::<(std::path::PathBuf, ReviewItem)>::new();
    for (path, mut item) in items {
        let validation = match item.proposal.target.as_ref() {
            Some(ProposalTarget::FilePath(_)) => {
                let projection = crate::path_projection::PathProjection::from_connection(
                    &candidate_config,
                    &conn,
                )
                .map_err(|error| format!("build current path projection: {error:#}"));
                projection.and_then(|projection| {
                    file_path_candidates(&item.file, &projection)
                        .map_err(|error| format!("resolve current path target: {error:#}"))
                        .and_then(|candidates| {
                            validate_file_path_proposal(
                                &item.proposal,
                                &candidates,
                                &candidate_config,
                                &tracked,
                            )
                        })
                })
            }
            Some(ProposalTarget::Semantic(_)) | None => {
                let source = match source_cache.get(&item.file) {
                    Some(source) => Ok(Arc::clone(source)),
                    None => {
                        let rel = crate::config::normalize_safe_rel_path(
                            Path::new(&item.file),
                            "selected proposal source",
                        )
                        .map_err(|error| error.to_string());
                        rel.and_then(|rel| {
                            std::fs::read_to_string(root.join(rel))
                                .map(Arc::<str>::from)
                                .map_err(|error| format!("read {}: {error}", item.file))
                        })
                        .inspect(|source| {
                            source_cache.insert(item.file.clone(), Arc::clone(source));
                        })
                    }
                };
                source.and_then(|source| {
                    let protected = protected_cache.entry(item.file.clone()).or_insert_with(|| {
                        collect_protected_identifiers(Path::new(&item.file), &source)
                    });
                    validate_proposal_with_protected(
                        Path::new(&item.file),
                        &item.proposal,
                        &source,
                        &candidate_config,
                        protected,
                    )
                })
            }
        };
        match validation {
            Ok(_) => match item.proposal.target.as_ref() {
                Some(ProposalTarget::FilePath(_)) => {
                    candidate_config.sanitizer.path_alias_registry.insert(
                        item.proposal.original_text.clone(),
                        item.proposal.sanitized_text.clone(),
                    );
                }
                None => {
                    candidate_config.sanitizer.alias_registry.insert(
                        item.proposal.original_text.clone(),
                        item.proposal.sanitized_text.clone(),
                    );
                }
                Some(ProposalTarget::Semantic(_)) => {}
            },
            Err(reason) => {
                item.status = ReviewStatus::Stale;
                item.flag = combine_review_flags(
                    &item.flag,
                    &format!("proposal no longer passes deterministic validation: {reason}"),
                );
                retired.push((path, item));
            }
        }
    }

    let mut retired_ids = BTreeSet::new();
    for (path, item) in retired {
        let updated = serde_json::to_string_pretty(&item)?;
        crate::fsutil::atomic_write(&path, &updated)
            .with_context(|| format!("retire invalid selected review {}", item.id))?;
        if matches!(
            item.proposal.target.as_ref(),
            Some(ProposalTarget::Semantic(_))
        ) {
            crate::semantic_store::update_proposal_status(&conn, &item.id, "stale")?;
        }
        retired_ids.insert(item.id);
    }
    Ok(retired_ids)
}

/// Compute the exact agent-facing path before/after a pending file_path
/// proposal. The real repository path is only an internal identity and is
/// never renamed.
pub fn preview_file_path_change(item: &ReviewItem) -> Result<(String, String)> {
    let Some(ProposalTarget::FilePath(_)) = item.proposal.target.as_ref() else {
        bail!("review item is not a file_path proposal");
    };
    let term = crate::sanitize::Term {
        raw: item.proposal.original_text.clone(),
        normalized: normalize_term(&item.proposal.original_text),
        replacement: item.proposal.sanitized_text.clone(),
        policy_source: "path-proposal-preview",
    };
    let before = item.file.clone();
    let path = crate::config::normalize_safe_rel_path(Path::new(&before), "path preview")?;
    let parts = path
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let mut after = std::path::PathBuf::new();
    for (index, part) in parts.iter().enumerate() {
        let is_file = index + 1 == parts.len();
        let next = if is_file {
            let path = Path::new(part);
            match (
                path.file_stem().and_then(|value| value.to_str()),
                path.extension().and_then(|value| value.to_str()),
            ) {
                (Some(stem), Some(extension)) if !stem.is_empty() => format!(
                    "{}.{}",
                    crate::sanitize::sanitize_unprotected_text(stem, std::slice::from_ref(&term)),
                    extension
                ),
                _ => crate::sanitize::sanitize_unprotected_text(part, std::slice::from_ref(&term)),
            }
        } else {
            crate::sanitize::sanitize_unprotected_text(part, std::slice::from_ref(&term))
        };
        after.push(next);
    }
    Ok((before, crate::config::normalize_rel_path(&after)))
}

fn connect_semantic_alias_targets(
    graph: &mut BTreeMap<String, BTreeSet<String>>,
    left: &str,
    right: &str,
) {
    graph
        .entry(left.to_string())
        .or_default()
        .insert(right.to_string());
    graph
        .entry(right.to_string())
        .or_default()
        .insert(left.to_string());
}

fn mask_cpp_signature_source(source: &str) -> String {
    let strings = crate::sanitize::string_ranges("c", source);
    let comments = crate::sanitize::comment_ranges("c", source, &strings);
    let mut bytes = source.as_bytes().to_vec();
    for range in &strings {
        let start = range.start.saturating_sub(1);
        let end = (range.end + 1).min(source.len());
        for byte in &mut bytes[start..end] {
            if !matches!(*byte, b'\n' | b'\r') {
                *byte = b' ';
            }
        }
    }
    for range in &comments {
        let start = range.start.saturating_sub(2);
        let block = source.as_bytes().get(start..start + 2) == Some(b"/*");
        let end = if block {
            (range.end + 2).min(source.len())
        } else {
            range.end.min(source.len())
        };
        for byte in &mut bytes[start..end] {
            if !matches!(*byte, b'\n' | b'\r') {
                *byte = b' ';
            }
        }
    }
    String::from_utf8(bytes).expect("masking UTF-8 with ASCII spaces preserves UTF-8")
}

fn cpp_namespace_aliases(source: &str) -> BTreeMap<String, String> {
    fn identifier_end(bytes: &[u8], start: usize) -> usize {
        let mut end = start;
        while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
            end += 1;
        }
        end
    }
    fn skip_space(bytes: &[u8], mut cursor: usize) -> usize {
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        cursor
    }

    let bytes = source.as_bytes();
    let keyword = b"namespace";
    let mut cursor = 0usize;
    let mut aliases = BTreeMap::new();
    while cursor + keyword.len() <= bytes.len() {
        let Some(offset) = source[cursor..].find("namespace") else {
            break;
        };
        let start = cursor + offset;
        let before_ok =
            start == 0 || !(bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_');
        let keyword_end = start + keyword.len();
        let after_ok = keyword_end == bytes.len()
            || !(bytes[keyword_end].is_ascii_alphanumeric() || bytes[keyword_end] == b'_');
        cursor = keyword_end;
        if !before_ok || !after_ok {
            continue;
        }
        let alias_start = skip_space(bytes, cursor);
        let alias_end = identifier_end(bytes, alias_start);
        if alias_end == alias_start {
            continue;
        }
        let equals = skip_space(bytes, alias_end);
        if bytes.get(equals) != Some(&b'=') {
            continue;
        }
        let target_start = skip_space(bytes, equals + 1);
        let Some(relative_end) = source[target_start..].find(';') else {
            continue;
        };
        let target_end = target_start + relative_end;
        let target = source[target_start..target_end]
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        let valid_target = !target.is_empty()
            && target
                .split("::")
                .filter(|component| !component.is_empty())
                .all(|component| {
                    component.bytes().enumerate().all(|(index, byte)| {
                        byte == b'_'
                            || byte.is_ascii_alphabetic()
                            || (index != 0 && byte.is_ascii_digit())
                    })
                });
        if valid_target {
            aliases.insert(source[alias_start..alias_end].to_string(), target);
        }
        cursor = target_end + 1;
    }
    aliases
}

fn expand_cpp_namespace_aliases(value: &str, aliases: &BTreeMap<String, String>) -> String {
    let mut expanded = value.to_string();
    let mut ordered = aliases.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|(alias, _)| std::cmp::Reverse(alias.len()));
    for _ in 0..4 {
        let mut changed = false;
        for (alias, target) in &ordered {
            let mut cursor = 0usize;
            while cursor + alias.len() <= expanded.len() {
                let Some(offset) = expanded[cursor..].find(alias.as_str()) else {
                    break;
                };
                let start = cursor + offset;
                let bytes = expanded.as_bytes();
                let before_ok = start == 0
                    || !(bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_');
                let mut after = start + alias.len();
                while after < bytes.len() && bytes[after].is_ascii_whitespace() {
                    after += 1;
                }
                let first_colon = after;
                if bytes.get(after) == Some(&b':') {
                    after += 1;
                    while after < bytes.len() && bytes[after].is_ascii_whitespace() {
                        after += 1;
                    }
                }
                let qualified =
                    bytes.get(first_colon) == Some(&b':') && bytes.get(after) == Some(&b':');
                if !before_ok || !qualified {
                    cursor = start + alias.len();
                    continue;
                }
                after += 1;
                expanded.replace_range(start..after, &format!("{target}::"));
                cursor = start + target.len() + 2;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    expanded
}

fn cpp_parameter_without_name(value: &str) -> String {
    let mut depth = 0i32;
    let mut default_start = None;
    for (index, character) in value.char_indices() {
        match character {
            '(' | '[' | '{' | '<' => depth += 1,
            ')' | ']' | '}' | '>' => depth = (depth - 1).max(0),
            '=' if depth == 0 => {
                default_start = Some(index);
                break;
            }
            _ => {}
        }
    }
    let value = value[..default_start.unwrap_or(value.len())].trim();
    if value.is_empty() || value == "void" || value == "..." {
        return value.to_string();
    }

    let bytes = value.as_bytes();
    let mut identifiers = Vec::<(usize, usize, bool)>::new();
    let mut cursor = 0usize;
    let mut nesting = 0i32;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'(' | b'[' | b'{' | b'<' => {
                nesting += 1;
                cursor += 1;
            }
            b')' | b']' | b'}' | b'>' => {
                nesting = (nesting - 1).max(0);
                cursor += 1;
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                let start = cursor;
                cursor += 1;
                while cursor < bytes.len()
                    && (bytes[cursor].is_ascii_alphanumeric() || bytes[cursor] == b'_')
                {
                    cursor += 1;
                }
                let mut before = start;
                while before > 0 && bytes[before - 1].is_ascii_whitespace() {
                    before -= 1;
                }
                let mut after = cursor;
                while after < bytes.len() && bytes[after].is_ascii_whitespace() {
                    after += 1;
                }
                let qualified = (before >= 2 && &bytes[before - 2..before] == b"::")
                    || (after + 2 <= bytes.len() && &bytes[after..after + 2] == b"::");
                identifiers.push((start, cursor, nesting == 0 && !qualified));
            }
            _ => cursor += 1,
        }
    }
    let keywords = [
        "alignas", "auto", "bool", "char", "char8_t", "char16_t", "char32_t", "double", "float",
        "int", "long", "short", "signed", "unsigned", "void", "wchar_t",
    ];
    let candidates = identifiers
        .iter()
        .filter(|(_, _, eligible)| *eligible)
        .filter(|(start, end, _)| !keywords.contains(&&value[*start..*end]))
        .copied()
        .collect::<Vec<_>>();
    let Some((name_start, name_end, _)) = candidates.last().copied() else {
        return value
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
    };
    if !value[name_end..].trim().is_empty() {
        return value
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
    }
    let has_other_type_name = candidates.len() >= 2;
    let has_builtin_type = identifiers
        .iter()
        .any(|(start, end, top_level)| *top_level && keywords.contains(&&value[*start..*end]));
    let prefix = &value[..name_start];
    let has_declarator_evidence = prefix.contains("::")
        || prefix.contains('*')
        || prefix.contains('&')
        || prefix.contains('>')
        || prefix.contains(']')
        || prefix.contains(')');
    let without_name = if has_other_type_name || has_builtin_type || has_declarator_evidence {
        format!("{}{}", &value[..name_start], &value[name_end..])
    } else {
        value.to_string()
    };
    without_name
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn canonical_cpp_declarator_tail(value: &str, aliases: &BTreeMap<String, String>) -> String {
    let expanded = expand_cpp_namespace_aliases(value, aliases);
    let Some(open) = expanded.find('(') else {
        return expanded
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
    };
    let mut depth = 0i32;
    let mut close = None;
    for (offset, character) in expanded[open..].char_indices() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(open + offset);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(close) = close else {
        return expanded
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
    };
    let parameters = &expanded[open + 1..close];
    let mut canonical_parameters = Vec::new();
    let mut start = 0usize;
    let mut nesting = 0i32;
    for (index, character) in parameters.char_indices() {
        match character {
            '(' | '[' | '{' | '<' => nesting += 1,
            ')' | ']' | '}' | '>' => nesting = (nesting - 1).max(0),
            ',' if nesting == 0 => {
                canonical_parameters.push(cpp_parameter_without_name(&parameters[start..index]));
                start = index + 1;
            }
            _ => {}
        }
    }
    canonical_parameters.push(cpp_parameter_without_name(&parameters[start..]));
    if canonical_parameters.len() == 1 && canonical_parameters[0] == "void" {
        canonical_parameters.clear();
    }
    let suffix = expanded[close + 1..]
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    format!("({}){suffix}", canonical_parameters.join(","))
}

/// Return a stable representative for every syntax symbol that the admitted
/// compiler graph (or one of this batch's prepared LSP closures) proves is the
/// same semantic target. Header declarations and implementation definitions
/// intentionally have different parser IDs; treating those raw IDs as owners
/// produces false alias collisions during bulk approval.
fn semantic_alias_representatives(
    root: &Path,
    conn: &rusqlite::Connection,
    prepared: &[Option<PreparedCompilerProposalResolution>],
) -> Result<BTreeMap<String, String>> {
    semantic_alias_representatives_with_base(root, conn, prepared, None)
}

fn semantic_alias_representatives_with_base(
    root: &Path,
    conn: &rusqlite::Connection,
    prepared: &[Option<PreparedCompilerProposalResolution>],
    base: Option<&BTreeMap<String, String>>,
) -> Result<BTreeMap<String, String>> {
    let mut graph = BTreeMap::<String, BTreeSet<String>>::new();
    if let Some(base) = base {
        for (symbol_id, representative) in base {
            graph.entry(symbol_id.clone()).or_default();
            graph.entry(representative.clone()).or_default();
            if symbol_id != representative {
                connect_semantic_alias_targets(&mut graph, symbol_id, representative);
            }
        }
    }
    {
        let mut statement = conn
            .prepare("select canonical_symbol_id, linked_symbol_id from semantic_compiler_links")
            .context("prepare compiler alias-equivalence query")?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .context("query compiler alias-equivalence links")?;
        for row in rows {
            let (canonical, linked) = row.context("read compiler alias-equivalence link")?;
            connect_semantic_alias_targets(&mut graph, &canonical, &linked);
        }
    }

    // Resolve every prepared location in one indexed-table pass. The former
    // query-per-location path performed thousands of SQLite scans on large
    // C++ batches and made semantic canonicalization dominate approval time.
    let needed_occurrences = prepared
        .iter()
        .flatten()
        .flat_map(|resolution| {
            resolution.locations.iter().map(|location| {
                (
                    location.rel_path.clone(),
                    location.range.start_byte,
                    location.range.end_byte,
                )
            })
        })
        .collect::<BTreeSet<_>>();
    let mut occurrence_members = BTreeMap::<(String, usize, usize), BTreeSet<String>>::new();
    if !needed_occurrences.is_empty() {
        let mut statement = conn
            .prepare(
                r#"
                select rel_path, start_byte, end_byte, symbol_id
                from semantic_occurrences where symbol_id is not null
                "#,
            )
            .context("prepare pending compiler alias-equivalence scan")?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)? as usize,
                    row.get::<_, i64>(2)? as usize,
                    row.get::<_, String>(3)?,
                ))
            })
            .context("scan pending compiler alias-equivalence members")?;
        for row in rows {
            let (rel_path, start_byte, end_byte, symbol_id) =
                row.context("read pending compiler alias-equivalence member")?;
            let key = (rel_path, start_byte, end_byte);
            if needed_occurrences.contains(&key) {
                occurrence_members.entry(key).or_default().insert(symbol_id);
            }
        }
    }
    for resolution in prepared.iter().flatten() {
        graph.entry(resolution.symbol_id.clone()).or_default();
        for location in &resolution.locations {
            let key = (
                location.rel_path.clone(),
                location.range.start_byte,
                location.range.end_byte,
            );
            for member in occurrence_members.get(&key).into_iter().flatten() {
                connect_semantic_alias_targets(&mut graph, &resolution.symbol_id, member);
            }
        }
    }

    if base.is_none() {
        // Declarator fallback is needed only for components that can affect this
        // decision: already persisted aliases/compiler links or targets in this
        // prepared batch. The former all-workspace recursive CTE walked every C++
        // symbol and ancestor on every approval, which dominated large projects.
        let mut relevant_ids = graph.keys().cloned().collect::<BTreeSet<_>>();
        {
            let mut statement = conn
                .prepare(
                    "select symbol_id from semantic_aliases where status in ('accepted', 'stale')",
                )
                .context("prepare persisted semantic alias targets")?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .context("query persisted semantic alias targets")?;
            for row in rows {
                relevant_ids.insert(row.context("read persisted semantic alias target")?);
            }
        }
        if relevant_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let relevant_json = serde_json::to_string(&relevant_ids)
            .context("serialize relevant semantic alias targets")?;

        // clangd may legitimately return only the opened declaration when its
        // compilation database is unavailable or incomplete. For C++/ObjC++, an
        // exact header/implementation declarator tail is still deterministic
        // declaration-definition evidence: same qualified name, symbol kind and
        // parameter/qualifier spelling. Restrict the fallback to a header paired
        // with an implementation file and exclude internal-linkage declarations.
        #[derive(Debug)]
        struct SyntaxCandidate {
            symbol_id: String,
            rel_path: String,
            kind: String,
            qualified_name: String,
            language: String,
            symbol_start: usize,
            symbol_end: usize,
            declarator_end: Option<(usize, usize)>,
            outer_start: Option<(usize, usize)>,
            namespace_starts: Vec<usize>,
        }
        let mut candidates = BTreeMap::<String, SyntaxCandidate>::new();
        {
            let mut statement = conn
                .prepare(
                    r#"
                with recursive relevant_shapes(kind, qualified_name, language_family) as (
                    select distinct symbol.kind, symbol.qualified_name,
                           case when document.language = 'objective-cpp'
                                then 'cpp' else document.language end
                    from semantic_symbols symbol
                    join semantic_documents document on document.rel_path = symbol.rel_path
                    where symbol.symbol_id in (select value from json_each(?1))
                      and symbol.origin = 'owned'
                      and document.language in ('cpp', 'objective-cpp')
                ), candidate_symbols(
                    symbol_id, rel_path, kind, qualified_name, language,
                    symbol_start, symbol_end, node_id, parent_node_id
                ) as (
                    select symbol.symbol_id, symbol.rel_path, symbol.kind,
                           symbol.qualified_name, document.language,
                           node.start_byte, node.end_byte, node.node_id,
                           node.parent_node_id
                    from semantic_symbols symbol
                    join semantic_nodes node on node.node_id = symbol.node_id
                    join semantic_documents document on document.rel_path = symbol.rel_path
                    join relevant_shapes shape
                      on shape.kind = symbol.kind
                     and shape.qualified_name = symbol.qualified_name
                     and shape.language_family = case
                           when document.language = 'objective-cpp'
                           then 'cpp' else document.language end
                    where symbol.origin = 'owned'
                      and document.language in ('cpp', 'objective-cpp')
                ), ancestors(
                    symbol_id, rel_path, kind, qualified_name, language,
                    symbol_start, symbol_end, node_id, parent_node_id, depth
                ) as (
                    select symbol_id, rel_path, kind, qualified_name, language,
                           symbol_start, symbol_end, node_id, parent_node_id, 0
                    from candidate_symbols
                    union all
                    select ancestor.symbol_id, ancestor.rel_path, ancestor.kind,
                           ancestor.qualified_name, ancestor.language,
                           ancestor.symbol_start, ancestor.symbol_end,
                           parent.node_id, parent.parent_node_id, ancestor.depth + 1
                    from ancestors ancestor
                    join semantic_nodes parent on parent.node_id = ancestor.parent_node_id
                    where ancestor.depth < 12
                )
                select ancestor.symbol_id, ancestor.rel_path, ancestor.kind,
                       ancestor.qualified_name, ancestor.language,
                       ancestor.symbol_start, ancestor.symbol_end,
                       node.kind, node.start_byte, node.end_byte, ancestor.depth
                from ancestors ancestor
                join semantic_nodes node on node.node_id = ancestor.node_id
                where ancestor.depth > 0
                  and node.kind in (
                      'function_declarator', 'declaration', 'function_definition',
                      'namespace_definition'
                  )
                order by ancestor.symbol_id, ancestor.depth
                "#,
                )
                .context("prepare C++ declaration-equivalence query")?;
            let rows = statement
                .query_map([relevant_json], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)? as usize,
                        row.get::<_, i64>(6)? as usize,
                        row.get::<_, String>(7)?,
                        row.get::<_, i64>(8)? as usize,
                        row.get::<_, i64>(9)? as usize,
                        row.get::<_, i64>(10)? as usize,
                    ))
                })
                .context("query C++ declaration-equivalence candidates")?;
            for row in rows {
                let (
                    symbol_id,
                    rel_path,
                    kind,
                    qualified_name,
                    language,
                    symbol_start,
                    symbol_end,
                    node_kind,
                    node_start,
                    node_end,
                    depth,
                ) = row.context("read C++ declaration-equivalence candidate")?;
                let candidate = candidates
                    .entry(symbol_id.clone())
                    .or_insert(SyntaxCandidate {
                        symbol_id,
                        rel_path,
                        kind,
                        qualified_name,
                        language,
                        symbol_start,
                        symbol_end,
                        declarator_end: None,
                        outer_start: None,
                        namespace_starts: Vec::new(),
                    });
                if node_kind == "function_declarator"
                    && candidate
                        .declarator_end
                        .is_none_or(|(existing_depth, _)| depth < existing_depth)
                {
                    candidate.declarator_end = Some((depth, node_end));
                }
                if matches!(node_kind.as_str(), "declaration" | "function_definition")
                    && candidate
                        .outer_start
                        .is_none_or(|(existing_depth, _)| depth < existing_depth)
                {
                    candidate.outer_start = Some((depth, node_start));
                }
                if node_kind == "namespace_definition" {
                    candidate.namespace_starts.push(node_start);
                }
            }
        }
        let mut source_cache = BTreeMap::<String, String>::new();
        let mut signature_source_cache = BTreeMap::<String, String>::new();
        let mut namespace_alias_cache = BTreeMap::<String, BTreeMap<String, String>>::new();
        let mut groups = BTreeMap::<String, Vec<(String, bool, bool)>>::new();
        for candidate in candidates.into_values() {
            let Some((_, declarator_end)) = candidate.declarator_end else {
                continue;
            };
            if candidate.qualified_name.is_empty() || candidate.symbol_end > declarator_end {
                continue;
            }
            let source = match source_cache.entry(candidate.rel_path.clone()) {
                std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    let rel = crate::config::normalize_safe_rel_path(
                        Path::new(&candidate.rel_path),
                        "C++ declaration equivalence",
                    )?;
                    entry.insert(std::fs::read_to_string(root.join(rel)).with_context(|| {
                        format!(
                            "read C++ declaration-equivalence source {}",
                            candidate.rel_path
                        )
                    })?)
                }
            };
            let signature_source = signature_source_cache
                .entry(candidate.rel_path.clone())
                .or_insert_with(|| mask_cpp_signature_source(source));
            let namespace_aliases = namespace_alias_cache
                .entry(candidate.rel_path.clone())
                .or_insert_with(|| cpp_namespace_aliases(signature_source));
            let signature = signature_source
                .get(candidate.symbol_end..declarator_end)
                .ok_or_else(|| {
                    anyhow!(
                        "indexed C++ declarator range is stale in {}",
                        candidate.rel_path
                    )
                })?;
            let signature = canonical_cpp_declarator_tail(signature, namespace_aliases);
            let outer_start = candidate
                .outer_start
                .map(|(_, start)| start)
                .unwrap_or(candidate.symbol_start);
            let prefix = source
                .get(outer_start.min(candidate.symbol_start)..candidate.symbol_start)
                .unwrap_or_default();
            if contains_whole_word(prefix, "static") {
                continue;
            }
            let anonymous_namespace = candidate.namespace_starts.iter().any(|start| {
                source
                    .get(*start..candidate.symbol_start)
                    .and_then(|namespace| namespace.split_once('{').map(|(header, _)| header))
                    .is_some_and(|header| {
                        let tokens = identifier_tokens(header);
                        tokens.len() == 1 && tokens.contains("namespace")
                    })
            });
            if anonymous_namespace {
                continue;
            }
            let extension = Path::new(&candidate.rel_path)
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            let is_header = matches!(extension.as_str(), "h" | "hh" | "hpp" | "hxx");
            let is_implementation =
                matches!(extension.as_str(), "c" | "cc" | "cpp" | "cxx" | "m" | "mm");
            if !is_header && !is_implementation {
                continue;
            }
            let language_family = if candidate.language == "objective-cpp" {
                "cpp"
            } else {
                candidate.language.as_str()
            };
            let key = format!(
                "{language_family}\0{}\0{}\0{signature}",
                candidate.kind, candidate.qualified_name
            );
            groups.entry(key).or_default().push((
                candidate.symbol_id,
                is_header,
                is_implementation,
            ));
        }
        for members in groups.into_values() {
            if !members.iter().any(|(_, header, _)| *header)
                || !members.iter().any(|(_, _, implementation)| *implementation)
            {
                continue;
            }
            let owner = &members[0].0;
            for (member, _, _) in &members[1..] {
                connect_semantic_alias_targets(&mut graph, owner, member);
            }
        }
    }

    let mut remaining = graph.keys().cloned().collect::<BTreeSet<_>>();
    let mut representatives = BTreeMap::new();
    while let Some(start) = remaining.iter().next().cloned() {
        let mut component = BTreeSet::new();
        let mut frontier = vec![start];
        while let Some(symbol_id) = frontier.pop() {
            if !component.insert(symbol_id.clone()) {
                continue;
            }
            remaining.remove(&symbol_id);
            frontier.extend(
                graph
                    .get(&symbol_id)
                    .into_iter()
                    .flatten()
                    .filter(|neighbor| !component.contains(*neighbor))
                    .cloned(),
            );
        }
        let representative = component
            .iter()
            .next()
            .cloned()
            .expect("semantic alias component is non-empty");
        for symbol_id in component {
            representatives.insert(symbol_id, representative.clone());
        }
    }
    Ok(representatives)
}

/// Repair aliases created before declaration/definition canonicalization was
/// enforced. A C++ header is the authoritative contract; when an equivalent
/// implementation anchor has a different accepted spelling, converge the
/// whole semantic component onto the header alias. Components containing a
/// stale alias or an alias owned outside the component are left untouched for
/// explicit recovery rather than guessed.
pub(crate) fn reconcile_equivalent_semantic_aliases(
    root: &Path,
    conn: &mut rusqlite::Connection,
) -> Result<usize> {
    #[derive(Debug, Clone)]
    struct AliasState {
        sanitized_name: String,
        category: String,
        confidence: Option<f64>,
        reason: Option<String>,
        status: String,
        created_revision: u64,
    }

    #[derive(Debug)]
    struct Member {
        symbol_id: String,
        rel_path: String,
        name: String,
        kind: String,
        qualified_name: String,
        alias: Option<AliasState>,
    }

    let representatives = semantic_alias_representatives(root, conn, &[])?;
    if representatives.is_empty() {
        return Ok(0);
    }
    let mut components = BTreeMap::<String, Vec<Member>>::new();
    let mut accepted_alias_owners = BTreeMap::<String, BTreeSet<String>>::new();
    {
        let mut statement = conn
            .prepare(
                r#"
                select symbol.symbol_id, symbol.rel_path, symbol.name, symbol.kind,
                       symbol.qualified_name, alias.sanitized_name, alias.category,
                       alias.confidence, alias.reason, alias.status, alias.created_revision
                from semantic_symbols symbol
                left join semantic_aliases alias on alias.symbol_id = symbol.symbol_id
                order by symbol.symbol_id
                "#,
            )
            .context("prepare equivalent semantic alias reconciliation")?;
        let rows = statement
            .query_map([], |row| {
                let sanitized_name = row.get::<_, Option<String>>(5)?;
                let category = row
                    .get::<_, Option<String>>(6)?
                    .unwrap_or_else(|| "identifier".into());
                let confidence = row.get(7)?;
                let reason = row.get(8)?;
                let status = row.get::<_, Option<String>>(9)?.unwrap_or_default();
                let created_revision = row.get::<_, Option<i64>>(10)?.unwrap_or_default() as u64;
                let alias = sanitized_name.map(|sanitized_name| AliasState {
                    sanitized_name,
                    category,
                    confidence,
                    reason,
                    status,
                    created_revision,
                });
                Ok(Member {
                    symbol_id: row.get(0)?,
                    rel_path: row.get(1)?,
                    name: row.get(2)?,
                    kind: row.get(3)?,
                    qualified_name: row.get(4)?,
                    alias,
                })
            })
            .context("query equivalent semantic alias reconciliation")?;
        for row in rows {
            let member = row.context("read equivalent semantic alias member")?;
            if let Some(alias) = member
                .alias
                .as_ref()
                .filter(|alias| alias.status == "accepted")
            {
                accepted_alias_owners
                    .entry(normalize_term(&alias.sanitized_name))
                    .or_default()
                    .insert(member.symbol_id.clone());
            }
            let Some(representative) = representatives.get(&member.symbol_id) else {
                continue;
            };
            components
                .entry(representative.clone())
                .or_default()
                .push(member);
        }
    }

    struct Repair {
        symbol_id: String,
        original_name: String,
        alias: AliasState,
    }
    let mut repairs = Vec::<Repair>::new();
    let mut link_repairs = BTreeSet::<(String, String)>::new();
    for members in components.into_values() {
        if members.len() < 2
            || members.iter().any(|member| {
                member
                    .alias
                    .as_ref()
                    .is_some_and(|alias| alias.status == "stale")
            })
        {
            continue;
        }
        let Some(first) = members.first() else {
            continue;
        };
        if members.iter().any(|member| {
            member.name != first.name
                || member.kind != first.kind
                || member.qualified_name != first.qualified_name
        }) {
            log::warn!(
                "ignored malformed compiler alias component rooted at {}",
                first.symbol_id
            );
            continue;
        }
        let mut accepted = members
            .iter()
            .filter_map(|member| {
                member
                    .alias
                    .as_ref()
                    .filter(|alias| alias.status == "accepted")
                    .map(|alias| (member, alias))
            })
            .collect::<Vec<_>>();
        if accepted.is_empty() {
            continue;
        }
        let mut all_complete = true;
        let mut active_canonical = None;
        for member in &members {
            all_complete &=
                crate::semantic_store::symbol_projection_is_complete(conn, &member.symbol_id)?;
            if active_canonical.is_none() {
                active_canonical =
                    crate::semantic_store::active_compiler_canonical(conn, &member.symbol_id)?;
            }
        }
        if !all_complete {
            let Some(active_canonical) = active_canonical else {
                log::warn!(
                    "cannot reconcile alias anchors for {}: no complete compiler closure",
                    first.qualified_name
                );
                continue;
            };
            for member in &members {
                link_repairs.insert((active_canonical.clone(), member.symbol_id.clone()));
            }
        }
        accepted.sort_by_key(|(member, alias)| {
            let extension = Path::new(&member.rel_path)
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            let header_rank =
                usize::from(!matches!(extension.as_str(), "h" | "hh" | "hpp" | "hxx"));
            (
                header_rank,
                alias.created_revision,
                member.symbol_id.as_str(),
            )
        });
        let canonical = accepted[0].1.clone();
        let member_ids = members
            .iter()
            .map(|member| member.symbol_id.as_str())
            .collect::<BTreeSet<_>>();
        if accepted_alias_owners
            .get(&normalize_term(&canonical.sanitized_name))
            .is_some_and(|owners| {
                owners
                    .iter()
                    .any(|owner| !member_ids.contains(owner.as_str()))
            })
        {
            log::warn!(
                "cannot reconcile alias {:?} for {}: it is owned outside the semantic component",
                canonical.sanitized_name,
                first.qualified_name
            );
            continue;
        }
        for member in members {
            let already_canonical = member.alias.as_ref().is_some_and(|alias| {
                alias.status == "accepted" && alias.sanitized_name == canonical.sanitized_name
            });
            if already_canonical {
                continue;
            }
            repairs.push(Repair {
                symbol_id: member.symbol_id,
                original_name: member.name,
                alias: canonical.clone(),
            });
        }
    }
    if repairs.is_empty() && link_repairs.is_empty() {
        return Ok(0);
    }

    let tx = conn
        .transaction()
        .context("begin equivalent semantic alias reconciliation")?;
    let base_revision = crate::semantic_store::current_revision(&tx)?;
    let next_revision = base_revision
        .checked_add(1)
        .ok_or_else(|| anyhow!("semantic workspace revision overflow"))?;
    let mut linked = 0usize;
    for (canonical_symbol_id, linked_symbol_id) in &link_repairs {
        linked += tx
            .execute(
                r#"
                insert or ignore into semantic_compiler_links(
                  canonical_symbol_id, linked_symbol_id
                ) values(?1, ?2)
                "#,
                rusqlite::params![canonical_symbol_id, linked_symbol_id],
            )
            .context("reconcile deterministic compiler-equivalent symbol link")?;
    }
    for repair in &repairs {
        let reason = repair
            .alias
            .reason
            .as_deref()
            .map(|reason| format!("{reason}; reconciled across equivalent declarations"))
            .unwrap_or_else(|| "reconciled across equivalent declarations".to_string());
        tx.execute(
            r#"
            insert into semantic_aliases(
                symbol_id, original_name, sanitized_name, category, confidence,
                reason, status, source, created_revision
            ) values(?1, ?2, ?3, ?4, ?5, ?6, 'accepted', 'proposal-v2', ?7)
            on conflict(symbol_id) do update set
                original_name = excluded.original_name,
                sanitized_name = excluded.sanitized_name,
                category = excluded.category,
                confidence = excluded.confidence,
                reason = excluded.reason,
                status = excluded.status,
                source = excluded.source,
                created_revision = excluded.created_revision
            "#,
            rusqlite::params![
                repair.symbol_id,
                repair.original_name,
                repair.alias.sanitized_name,
                repair.alias.category,
                repair.alias.confidence,
                reason,
                next_revision as i64,
            ],
        )
        .context("reconcile equivalent semantic alias")?;
    }
    let updated = tx
        .execute(
            "update semantic_workspace set revision = ?2 where singleton = 1 and revision = ?1",
            rusqlite::params![base_revision as i64, next_revision as i64],
        )
        .context("advance equivalent semantic alias reconciliation revision")?;
    if updated != 1 {
        bail!("semantic workspace changed during alias reconciliation");
    }
    tx.commit()
        .context("commit equivalent semantic alias reconciliation")?;
    log::warn!(
        "reconciled {} declaration/definition alias anchor(s) and {} compiler link(s)",
        repairs.len(),
        linked
    );
    Ok(repairs.len() + linked)
}

fn approval_alias_identity(
    proposal: &Proposal,
    representatives: &BTreeMap<String, String>,
) -> String {
    match proposal.target.as_ref() {
        Some(ProposalTarget::Semantic(target)) => format!(
            "semantic:{}",
            representatives
                .get(&target.symbol_id)
                .unwrap_or(&target.symbol_id)
        ),
        Some(ProposalTarget::FilePath(_)) => format!("path:{}", proposal_identity(proposal)),
        None => format!("legacy:{}", proposal_identity(proposal)),
    }
}

type SelectedAliasAssignments = BTreeMap<String, Vec<(String, Option<String>, String)>>;
type SelectedTargetAssignments = BTreeMap<String, Vec<(String, String)>>;

fn register_selected_alias(
    selected_aliases: &mut SelectedAliasAssignments,
    selected_targets: &mut SelectedTargetAssignments,
    alias: &str,
    identity: &str,
    semantic_original: Option<&str>,
    description: &str,
) -> Result<()> {
    let alias_key = normalize_term(alias);
    let semantic_original = semantic_original.map(normalize_term);
    if let Some(assignments) = selected_aliases.get(&alias_key) {
        if let Some((_, _, existing)) = assignments.iter().find(|(owner, existing_original, _)| {
            owner != identity
                && !matches!(
                    (existing_original.as_ref(), semantic_original.as_ref()),
                    (Some(existing), Some(incoming)) if existing == incoming
                )
        }) {
            bail!(
                "selected proposals reuse alias {alias:?} for different targets: {existing}; {description}"
            );
        }
    }
    if let Some(assignments) = selected_targets.get(identity) {
        if let Some((_, existing)) = assignments
            .iter()
            .find(|(existing_alias, _)| existing_alias != alias)
        {
            bail!("selected target has incompatible aliases: {existing}; {description}");
        }
    }
    selected_aliases.entry(alias_key).or_default().push((
        identity.to_string(),
        semantic_original,
        description.to_string(),
    ));
    selected_targets
        .entry(identity.to_string())
        .or_default()
        .push((alias.to_string(), description.to_string()));
    Ok(())
}

/// Approve or reject a queued proposal. Approving records the alias in the config
/// registry (deterministic) and reindexes the affected file so the deterministic
/// engine applies it; rejecting just marks the item.
pub fn resolve_review(root: &Path, id: &str, approve: bool) -> Result<ReviewItem> {
    let mut compiler_resolution = if approve {
        prepare_compiler_proposal_resolution(root, id)
            .with_context(|| format!("prepare semantic closure for review {id}"))?
    } else {
        None
    };
    if compiler_resolution.is_some() {
        populate_prepared_alias_equivalents(root, std::slice::from_mut(&mut compiler_resolution))?;
    }
    resolve_review_prepared(root, id, approve, compiler_resolution)
}

/// Two-phase batch approval. Every deterministic/config/LSP precondition is
/// resolved before the first decision is persisted, preventing the historical
/// "178 approved, 3 failed" validation split. The apply phase rechecks source
/// hashes and the document fingerprint for race safety.
pub fn approve_reviews(root: &Path, ids: &[String]) -> Result<Vec<ReviewItem>> {
    approve_reviews_with_progress(root, ids, |_| {})
}

pub fn approve_reviews_with_progress(
    root: &Path,
    ids: &[String],
    mut progress: impl FnMut(ApprovalProgress),
) -> Result<Vec<ReviewItem>> {
    if ids.is_empty() {
        bail!("no proposals selected");
    }
    let layout = Layout::new(root);
    layout.require_initialized()?;
    progress(ApprovalProgress {
        detail: "reconciling obsolete selected proposals".to_string(),
        completed: 0,
        total: ids.len(),
    });
    {
        let _lock = WorkspaceLock::acquire(&layout)?;
        crate::journal::ensure_no_interrupted_apply(&layout)?;
        let pending_mirrors = crate::index::pending_mirror_edit_count(&layout)?;
        if pending_mirrors != 0 {
            bail!(
                "cannot approve while {pending_mirrors} sanitized mirror file(s) contain pending agent edits; project or discard those edits first"
            );
        }
        if !crate::index::persisted_workspace_is_current(root, &layout)? {
            progress(ApprovalProgress {
                detail: "refreshing stale index before approval".to_string(),
                completed: 0,
                total: 1,
            });
            let report = crate::index::index_workspace_locked(root, &layout)
                .context("refresh stale index before bulk approval")?;
            if report.pending != 0 {
                bail!(
                    "cannot approve while {} sanitized mirror file(s) contain pending agent edits; project or discard those edits first",
                    report.pending
                );
            }
            if !report.errors.is_empty() || !report.semantic.errors.is_empty() {
                bail!(
                    "index refresh before approval reported {} file error(s) and {} semantic error(s)",
                    report.errors.len(),
                    report.semantic.errors.len()
                );
            }
            progress(ApprovalProgress {
                detail: if report.semantic.quarantined_aliases == 0 {
                    "refreshed stale index before approval".to_string()
                } else {
                    format!(
                        "refreshed stale index; quarantined {} unsafe legacy aliases",
                        report.semantic.quarantined_aliases
                    )
                },
                completed: 1,
                total: 1,
            });
        }
        let quarantined = {
            let mut conn = db::connect(&layout)?;
            db::check_schema(&conn)?;
            let mut quarantined =
                crate::semantic_store::quarantine_unrestored_stale_aliases(&mut conn)?;
            quarantined.extend(
                crate::semantic_store::quarantine_legacy_invalid_accepted_aliases(&mut conn)?,
            );
            quarantined
        };
        if !quarantined.is_empty() {
            forget_quarantined_alias_decisions(&layout, &quarantined)?;
            let changed_symbols = quarantined
                .iter()
                .map(|alias| alias.symbol_id.clone())
                .collect::<BTreeSet<_>>();
            crate::index::refresh_semantic_mirrors_for_symbols_locked(
                root,
                &layout,
                &changed_symbols,
            )
            .context("remove quarantined legacy aliases from mirrors")?;
            progress(ApprovalProgress {
                detail: format!(
                    "quarantined {} unsafe aliases accepted by older releases",
                    quarantined.len()
                ),
                completed: quarantined.len(),
                total: quarantined.len(),
            });
        }
        reconcile_review_queue_locked(root, &layout)?;
        retire_selected_invalid_proposals(root, &layout, ids)?;
        retire_selected_alias_collisions(&layout, ids)?;
    }
    let mut active_ids = Vec::with_capacity(ids.len());
    let mut retired = 0usize;
    for id in ids {
        let path = layout.review_dir.join(format!("{id}.json"));
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("review item {id} not found ({})", path.display()))?;
        let item: ReviewItem =
            serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        match item.status {
            ReviewStatus::Pending => active_ids.push(id.clone()),
            ReviewStatus::Stale => retired += 1,
            ReviewStatus::Approved | ReviewStatus::Rejected => {
                bail!("review item {id} is already {:?}", item.status)
            }
        }
    }
    if retired != 0 {
        progress(ApprovalProgress {
            detail: format!("retired {retired} selected proposals during preflight"),
            completed: retired,
            total: ids.len(),
        });
    }
    if active_ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids = active_ids.as_slice();
    progress(ApprovalProgress {
        detail: "checking existing alias ownership".to_string(),
        completed: 0,
        total: ids.len(),
    });
    let preflight_representatives = preflight_existing_target_aliases(root, ids)?;
    // LSP resolution is intentionally outside the workspace lock. Besides
    // preparing the apply phase, these closures let preflight canonicalize a
    // declaration in a header and its definition in an implementation file.
    let prepared = prepare_compiler_proposal_resolutions(
        root,
        ids,
        &preflight_representatives,
        &mut progress,
    )?;
    progress(ApprovalProgress {
        detail: "checking workspace-wide collisions".to_string(),
        completed: 0,
        total: ids.len(),
    });
    let (layout, _lock) = crate::index::init_workspace_locked(root)?;
    crate::journal::ensure_no_interrupted_apply(&layout)?;
    let mut conn = db::connect(&layout)?;
    db::check_schema(&conn)?;
    let mut candidate_config = Config::load_or_default(&layout)?;
    let tracked = db::tracked_files(&conn)?;
    let projection =
        crate::path_projection::PathProjection::from_connection(&candidate_config, &conn)?;
    let current_fingerprint = crate::semantic_store::document_fingerprint(&conn)?;
    let CompilerQuarantineResult {
        ids: admissible_ids,
        prepared: admissible_prepared,
        mut representatives,
        retired: closure_retired,
    } = quarantine_unadmissible_compiler_resolutions(
        CompilerQuarantineContext {
            root,
            layout: &layout,
            conn: &conn,
            base_representatives: &preflight_representatives,
            current_fingerprint: &current_fingerprint,
        },
        ids.to_vec(),
        prepared,
        &mut progress,
    )?;
    if admissible_ids.is_empty() {
        return Ok(Vec::new());
    }
    let compiler_retired =
        retire_selected_target_alternatives(&layout, &conn, &admissible_ids, &representatives)?;
    let mut remaining_ids = Vec::with_capacity(admissible_ids.len() - compiler_retired.len());
    let mut remaining_prepared = Vec::with_capacity(remaining_ids.capacity());
    for (id, resolution) in admissible_ids.into_iter().zip(admissible_prepared) {
        if !compiler_retired.contains(&id) {
            remaining_ids.push(id);
            remaining_prepared.push(resolution);
        }
    }
    if !compiler_retired.is_empty() {
        progress(ApprovalProgress {
            detail: format!(
                "retired {} compiler-equivalent alternatives",
                compiler_retired.len()
            ),
            completed: compiler_retired.len(),
            total: remaining_ids.len() + compiler_retired.len(),
        });
    }
    if remaining_ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids = remaining_ids.as_slice();
    let mut prepared = remaining_prepared;
    if !compiler_retired.is_empty() {
        representatives = semantic_alias_representatives_with_base(
            root,
            &conn,
            &prepared,
            Some(&preflight_representatives),
        )?;
    }
    populate_alias_equivalents_from_representatives(&mut prepared, &representatives);
    if closure_retired != 0 {
        progress(ApprovalProgress {
            detail: format!("continuing with {} admissible proposals", ids.len()),
            completed: ids.len(),
            total: ids.len(),
        });
    }

    let mut selected_aliases = SelectedAliasAssignments::new();
    let mut selected_targets = SelectedTargetAssignments::new();
    for pair in crate::semantic_store::accepted_alias_bindings(&conn)? {
        let alias = crate::sanitize::normalize_term(&pair.alias);
        let exact_alias = pair.alias.clone();
        let target = format!(
            "semantic:{}",
            representatives
                .get(&pair.symbol_id)
                .unwrap_or(&pair.symbol_id)
        );
        let description = format!(
            "accepted semantic target {} ({:?} -> {:?})",
            pair.symbol_id, pair.original, pair.alias
        );
        selected_aliases.entry(alias).or_default().push((
            target.clone(),
            Some(crate::sanitize::normalize_term(&pair.original)),
            description.clone(),
        ));
        selected_targets
            .entry(target)
            .or_default()
            .push((exact_alias, description));
    }
    let mut natural_symbol_owners = BTreeMap::<String, BTreeSet<String>>::new();
    {
        let mut statement = conn
            .prepare("select symbol_id, name from semantic_symbols")
            .context("prepare batch alias collision preflight")?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .context("query batch alias collision preflight")?;
        for row in rows {
            let (symbol_id, name) = row.context("read batch alias collision owner")?;
            natural_symbol_owners
                .entry(crate::sanitize::normalize_term(&name))
                .or_default()
                .insert(symbol_id);
        }
    }

    let mut items = Vec::with_capacity(ids.len());
    let mut source_cache = BTreeMap::<String, Arc<str>>::new();
    let mut protected_cache = BTreeMap::<String, BTreeSet<String>>::new();
    let mut collision_terms = Vec::<crate::sanitize::Term>::new();
    let mut semantic_aliases =
        BTreeMap::<String, crate::semantic_store::SymbolAliasAcceptance>::new();
    let mut path_config_changed = false;
    let mut content_config_changed = false;
    for id in ids {
        let path = layout.review_dir.join(format!("{id}.json"));
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("review item {id} not found"))?;
        let item: ReviewItem =
            serde_json::from_str(&raw).with_context(|| format!("parse review item {id}"))?;
        if item.status != ReviewStatus::Pending {
            bail!("review item {id} is already {:?}", item.status);
        }
        let alias_key = crate::sanitize::normalize_term(&item.proposal.sanitized_text);
        let identity = approval_alias_identity(&item.proposal, &representatives);
        let description = format!(
            "{id} ({}: {:?} -> {:?})",
            item.file, item.proposal.original_text, item.proposal.sanitized_text
        );
        register_selected_alias(
            &mut selected_aliases,
            &mut selected_targets,
            &item.proposal.sanitized_text,
            &identity,
            Some(item.proposal.original_text.as_str()),
            &description,
        )?;
        match item.proposal.target.as_ref() {
            Some(ProposalTarget::FilePath(_)) => {
                let candidates = file_path_candidates(&item.file, &projection)?;
                validate_file_path_proposal(
                    &item.proposal,
                    &candidates,
                    &candidate_config,
                    &tracked,
                )
                .map_err(|reason| anyhow!("{id}: {reason}"))?;
                candidate_config.sanitizer.path_alias_registry.insert(
                    item.proposal.original_text.clone(),
                    item.proposal.sanitized_text.clone(),
                );
                path_config_changed = true;
            }
            Some(ProposalTarget::Semantic(target)) => {
                let real = match source_cache.get(&item.file) {
                    Some(real) => Arc::clone(real),
                    None => {
                        let real: Arc<str> = Arc::from(
                            std::fs::read_to_string(root.join(&item.file))
                                .with_context(|| format!("read {}", item.file))?,
                        );
                        source_cache.insert(item.file.clone(), Arc::clone(&real));
                        real
                    }
                };
                let protected = protected_cache
                    .entry(item.file.clone())
                    .or_insert_with(|| collect_protected_identifiers(Path::new(&item.file), &real));
                validate_proposal_with_protected(
                    Path::new(&item.file),
                    &item.proposal,
                    &real,
                    &candidate_config,
                    protected,
                )
                .map_err(|reason| anyhow!("{id}: {reason}"))?;
                let (target_file, symbol) =
                    crate::semantic_store::load_symbol_with_path(&conn, &target.symbol_id)?
                        .ok_or_else(|| anyhow!("{id}: proposal target disappeared"))?;
                if target_file != item.file || symbol.name != item.proposal.original_text {
                    bail!("{id}: proposal target no longer matches source");
                }
                let occurrence_matches =
                    crate::semantic_store::occurrences_for_symbol(&conn, &target.symbol_id)?
                        .iter()
                        .any(|(_, occurrence)| occurrence.occurrence_id == target.occurrence_id);
                if !occurrence_matches {
                    bail!("{id}: proposal target occurrence no longer exists");
                }
                if natural_symbol_owners.get(&alias_key).is_some_and(|owners| {
                    owners.iter().any(|owner| {
                        representatives.get(owner).unwrap_or(owner)
                            != representatives
                                .get(&target.symbol_id)
                                .unwrap_or(&target.symbol_id)
                    })
                }) {
                    bail!(
                        "{id}: semantic alias {:?} collides with an existing symbol name",
                        item.proposal.sanitized_text
                    );
                }
                semantic_aliases.entry(identity.clone()).or_insert_with(|| {
                    crate::semantic_store::SymbolAliasAcceptance {
                        symbol_id: target.symbol_id.clone(),
                        replacement: item.proposal.sanitized_text.clone(),
                        category: item.proposal.category.clone(),
                        confidence: item.proposal.confidence,
                        reason: item.proposal.rationale.clone(),
                    }
                });
                collision_terms.push(crate::sanitize::Term {
                    raw: item.proposal.original_text.clone(),
                    normalized: crate::sanitize::normalize_term(&item.proposal.original_text),
                    replacement: item.proposal.sanitized_text.clone(),
                    policy_source: "proposal-v2",
                });
            }
            None => {
                let real = match source_cache.get(&item.file) {
                    Some(real) => Arc::clone(real),
                    None => {
                        let real: Arc<str> = Arc::from(
                            std::fs::read_to_string(root.join(&item.file))
                                .with_context(|| format!("read {}", item.file))?,
                        );
                        source_cache.insert(item.file.clone(), Arc::clone(&real));
                        real
                    }
                };
                let protected = protected_cache
                    .entry(item.file.clone())
                    .or_insert_with(|| collect_protected_identifiers(Path::new(&item.file), &real));
                validate_proposal_with_protected(
                    Path::new(&item.file),
                    &item.proposal,
                    &real,
                    &candidate_config,
                    protected,
                )
                .map_err(|reason| anyhow!("{id}: {reason}"))?;
                candidate_config.sanitizer.alias_registry.insert(
                    item.proposal.original_text.clone(),
                    item.proposal.sanitized_text.clone(),
                );
                collision_terms.push(crate::sanitize::Term {
                    raw: item.proposal.original_text.clone(),
                    normalized: crate::sanitize::normalize_term(&item.proposal.original_text),
                    replacement: item.proposal.sanitized_text.clone(),
                    policy_source: "alias-registry",
                });
                content_config_changed = true;
            }
        }
        items.push(item);
    }
    let semantic_aliases = semantic_aliases.into_values().collect::<Vec<_>>();
    crate::sanitize::validate_sanitizer_config(&candidate_config)
        .context("selected approvals produce an invalid sanitizer policy")?;
    crate::path_projection::PathProjection::build(&candidate_config, tracked.iter())
        .context("selected approvals produce a path projection collision")?;

    if !collision_terms.is_empty() {
        for rel in &tracked {
            let content = match source_cache.get(rel) {
                Some(content) => Arc::clone(content),
                None => match std::fs::read_to_string(root.join(rel)) {
                    Ok(content) => Arc::from(content),
                    Err(_) => continue,
                },
            };
            if let Some(collision) =
                crate::sanitize::alias_collisions(&content, &collision_terms).first()
            {
                let display_file = projection.projected_string_for_real(rel)?;
                bail!(
                    "selected alias {:?} (for {:?}) occurs in {display_file} at byte {} as {:?}; approval refused — pick a different alias",
                    collision.alias,
                    collision.term,
                    collision.offset,
                    collision.word
                );
            }
        }
    }

    let compiler_admissions = prepared
        .iter()
        .flatten()
        .map(
            |resolution| crate::semantic_store::CompilerReferenceAdmission {
                symbol_id: resolution.symbol_id.clone(),
                provider: resolution.provider.clone(),
                locations: resolution.locations.clone(),
                equivalent_symbol_ids: resolution.equivalent_symbol_ids.clone(),
            },
        )
        .collect::<Vec<_>>();
    if !compiler_admissions.is_empty() {
        progress(ApprovalProgress {
            detail: format!(
                "atomically admitting {} compiler closures",
                compiler_admissions.len()
            ),
            completed: 0,
            total: compiler_admissions.len(),
        });
        crate::semantic_store::admit_compiler_reference_batch(
            &mut conn,
            root,
            &compiler_admissions,
        )
        .context("atomically admit validated compiler closures")?;
    }
    progress(ApprovalProgress {
        detail: format!("applying {} approved aliases", ids.len()),
        completed: 0,
        total: ids.len(),
    });
    crate::semantic_store::accept_symbol_aliases(&mut conn, &semantic_aliases)?;
    if path_config_changed || content_config_changed {
        candidate_config.save(&layout)?;
    }
    drop(conn);

    progress(ApprovalProgress {
        detail: "refreshing sanitized mirror once".to_string(),
        completed: 0,
        total: 1,
    });
    if content_config_changed {
        reconverge_workspace(root, &layout).context("reindex after bulk alias approval")?;
    } else if path_config_changed {
        crate::index::reproject_tracked_mirrors_locked(root, &layout)
            .context("reproject mirrors after bulk path alias approval")?;
    } else if !semantic_aliases.is_empty() {
        let changed_symbols = semantic_aliases
            .iter()
            .map(|alias| alias.symbol_id.clone())
            .collect::<BTreeSet<_>>();
        crate::index::refresh_semantic_mirrors_for_symbols_locked(root, &layout, &changed_symbols)
            .context("refresh mirrors after bulk semantic alias approval")?;
    }

    let status_conn = db::connect(&layout)?;
    db::check_schema(&status_conn)?;
    let item_total = items.len();
    for (index, item) in items.iter_mut().enumerate() {
        item.status = ReviewStatus::Approved;
        let path = layout.review_dir.join(format!("{}.json", item.id));
        let updated = serde_json::to_string_pretty(item).context("serialize review item")?;
        crate::fsutil::atomic_write(&path, &updated)
            .with_context(|| format!("write {}", path.display()))?;
        if matches!(
            item.proposal.target.as_ref(),
            Some(ProposalTarget::Semantic(_))
        ) {
            crate::semantic_store::update_proposal_status(&status_conn, &item.id, "approved")?;
        }
        if index == 0 || (index + 1) % 32 == 0 || index + 1 == item_total {
            progress(ApprovalProgress {
                detail: format!("recording approvals ({}/{item_total})", index + 1),
                completed: index + 1,
                total: item_total,
            });
        }
    }
    drop(status_conn);
    record_review_decisions(&layout, &items)?;
    let keep = Config::load_or_default_lenient(&layout)
        .map(|config| config.journal.max_entries)
        .unwrap_or(0);
    if let Err(err) = prune_resolved_reviews(&layout, keep) {
        log::warn!("review-queue pruning failed: {err:#}");
    }
    for item in &mut items {
        if let Ok(projected) =
            crate::path_projection::project_rel_path(Path::new(&item.file), &candidate_config)
        {
            item.file = crate::config::normalize_rel_path(&projected);
        }
    }
    Ok(items)
}

fn preflight_existing_target_aliases(
    root: &Path,
    ids: &[String],
) -> Result<BTreeMap<String, String>> {
    let layout = Layout::new(root);
    let _lock = WorkspaceLock::acquire_shared(&layout)?;
    let conn = db::connect(&layout)?;
    db::check_schema(&conn)?;
    let mut items = Vec::with_capacity(ids.len());
    let mut target_seeds = Vec::with_capacity(ids.len());
    for id in ids {
        let path = layout.review_dir.join(format!("{id}.json"));
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("review item {id} not found"))?;
        let item: ReviewItem =
            serde_json::from_str(&raw).with_context(|| format!("parse review item {id}"))?;
        if item.status != ReviewStatus::Pending {
            bail!("review item {id} is already {:?}", item.status);
        }
        if let Some(ProposalTarget::Semantic(target)) = item.proposal.target.as_ref() {
            target_seeds.push(Some(PreparedCompilerProposalResolution {
                symbol_id: target.symbol_id.clone(),
                document_fingerprint: String::new(),
                provider: String::new(),
                locations: Vec::new(),
                equivalent_symbol_ids: BTreeSet::new(),
            }));
        }
        items.push(item);
    }
    let representatives = semantic_alias_representatives(root, &conn, &target_seeds)?;
    let mut aliases_by_target = BTreeMap::<String, Vec<(String, String)>>::new();
    for pair in crate::semantic_store::accepted_alias_bindings(&conn)? {
        let target = format!(
            "semantic:{}",
            representatives
                .get(&pair.symbol_id)
                .unwrap_or(&pair.symbol_id)
        );
        aliases_by_target.entry(target).or_default().push((
            pair.alias.clone(),
            format!(
                "accepted semantic target {} ({:?} -> {:?})",
                pair.symbol_id, pair.original, pair.alias
            ),
        ));
    }
    for item in &items {
        let identity = approval_alias_identity(&item.proposal, &representatives);
        if let Some((_, existing)) = aliases_by_target
            .get(&identity)
            .into_iter()
            .flatten()
            .find(|(alias, _)| alias != &item.proposal.sanitized_text)
        {
            bail!(
                "selected target has incompatible aliases: {existing}; {} ({}: {:?} -> {:?})",
                item.id,
                item.file,
                item.proposal.original_text,
                item.proposal.sanitized_text
            );
        }
        aliases_by_target.entry(identity).or_default().push((
            item.proposal.sanitized_text.clone(),
            format!(
                "{} ({}: {:?} -> {:?})",
                item.id, item.file, item.proposal.original_text, item.proposal.sanitized_text
            ),
        ));
    }
    Ok(representatives)
}

fn resolve_review_prepared(
    root: &Path,
    id: &str,
    approve: bool,
    compiler_resolution: Option<PreparedCompilerProposalResolution>,
) -> Result<ReviewItem> {
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
        let mut conn = db::connect(&layout)?;
        db::check_schema(&conn)?;
        let projection = crate::path_projection::PathProjection::from_connection(&config, &conn)?;
        let tracked_files = db::tracked_files(&conn)?;
        match &item.proposal.target {
            Some(ProposalTarget::FilePath(_)) => {
                let candidates = file_path_candidates(&item.file, &projection)?;
                validate_file_path_proposal(&item.proposal, &candidates, &config, &tracked_files)
                    .map_err(|reason| anyhow!("proposal no longer valid: {reason}"))?;
                config.sanitizer.path_alias_registry.insert(
                    item.proposal.original_text.clone(),
                    item.proposal.sanitized_text.clone(),
                );
                config.save(&layout)?;
                reconverge_workspace(root, &layout)
                    .with_context(|| format!("reindex after approving path alias {}", item.id))?;
            }
            semantic_or_legacy => {
                // Re-validate source proposals at approval time so a stale
                // queue cannot apply an unsafe alias.
                let real = std::fs::read_to_string(root.join(&item.file))
                    .with_context(|| format!("read {}", item.file))?;
                validate_proposal(Path::new(&item.file), &item.proposal, &real, &config)
                    .map_err(|reason| anyhow!("proposal no longer valid: {reason}"))?;

                // Repo-wide content alias-collision scan before persistence.
                let candidate_terms = [crate::sanitize::Term {
                    raw: item.proposal.original_text.clone(),
                    normalized: crate::sanitize::normalize_term(&item.proposal.original_text),
                    replacement: item.proposal.sanitized_text.clone(),
                    policy_source: "alias-registry",
                }];
                for rel in &tracked_files {
                    let Ok(content) = std::fs::read_to_string(root.join(rel)) else {
                        continue;
                    };
                    if let Some(collision) =
                        crate::sanitize::alias_collisions(&content, &candidate_terms).first()
                    {
                        let display_file = projection.projected_string_for_real(rel)?;
                        bail!(
                            "proposal alias {:?} occurs in {display_file} at byte {} as {:?}; approval \
                             refused — pick a different alias",
                            item.proposal.sanitized_text,
                            collision.offset,
                            collision.word
                        );
                    }
                }

                if let Some(ProposalTarget::Semantic(target)) = semantic_or_legacy {
                    let (target_file, symbol) =
                        crate::semantic_store::load_symbol_with_path(&conn, &target.symbol_id)?
                            .ok_or_else(|| anyhow!("proposal target symbol no longer exists"))?;
                    if target_file != item.file || symbol.name != item.proposal.original_text {
                        bail!("proposal target no longer matches its indexed symbol");
                    }
                    let occurrence_matches =
                        crate::semantic_store::occurrences_for_symbol(&conn, &target.symbol_id)?
                            .iter()
                            .any(|(_, occurrence)| {
                                occurrence.occurrence_id == target.occurrence_id
                            });
                    if !occurrence_matches {
                        bail!("proposal target occurrence no longer exists");
                    }
                    if let Some(prepared) = compiler_resolution.as_ref() {
                        if prepared.symbol_id != target.symbol_id
                            || crate::semantic_store::document_fingerprint(&conn)?
                                != prepared.document_fingerprint
                        {
                            bail!(
                                "semantic workspace changed during compiler resolution; retry approval"
                            );
                        }
                        // Injectivity is checked before compiler admission,
                        // whose own transaction advances the semantic graph.
                        // A bad alias must not leave compiler links behind
                        // when the review item itself remains pending.
                        crate::semantic_store::validate_symbol_alias_candidate(
                            &conn,
                            &target.symbol_id,
                            &item.proposal.sanitized_text,
                            &prepared.equivalent_symbol_ids,
                        )?;
                        admit_prepared_compiler_resolution(&mut conn, root, prepared)?;
                    }
                    crate::semantic_store::accept_symbol_alias(
                        &mut conn,
                        &target.symbol_id,
                        &item.proposal.sanitized_text,
                        &item.proposal.category,
                        item.proposal.confidence,
                        item.proposal.rationale.as_deref(),
                    )?;
                } else {
                    config.sanitizer.alias_registry.insert(
                        item.proposal.original_text.clone(),
                        item.proposal.sanitized_text.clone(),
                    );
                    config.save(&layout)?;
                    // Legacy review items preserve the v1 global registry behavior.
                    reconverge_workspace(root, &layout)
                        .with_context(|| format!("reindex after approving {}", item.id))?;
                }
            }
        }
        drop(conn);
        if matches!(
            item.proposal.target.as_ref(),
            Some(ProposalTarget::Semantic(_))
        ) {
            let changed_symbols = item
                .proposal
                .target
                .as_ref()
                .and_then(|target| match target {
                    ProposalTarget::Semantic(target) => Some(target.symbol_id.clone()),
                    ProposalTarget::FilePath(_) => None,
                })
                .into_iter()
                .collect::<BTreeSet<_>>();
            crate::index::refresh_semantic_mirrors_for_symbols_locked(
                root,
                &layout,
                &changed_symbols,
            )
            .context("refresh mirrors after semantic alias approval")?;
        }
        item.status = ReviewStatus::Approved;
    } else {
        item.status = ReviewStatus::Rejected;
    }
    let updated = serde_json::to_string_pretty(&item).context("serialize review item")?;
    crate::fsutil::atomic_write(&path, &updated)
        .with_context(|| format!("write {}", path.display()))?;
    record_review_decision(&layout, &item)?;
    if matches!(
        item.proposal.target.as_ref(),
        Some(ProposalTarget::Semantic(_))
    ) {
        let conn = db::connect(&layout)?;
        db::check_schema(&conn)?;
        crate::semantic_store::update_proposal_status(
            &conn,
            &item.id,
            match item.status {
                ReviewStatus::Approved => "approved",
                ReviewStatus::Rejected => "rejected",
                ReviewStatus::Stale => "stale",
                ReviewStatus::Pending => "pending",
            },
        )?;
    }
    // Best-effort retention of the resolved history (same knob as the
    // journal); the resolution itself already landed. Lenient config load: a
    // sanitizer policy violation must not fail a reject.
    let keep = Config::load_or_default_lenient(&layout)
        .map(|config| config.journal.max_entries)
        .unwrap_or(0);
    if let Err(err) = prune_resolved_reviews(&layout, keep) {
        log::warn!("review-queue pruning failed: {err:#}");
    }
    if let Ok(config) = Config::load_or_default_lenient(&layout) {
        if let Ok(projected) =
            crate::path_projection::project_rel_path(Path::new(&item.file), &config)
        {
            item.file = crate::config::normalize_rel_path(&projected);
        }
    }
    Ok(item)
}

#[derive(Clone, PartialEq, Eq)]
struct PreparedCompilerProposalResolution {
    symbol_id: String,
    document_fingerprint: String,
    provider: String,
    locations: Vec<crate::lsp::LspLocation>,
    equivalent_symbol_ids: BTreeSet<String>,
}

/// Validate a prepared compiler closure without mutating the semantic store.
/// If clangd produced an unusable closure for a genuinely translation-unit-
/// local symbol, replace it with the deterministic syntax closure and report
/// that the representative graph must be rebuilt.
fn validate_prepared_compiler_resolution(
    conn: &rusqlite::Connection,
    root: &Path,
    prepared: &mut PreparedCompilerProposalResolution,
) -> Result<bool> {
    match crate::semantic_store::validate_compiler_references_with_equivalents(
        conn,
        root,
        &prepared.symbol_id,
        &prepared.locations,
        &prepared.equivalent_symbol_ids,
    ) {
        Ok(()) => Ok(false),
        Err(error) if prepared.provider == "syntax:translation-unit-local" => Err(error),
        Err(compiler_error) => {
            let fallback = crate::semantic_store::translation_unit_local_reference_closure(
                conn,
                root,
                &prepared.symbol_id,
            )
            .map_err(|fallback_error| {
                anyhow!(
                    "compiler closure failed: {compiler_error:#}; translation-unit-local fallback inspection failed: {fallback_error:#}"
                )
            })?;
            let Some(locations) = fallback else {
                return Err(compiler_error);
            };
            crate::semantic_store::validate_compiler_references_with_equivalents(
                conn,
                root,
                &prepared.symbol_id,
                &locations,
                &prepared.equivalent_symbol_ids,
            )
            .with_context(|| {
                format!(
                    "compiler closure failed ({compiler_error:#}); translation-unit-local fallback also failed"
                )
            })?;
            let changed = prepared.provider != "syntax:translation-unit-local"
                || prepared.locations != locations;
            prepared.provider = "syntax:translation-unit-local".to_string();
            prepared.locations = locations;
            Ok(changed)
        }
    }
}

/// Quarantine only the selected reviews whose compiler closures are incomplete.
/// The remaining closure set is re-canonicalized until validation reaches a
/// fixed point, so an invalid closure cannot lend equivalence edges to an
/// otherwise unrelated proposal or abort the whole Select All operation.
struct CompilerQuarantineContext<'a> {
    root: &'a Path,
    layout: &'a Layout,
    conn: &'a rusqlite::Connection,
    base_representatives: &'a BTreeMap<String, String>,
    current_fingerprint: &'a str,
}

struct CompilerQuarantineResult {
    ids: Vec<String>,
    prepared: Vec<Option<PreparedCompilerProposalResolution>>,
    representatives: BTreeMap<String, String>,
    retired: usize,
}

fn quarantine_unadmissible_compiler_resolutions(
    context: CompilerQuarantineContext<'_>,
    mut ids: Vec<String>,
    mut prepared: Vec<Option<PreparedCompilerProposalResolution>>,
    progress: &mut impl FnMut(ApprovalProgress),
) -> Result<CompilerQuarantineResult> {
    let CompilerQuarantineContext {
        root,
        layout,
        conn,
        base_representatives,
        current_fingerprint,
    } = context;
    let mut retired_total = 0usize;
    let mut validated = BTreeMap::<String, PreparedCompilerProposalResolution>::new();
    loop {
        let representatives = semantic_alias_representatives_with_base(
            root,
            conn,
            &prepared,
            Some(base_representatives),
        )?;
        populate_alias_equivalents_from_representatives(&mut prepared, &representatives);

        let compiler_total = ids
            .iter()
            .zip(&prepared)
            .filter(|(id, resolution)| {
                resolution.as_ref().is_some_and(|resolution| {
                    validated.get(*id).is_none_or(|cached| cached != resolution)
                })
            })
            .count();
        let mut compiler_done = 0usize;
        let mut retired = BTreeMap::<String, String>::new();
        let mut closure_changed = false;
        for (id, resolution) in ids.iter().zip(&mut prepared) {
            let Some(resolution) = resolution else {
                continue;
            };
            if resolution.document_fingerprint != current_fingerprint {
                bail!("semantic workspace changed while validating review {id}; retry approval");
            }
            if validated.get(id).is_some_and(|cached| cached == resolution) {
                continue;
            }
            if compiler_done == 0
                || (compiler_done + 1) % 32 == 0
                || compiler_done + 1 == compiler_total
            {
                progress(ApprovalProgress {
                    detail: format!(
                        "validating compiler closures ({}/{compiler_total})",
                        compiler_done + 1
                    ),
                    completed: compiler_done,
                    total: compiler_total,
                });
            }
            match validate_prepared_compiler_resolution(conn, root, resolution) {
                Ok(changed) => {
                    closure_changed |= changed;
                    validated.insert(id.clone(), resolution.clone());
                }
                Err(error) => {
                    retired.insert(id.clone(), format!("{error:#}"));
                }
            }
            compiler_done += 1;
        }

        if retired.is_empty() && !closure_changed {
            return Ok(CompilerQuarantineResult {
                ids,
                prepared,
                representatives,
                retired: retired_total,
            });
        }

        if !retired.is_empty() {
            for (id, reason) in &retired {
                let path = layout.review_dir.join(format!("{id}.json"));
                let raw = std::fs::read_to_string(&path)
                    .with_context(|| format!("read unadmissible compiler review {id}"))?;
                let mut item: ReviewItem = serde_json::from_str(&raw)
                    .with_context(|| format!("parse {}", path.display()))?;
                if item.status != ReviewStatus::Pending {
                    bail!("review item {id} changed status while validating compiler closure");
                }
                item.status = ReviewStatus::Stale;
                item.flag = combine_review_flags(
                    &item.flag,
                    &format!("compiler closure is incomplete; retry after reindexing: {reason}"),
                );
                crate::fsutil::atomic_write(&path, &serde_json::to_string_pretty(&item)?)
                    .with_context(|| format!("retire unadmissible compiler review {id}"))?;
                crate::semantic_store::update_proposal_status(conn, id, "stale")?;
            }
            retired_total += retired.len();
            progress(ApprovalProgress {
                detail: format!(
                    "retired {} proposals with incomplete compiler closures",
                    retired.len()
                ),
                completed: retired.len(),
                total: ids.len(),
            });
            let mut remaining_ids = Vec::with_capacity(ids.len() - retired.len());
            let mut remaining_prepared = Vec::with_capacity(remaining_ids.capacity());
            for (id, resolution) in ids.into_iter().zip(prepared) {
                if !retired.contains_key(&id) {
                    remaining_ids.push(id);
                    remaining_prepared.push(resolution);
                }
            }
            ids = remaining_ids;
            prepared = remaining_prepared;
            if ids.is_empty() {
                return Ok(CompilerQuarantineResult {
                    ids,
                    prepared,
                    representatives: BTreeMap::new(),
                    retired: retired_total,
                });
            }
        }
    }
}

fn admit_prepared_compiler_resolution(
    conn: &mut rusqlite::Connection,
    root: &Path,
    prepared: &PreparedCompilerProposalResolution,
) -> Result<()> {
    match crate::semantic_store::admit_compiler_references_with_equivalents(
        conn,
        root,
        &prepared.symbol_id,
        &prepared.provider,
        &prepared.locations,
        &prepared.equivalent_symbol_ids,
    ) {
        Ok(_) => return Ok(()),
        Err(error) if prepared.provider == "syntax:translation-unit-local" => return Err(error),
        Err(compiler_error) => {
            let fallback = crate::semantic_store::translation_unit_local_reference_closure(
                conn,
                root,
                &prepared.symbol_id,
            )
            .map_err(|fallback_error| {
                anyhow!(
                    "compiler closure failed: {compiler_error:#}; translation-unit-local fallback inspection failed: {fallback_error:#}"
                )
            })?;
            let Some(locations) = fallback else {
                return Err(compiler_error);
            };
            crate::semantic_store::admit_compiler_references_with_equivalents(
                conn,
                root,
                &prepared.symbol_id,
                "syntax:translation-unit-local",
                &locations,
                &prepared.equivalent_symbol_ids,
            )
            .with_context(|| {
                format!(
                    "compiler closure failed ({compiler_error:#}); translation-unit-local fallback also failed"
                )
            })?;
        }
    }
    Ok(())
}

fn populate_alias_equivalents_from_representatives(
    prepared: &mut [Option<PreparedCompilerProposalResolution>],
    representatives: &BTreeMap<String, String>,
) {
    let mut components = BTreeMap::<String, BTreeSet<String>>::new();
    for (symbol_id, representative) in representatives {
        components
            .entry(representative.clone())
            .or_default()
            .insert(symbol_id.clone());
    }
    for resolution in prepared.iter_mut().flatten() {
        let representative = representatives
            .get(&resolution.symbol_id)
            .unwrap_or(&resolution.symbol_id);
        resolution.equivalent_symbol_ids =
            components.get(representative).cloned().unwrap_or_default();
        resolution
            .equivalent_symbol_ids
            .remove(&resolution.symbol_id);
    }
}

fn populate_prepared_alias_equivalents(
    root: &Path,
    prepared: &mut [Option<PreparedCompilerProposalResolution>],
) -> Result<()> {
    let layout = Layout::new(root);
    let _lock = WorkspaceLock::acquire_shared(&layout)?;
    let conn = db::connect(&layout)?;
    db::check_schema(&conn)?;
    let representatives = semantic_alias_representatives(root, &conn, prepared)?;
    populate_alias_equivalents_from_representatives(prepared, &representatives);
    Ok(())
}

fn prepare_compiler_proposal_resolution(
    root: &Path,
    id: &str,
) -> Result<Option<PreparedCompilerProposalResolution>> {
    prepare_compiler_proposal_resolutions(root, &[id.to_string()], &BTreeMap::new(), &mut |_| {})?
        .pop()
        .context("compiler proposal preparation omitted its only result")
}

fn prepare_compiler_proposal_resolutions(
    root: &Path,
    ids: &[String],
    representatives: &BTreeMap<String, String>,
    progress: &mut impl FnMut(ApprovalProgress),
) -> Result<Vec<Option<PreparedCompilerProposalResolution>>> {
    let layout = Layout::new(root);
    layout.require_initialized()?;
    struct PendingLspResolution {
        slot: usize,
        symbol_id: String,
        document_fingerprint: String,
        provider: String,
        request: crate::lsp::ReferenceBatchRequest,
    }
    let (mut prepared, pending_lsp) = {
        let _lock = crate::lock::WorkspaceLock::acquire_shared(&layout)?;
        let conn = db::connect(&layout)?;
        db::check_schema(&conn)?;
        let document_fingerprint = crate::semantic_store::document_fingerprint(&conn)?;
        let mut prepared = vec![None; ids.len()];
        let mut pending_lsp = Vec::new();
        let mut source_cache = BTreeMap::<String, Arc<str>>::new();
        let mut compiler_components = BTreeSet::<String>::new();
        let review_items = ids
            .iter()
            .map(|id| -> Result<ReviewItem> {
                let path = layout.review_dir.join(format!("{id}.json"));
                let raw = std::fs::read_to_string(&path)
                    .with_context(|| format!("review item {id} not found ({})", path.display()))?;
                let item: ReviewItem = serde_json::from_str(&raw)
                    .with_context(|| format!("parse {}", path.display()))?;
                if item.status != ReviewStatus::Pending {
                    bail!("review item {id} is already {:?}", item.status);
                }
                Ok(item)
            })
            .collect::<Result<Vec<_>>>()?;
        let compiler_targets = review_items
            .iter()
            .filter_map(|item| match item.proposal.target.as_ref() {
                Some(ProposalTarget::Semantic(target)) => Some(target.symbol_id.clone()),
                Some(ProposalTarget::FilePath(_)) | None => None,
            })
            .collect::<Vec<_>>();
        progress(ApprovalProgress {
            detail: "detecting translation-unit-local closures".to_string(),
            completed: 0,
            total: compiler_targets.len(),
        });
        let static_closures = crate::semantic_store::translation_unit_local_reference_closures(
            &conn,
            root,
            &compiler_targets,
        )?;
        for (slot, (id, item)) in ids.iter().zip(review_items).enumerate() {
            if slot == 0 || (slot + 1) % 32 == 0 || slot + 1 == ids.len() {
                progress(ApprovalProgress {
                    detail: format!("classifying proposal closures ({}/{})", slot + 1, ids.len()),
                    completed: slot + 1,
                    total: ids.len(),
                });
            }
            let result = (|| -> Result<()> {
                let Some(ProposalTarget::Semantic(target)) = item.proposal.target else {
                    return Ok(());
                };
                let (rel_path, symbol) =
                    crate::semantic_store::load_symbol_with_path(&conn, &target.symbol_id)?
                        .ok_or_else(|| anyhow!("proposal target symbol no longer exists"))?;
                let document = crate::semantic_store::load_document(&conn, &rel_path)?
                    .ok_or_else(|| anyhow!("proposal target document disappeared"))?;
                if crate::semantic_store::symbol_is_lexically_closed(&conn, &target.symbol_id)? {
                    return Ok(());
                }
                let component = representatives
                    .get(&target.symbol_id)
                    .unwrap_or(&target.symbol_id)
                    .clone();
                if !compiler_components.insert(component) {
                    // One authoritative closure admits/aliases the complete
                    // syntax-proven declaration/definition component.
                    return Ok(());
                }
                if let Some(locations) = static_closures.get(&target.symbol_id) {
                    prepared[slot] = Some(PreparedCompilerProposalResolution {
                        symbol_id: target.symbol_id,
                        document_fingerprint: document_fingerprint.clone(),
                        provider: "syntax:translation-unit-local".to_string(),
                        locations: locations.clone(),
                        equivalent_symbol_ids: BTreeSet::new(),
                    });
                    return Ok(());
                }
                let provider = match document.capabilities.semantic_provider.clone() {
                    Some(provider) => provider,
                    None => bail!(
                        "compiler-backed approval is unavailable for non-local symbol in {rel_path}"
                    ),
                };
                let rel = crate::config::normalize_safe_rel_path(
                    Path::new(&rel_path),
                    "compiler proposal target",
                )?;
                let source = match source_cache.get(&rel_path) {
                    Some(source) => Arc::clone(source),
                    None => {
                        let source: Arc<str> =
                            Arc::from(std::fs::read_to_string(root.join(&rel)).with_context(
                                || format!("read compiler proposal target {rel_path}"),
                            )?);
                        if crate::map::sha256_hex(source.as_bytes()) != document.content_hash {
                            bail!(
                                "{rel_path} changed since semantic indexing; run code-sanity index"
                            );
                        }
                        source_cache.insert(rel_path.clone(), Arc::clone(&source));
                        source
                    }
                };
                pending_lsp.push(PendingLspResolution {
                    slot,
                    symbol_id: target.symbol_id,
                    document_fingerprint: document_fingerprint.clone(),
                    provider,
                    request: crate::lsp::ReferenceBatchRequest {
                        rel_path: rel,
                        source,
                        language: document.language,
                        declaration: symbol.range,
                        minimum_references: 0,
                    },
                });
                Ok(())
            })();
            result.with_context(|| format!("prepare semantic closure for review {id}"))?;
        }
        (prepared, pending_lsp)
    };
    if !pending_lsp.is_empty() {
        let requests = pending_lsp
            .iter()
            .map(|pending| pending.request.clone())
            .collect::<Vec<_>>();
        let locations = crate::lsp::references_batch(root, &requests, |completed, total| {
            progress(ApprovalProgress {
                detail: format!("compiler reference closures ({completed}/{total})"),
                completed,
                total,
            });
        })?;
        for (pending, locations) in pending_lsp.into_iter().zip(locations) {
            prepared[pending.slot] = Some(PreparedCompilerProposalResolution {
                symbol_id: pending.symbol_id,
                document_fingerprint: pending.document_fingerprint,
                provider: pending.provider,
                locations,
                equivalent_symbol_ids: BTreeSet::new(),
            });
        }
    }
    Ok(prepared)
}

/// Delete the oldest RESOLVED review items beyond `keep` (0 = unlimited).
/// Pending items are the actionable queue and are never touched; unparseable
/// files are kept for a human to inspect. Item ids start with a sortable UTC
/// timestamp, so lexicographic order is age order.
fn prune_resolved_reviews(layout: &Layout, keep: u64) -> Result<usize> {
    if keep == 0 {
        return Ok(0);
    }
    let read_dir = match std::fs::read_dir(&layout.review_dir) {
        Ok(read_dir) => read_dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => {
            return Err(err).with_context(|| format!("read {}", layout.review_dir.display()));
        }
    };
    let mut resolved: Vec<(String, std::path::PathBuf)> = Vec::new();
    for entry in read_dir {
        let path = entry.context("read review dir entry")?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(item) = serde_json::from_str::<ReviewItem>(&raw) else {
            continue;
        };
        if item.status != ReviewStatus::Pending {
            resolved.push((item.id, path));
        }
    }
    resolved.sort();
    let excess = resolved.len().saturating_sub(keep as usize);
    let mut removed = 0usize;
    for (_, path) in resolved.into_iter().take(excess) {
        std::fs::remove_file(&path)
            .with_context(|| format!("prune review item {}", path.display()))?;
        removed += 1;
    }
    Ok(removed)
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
    let config = Config::load_or_default(&layout)?;
    let projection = crate::path_projection::PathProjection::from_connection(&config, &conn)?;
    let files = match rel {
        Some(rel) => vec![crate::config::normalize_rel_path(
            &projection.real_for_agent(rel)?,
        )],
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
                file: projection.projected_string_for_real(&file)?,
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

    fn semantic_candidate(symbol_id: &str, name: &str) -> SemanticCandidate {
        SemanticCandidate {
            symbol_id: symbol_id.to_string(),
            occurrence_id: format!("occ_{symbol_id}"),
            name: name.to_string(),
            kind: "variable".to_string(),
            qualified_name: format!("scope::{symbol_id}"),
            declaration_line: 1,
            reference_count: 1,
            references_complete: true,
            compiler_resolvable: false,
            occurrence_lines: vec![1],
            call_lines: Vec::new(),
            signature: format!("let {name} = 1;"),
            enclosing_code: format!("let {name} = 1;"),
            api_boundary: false,
            lexically_closed: true,
            origin: "owned".to_string(),
            existing_alias: None,
        }
    }

    #[test]
    fn proposal_chunks_are_line_aligned_and_overlap() {
        let content = (1..=20)
            .map(|line| format!("line_{line:02}_payload\n"))
            .collect::<String>();
        let chunks = split_proposal_chunks(&content, 64, 1);
        assert!(chunks.len() > 1);
        for (index, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.meta.index, index + 1);
            assert_eq!(chunk.meta.total, chunks.len());
            assert!(content.contains(&chunk.content));
            assert!(chunk.content.ends_with('\n'));
        }
        for pair in chunks.windows(2) {
            assert_eq!(
                pair[0].content.lines().next_back(),
                pair[1].content.lines().next(),
                "adjacent chunks must share one complete line"
            );
            assert_eq!(
                pair[1].meta.core_start_line,
                pair[0].meta.end_line + 1,
                "the next chunk must not own its overlap prefix"
            );
        }
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| proposal_core_content(&chunk.content, chunk.meta))
                .collect::<String>(),
            content,
            "owned regions must cover the source exactly once"
        );
    }

    #[test]
    fn proposal_chunker_keeps_small_and_long_utf8_lines_intact() {
        let small = "alpha\nbeta\n";
        let chunks = split_proposal_chunks(small, 1024, 12);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].content, small);
        assert_eq!(chunks[0].meta.start_line, 1);
        assert_eq!(chunks[0].meta.end_line, 2);

        let long = format!("{}\nshort\n", "界".repeat(100));
        let chunks = split_proposal_chunks(&long, 32, 1);
        assert!(chunks[0].content.starts_with('界'));
        assert!(chunks[0].content.len() > 32, "one long line stays whole");
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.content.as_str())
                .collect::<Vec<_>>()
                .concat(),
            long
        );
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
    fn non_json_provider_replies_get_safe_actionable_errors() {
        let refusal = parse_proposals("I can't discuss that.").unwrap_err();
        assert!(refusal.to_string().contains("provider refused"));
        assert!(!refusal.to_string().contains("discuss"));

        let prose = parse_proposals("Here are the proposals you requested.").unwrap_err();
        assert!(prose.to_string().contains("non-JSON text"));
        let empty = parse_proposals("  \n").unwrap_err();
        assert!(empty.to_string().contains("empty response"));
    }

    #[test]
    fn duplicate_model_object_keys_are_normalized_before_validation() {
        let proposals = parse_proposals(
            r#"{"proposals":[{"category":"identifier","original_text":"stale","original_text":"helper","sanitized_text":"assistant","confidence":0.9}]}"#,
        )
        .unwrap();
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].original_text, "helper");
    }

    #[test]
    fn malformed_proposal_objects_do_not_discard_valid_siblings() {
        let proposals = parse_proposals(
            r#"{
                "proposals": [
                    {
                        "target": {
                            "symbol_id": "sym_valid",
                            "occurrence_id": "occ_valid",
                            "type": "identifier"
                        },
                        "category": "string",
                        "original_text": "shadowfax",
                        "sanitized_text": "neutral_helper",
                        "confidence": 0.9
                    },
                    {
                        "target": { "symbol_id": "sym_broken" },
                        "category": "identifier",
                        "original_text": "broken",
                        "sanitized_text": "neutral_broken",
                        "confidence": 0.9
                    }
                ]
            }"#,
        )
        .unwrap();
        assert_eq!(proposals.len(), 1);
        assert!(matches!(
            proposals[0].target,
            Some(ProposalTarget::Semantic(_))
        ));
        assert_eq!(proposals[0].category, "string");
    }

    #[test]
    fn invented_or_ambiguous_semantic_targets_are_rejected() {
        let candidates = ["sym_a", "sym_b"]
            .into_iter()
            .map(|symbol_id| semantic_candidate(symbol_id, "hwid"))
            .collect::<Vec<_>>();
        let mut invented = Proposal {
            target: Some(ProposalTarget::Semantic(SemanticProposalTarget {
                symbol_id: "sym_missing".to_string(),
                occurrence_id: "occ_missing".to_string(),
            })),
            category: "identifier".to_string(),
            original_text: "hwid".to_string(),
            sanitized_text: "device_id".to_string(),
            confidence: 1.0,
            rationale: None,
        };
        assert!(attach_semantic_target(&mut invented, &candidates).is_err());

        invented.target = None;
        let reason = attach_semantic_target(&mut invented, &candidates).unwrap_err();
        assert!(reason.contains("multiple semantic symbols"), "{reason}");
    }

    #[test]
    fn typed_target_recovers_model_category_drift() {
        let semantic = semantic_candidate("sym_auth", "auth_handler");
        let mut semantic_proposal = Proposal {
            target: Some(ProposalTarget::Semantic(SemanticProposalTarget {
                symbol_id: semantic.symbol_id.clone(),
                occurrence_id: semantic.occurrence_id.clone(),
            })),
            category: "string".to_string(),
            original_text: semantic.name.clone(),
            sanitized_text: "session_handler".to_string(),
            confidence: 0.8,
            rationale: None,
        };
        attach_proposal_target(&mut semantic_proposal, &[semantic], &[]).unwrap();
        assert_eq!(semantic_proposal.category, "identifier");

        let path = FilePathCandidate {
            path_id: "path-risk-loader".to_string(),
            path: "src/risk_loader.rs".to_string(),
            component_index: 1,
            kind: "filename_stem".to_string(),
            value: "risk_loader".to_string(),
        };
        let mut path_proposal = Proposal {
            target: Some(ProposalTarget::FilePath(FilePathProposalTarget {
                path_id: path.path_id.clone(),
            })),
            category: "filename".to_string(),
            original_text: "risk".to_string(),
            sanitized_text: "review".to_string(),
            confidence: 0.8,
            rationale: None,
        };
        attach_proposal_target(&mut path_proposal, &[], &[path]).unwrap();
        assert_eq!(path_proposal.category, "file_path");
    }

    #[test]
    fn content_and_proposal_allowlists_are_independent() {
        let mut config = Config::default();
        let semantic = Proposal {
            target: Some(ProposalTarget::Semantic(SemanticProposalTarget {
                symbol_id: "sym_auth".to_string(),
                occurrence_id: "occ_auth".to_string(),
            })),
            category: "identifier".to_string(),
            original_text: "auth".to_string(),
            sanitized_text: "session_gate".to_string(),
            confidence: 1.0,
            rationale: None,
        };
        assert!(
            validate_proposal(Path::new("src/lib.rs"), &semantic, "fn auth() {}", &config).is_ok(),
            "the legacy content allowlist must not suppress a typed symbol"
        );
        config.sanitizer.proposal_allowlist.push("auth".into());
        assert!(
            validate_proposal(Path::new("src/lib.rs"), &semantic, "fn auth() {}", &config)
                .unwrap_err()
                .contains("allowlisted")
        );

        config.sanitizer.proposal_allowlist.clear();
        let path_candidate = FilePathCandidate {
            path_id: "path-auth".to_string(),
            path: "auth.rs".to_string(),
            component_index: 0,
            kind: "filename_stem".to_string(),
            value: "auth".to_string(),
        };
        let path_proposal = Proposal {
            target: Some(ProposalTarget::FilePath(FilePathProposalTarget {
                path_id: path_candidate.path_id.clone(),
            })),
            category: "file_path".to_string(),
            original_text: "auth".to_string(),
            sanitized_text: "session_gate".to_string(),
            confidence: 1.0,
            rationale: None,
        };
        assert!(
            validate_file_path_proposal(
                &path_proposal,
                std::slice::from_ref(&path_candidate),
                &config,
                &["auth.rs".to_string()]
            )
            .is_ok(),
            "the content allowlist must not suppress path metadata"
        );
        config.sanitizer.path_allowlist.push("auth".into());
        assert!(
            validate_file_path_proposal(
                &path_proposal,
                &[path_candidate],
                &config,
                &["auth.rs".to_string()]
            )
            .unwrap_err()
            .contains("allowlisted")
        );
    }

    #[test]
    fn path_inventory_is_unique_batched_and_audited() {
        let shared = FilePathCandidate {
            path_id: "path-src".to_string(),
            path: "src".to_string(),
            component_index: 0,
            kind: "directory".to_string(),
            value: "src".to_string(),
        };
        let a = FilePathCandidate {
            path_id: "path-a".to_string(),
            path: "src/a.rs".to_string(),
            component_index: 1,
            kind: "filename_stem".to_string(),
            value: "a".to_string(),
        };
        let b = FilePathCandidate {
            path_id: "path-b".to_string(),
            path: "src/b.rs".to_string(),
            component_index: 1,
            kind: "filename_stem".to_string(),
            value: "b".to_string(),
        };
        let selected = BTreeSet::from(["src/a.rs".to_string(), "src/b.rs".to_string()]);
        let paths = BTreeMap::from([
            ("src/a.rs".to_string(), vec![shared.clone(), a]),
            ("src/b.rs".to_string(), vec![shared, b]),
        ]);
        let (inventory, owners) = unique_path_inventory(&selected, &paths);
        assert_eq!(inventory.len(), 3);
        assert_eq!(owners["path-src"], "src/a.rs");
        let batches = path_proposal_batches(&inventory, 2);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].candidates.len(), 2);
        assert_eq!(batches[1].candidates.len(), 1);
        assert_eq!(batches[0].meta.total, 2);

        let mut eligible = semantic_candidate("sym-ready", "ready");
        let mut unresolved = semantic_candidate("sym-unresolved", "unresolved");
        unresolved.references_complete = false;
        let mut api = semantic_candidate("sym-api", "api");
        api.api_boundary = true;
        let mut aliased = semantic_candidate("sym-aliased", "aliased");
        aliased.existing_alias = Some("replacement".to_string());
        eligible.occurrence_lines = vec![1];
        let semantic = BTreeMap::from([(
            "src/a.rs".to_string(),
            vec![eligible, unresolved, api, aliased],
        )]);
        let pending = BTreeMap::from([("sym-ready".to_string(), "ready".to_string())]);
        let representatives = BTreeMap::new();
        let candidate_owners = semantic_candidate_owners(&selected, &semantic, &representatives);
        let audit = proposal_eligibility(
            &selected,
            &semantic,
            &paths,
            &pending,
            &representatives,
            &candidate_owners,
        );
        assert_eq!(audit.owned_symbols, 4);
        assert_eq!(audit.eligible_symbols, 1);
        assert_eq!(audit.pending_symbol_decisions, 1);
        assert_eq!(audit.excluded_unresolved, 1);
        assert_eq!(audit.excluded_api_boundary, 1);
        assert_eq!(audit.excluded_existing_alias, 1);
        assert_eq!(audit.unique_path_candidates, 3);
    }

    #[test]
    fn rejects_invalid_identifier_alias() {
        let config = Config::default();
        let proposal = Proposal {
            target: None,
            category: "identifier".to_string(),
            original_text: "helper".to_string(),
            sanitized_text: "1bad-name".to_string(),
            confidence: 1.0,
            rationale: None,
        };
        let verdict = validate_proposal(
            Path::new("src/lib.rs"),
            &proposal,
            "fn helper() {}",
            &config,
        );
        assert!(verdict.is_err());
    }

    #[test]
    fn proposal_comment_mask_preserves_code_strings_and_offsets() {
        let content = "// TOCTOU and кириллица\nconst char *url = \"https://example.test\";\n/* fake */ int helper = 1;\n";
        let masked = mask_comments_for_proposal(Path::new("src/main.mm"), content);

        assert_eq!(masked.len(), content.len());
        assert_eq!(masked.lines().count(), content.lines().count());
        assert!(!masked.contains("TOCTOU"));
        assert!(!masked.contains("кириллица"));
        assert!(!masked.contains("fake"));
        assert!(masked.contains("https://example.test"));
        assert!(masked.contains("int helper = 1"));
    }

    #[test]
    fn rejects_comment_only_terms_regardless_of_claimed_category() {
        let config = Config::default();
        let mut proposal = Proposal {
            target: None,
            category: "identifier".to_string(),
            original_text: "TOCTOU".to_string(),
            sanitized_text: "race_guard".to_string(),
            confidence: 1.0,
            rationale: None,
        };
        let comment_only = "// TOCTOU protection\nint helper = 1;\n";
        let reason = validate_proposal(Path::new("src/main.cpp"), &proposal, comment_only, &config)
            .unwrap_err();
        assert!(reason.contains("only inside comments"), "{reason}");

        proposal.category = "comment".to_string();
        let reason = validate_proposal(
            Path::new("src/main.cpp"),
            &proposal,
            "int TOCTOU = 1;\n",
            &config,
        )
        .unwrap_err();
        assert!(reason.contains("out of scope"), "{reason}");
    }

    #[test]
    fn allows_terms_that_also_occur_outside_comments() {
        let proposal = Proposal {
            target: None,
            category: "identifier".to_string(),
            original_text: "TOCTOU".to_string(),
            sanitized_text: "race_guard".to_string(),
            confidence: 1.0,
            rationale: None,
        };
        let verdict = validate_proposal(
            Path::new("src/main.cpp"),
            &proposal,
            "// TOCTOU protection\nint TOCTOU = 1;\n",
            &Config::default(),
        );
        assert!(verdict.is_ok(), "{verdict:?}");
    }

    #[test]
    fn rejects_alias_containing_denylisted_term() {
        let config = config_with_denylist(&["secret"]);
        let proposal = Proposal {
            target: None,
            category: "comment".to_string(),
            original_text: "widget".to_string(),
            sanitized_text: "secret_widget".to_string(),
            confidence: 1.0,
            rationale: None,
        };
        assert!(
            validate_proposal(
                Path::new("src/lib.rs"),
                &proposal,
                "// widget here",
                &config
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_terms_that_already_have_a_deterministic_mapping() {
        let config = Config::default();
        let proposal = Proposal {
            target: None,
            category: "identifier".to_string(),
            original_text: "dangerous".to_string(),
            sanitized_text: "another_alias".to_string(),
            confidence: 1.0,
            rationale: None,
        };
        let reason = validate_proposal(
            Path::new("src/lib.rs"),
            &proposal,
            "fn dangerous() {}",
            &config,
        )
        .unwrap_err();
        assert!(reason.contains("deterministic mapping"), "{reason}");
    }

    #[test]
    fn indexed_external_api_overlap_is_a_file_local_review_warning() {
        let config = Config::default();
        let external = [(
            "src/main.mm".to_string(),
            [
                "Security".to_string(),
                "trezor_interface_js_data".to_string(),
            ]
            .into_iter()
            .collect(),
        )]
        .into_iter()
        .collect();
        let indexed_words = BTreeMap::new();
        let semantic_candidates = BTreeMap::new();
        let semantic_alias_representatives = BTreeMap::new();
        let path_candidates = BTreeMap::new();
        let tracked_files = Vec::new();
        let policy = ProposalPolicyContext {
            config: &config,
            indexed_external: &external,
            indexed_words: &indexed_words,
            semantic_candidates: &semantic_candidates,
            semantic_alias_representatives: &semantic_alias_representatives,
            path_candidates: &path_candidates,
            tracked_files: &tracked_files,
        };
        for (candidate, content) in [
            ("SecurityAgent", "int SecurityAgent = 1;"),
            ("Trezor", "int Trezor = 1;"),
        ] {
            let proposal = Proposal {
                target: None,
                category: "identifier".to_string(),
                original_text: candidate.to_string(),
                sanitized_text: "ExternalComponent".to_string(),
                confidence: 0.99,
                rationale: None,
            };
            let flag = validate_proposal_with_index(
                Path::new("src/main.mm"),
                &proposal,
                content,
                &[],
                &policy,
            )
            .unwrap();
            assert!(flag.contains("file-local external identifier"), "{flag}");
        }

        assert!(external_api_owner("files_to_grab", &BTreeSet::from(["file".into()])).is_none());
    }

    #[test]
    fn indexed_repo_words_reject_cross_file_alias_collisions() {
        let config = Config::default();
        let proposal = Proposal {
            target: None,
            category: "identifier".to_string(),
            original_text: "shadowfax".to_string(),
            sanitized_text: "gadget".to_string(),
            confidence: 0.99,
            rationale: None,
        };
        let indexed_words = [(
            normalize_term("gadget"),
            ("src/other.rs".to_string(), "Gadget".to_string()),
        )]
        .into_iter()
        .collect();
        let external = BTreeMap::new();
        let semantic_candidates = BTreeMap::new();
        let semantic_alias_representatives = BTreeMap::new();
        let path_candidates = BTreeMap::new();
        let tracked_files = Vec::new();
        let policy = ProposalPolicyContext {
            config: &config,
            indexed_external: &external,
            indexed_words: &indexed_words,
            semantic_candidates: &semantic_candidates,
            semantic_alias_representatives: &semantic_alias_representatives,
            path_candidates: &path_candidates,
            tracked_files: &tracked_files,
        };
        let reason = validate_proposal_with_index(
            Path::new("src/main.rs"),
            &proposal,
            "fn shadowfax() {}",
            &[],
            &policy,
        )
        .unwrap_err();
        assert!(reason.contains("src/other.rs"), "{reason}");
        assert!(reason.contains("Gadget"), "{reason}");
    }

    #[test]
    fn proposal_path_can_select_an_indexed_directory() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(repo.path().join("src/a.rs"), "let shadowfax = 1;\n").unwrap();
        std::fs::write(repo.path().join("src/b.rs"), "let shadowfax = 2;\n").unwrap();
        std::fs::write(repo.path().join("outside.rs"), "let shadowfax = 3;\n").unwrap();
        crate::index::index_workspace(repo.path()).unwrap();

        let layout = Layout::new(repo.path());
        let mut config = Config::load_or_default(&layout).unwrap();
        config.sanitizer.denylist = vec!["shadowfax".to_string()];
        config.save(&layout).unwrap();

        let report = propose_sanitize(
            repo.path(),
            Some(Path::new("src")),
            ProviderAllow::default(),
        )
        .unwrap();
        assert_eq!(report.queued, 2);
        assert_eq!(report.duplicates, 0);
        let items = list_review(repo.path(), false).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].file, "src/a.rs");
        assert_eq!(items[1].file, "src/b.rs");
    }

    #[test]
    fn nested_javascript_function_review_approves_without_a_language_server() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("src/device.js"),
            "(() => { function encGetEntropy() { return 7; } use(encGetEntropy()); })();\n",
        )
        .unwrap();
        crate::index_workspace(repo.path()).unwrap();
        let layout = Layout::new(repo.path());
        let conn = db::connect(&layout).unwrap();
        let (symbol_id, occurrence_id): (String, String) = conn
            .query_row(
                r#"
                select symbol.symbol_id, occurrence.occurrence_id
                from semantic_symbols symbol
                join semantic_occurrences occurrence on occurrence.symbol_id = symbol.symbol_id
                where symbol.rel_path = 'src/device.js'
                  and symbol.name = 'encGetEntropy'
                  and occurrence.role = 'declaration'
                "#,
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        drop(conn);
        let id = "nested-js-review";
        let item = ReviewItem {
            id: id.to_string(),
            file: "src/device.js".to_string(),
            proposal: Proposal {
                target: Some(ProposalTarget::Semantic(SemanticProposalTarget {
                    symbol_id,
                    occurrence_id,
                })),
                category: "identifier".to_string(),
                original_text: "encGetEntropy".to_string(),
                sanitized_text: "encodeRequest".to_string(),
                confidence: 1.0,
                rationale: Some("nested JavaScript binding regression".to_string()),
            },
            status: ReviewStatus::Pending,
            flag: "clean".to_string(),
            created_at: Utc::now().to_rfc3339(),
        };
        std::fs::create_dir_all(&layout.review_dir).unwrap();
        crate::fsutil::atomic_write(
            &layout.review_dir.join(format!("{id}.json")),
            &serde_json::to_string_pretty(&item).unwrap(),
        )
        .unwrap();

        assert!(
            prepare_compiler_proposal_resolution(repo.path(), id)
                .unwrap()
                .is_none(),
            "nested binding must use its syntax-proven callable closure"
        );
        let approved = resolve_review(repo.path(), id, true).unwrap();
        assert_eq!(approved.status, ReviewStatus::Approved);
        let mirror = crate::read_sanitized_file(repo.path(), Path::new("src/device.js")).unwrap();
        assert_eq!(mirror.matches("encodeRequest").count(), 2);
        assert!(!mirror.contains("encGetEntropy"));
    }

    #[test]
    fn batch_approval_retires_incompatible_alias_for_the_same_target() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("src/device.js"),
            "(() => { function updateWatcherPersist() { return 7; } use(updateWatcherPersist()); })();\n",
        )
        .unwrap();
        crate::index_workspace(repo.path()).unwrap();
        let layout = Layout::new(repo.path());
        let conn = db::connect(&layout).unwrap();
        let (symbol_id, occurrence_id): (String, String) = conn
            .query_row(
                r#"
                select symbol.symbol_id, occurrence.occurrence_id
                from semantic_symbols symbol
                join semantic_occurrences occurrence on occurrence.symbol_id = symbol.symbol_id
                where symbol.rel_path = 'src/device.js'
                  and symbol.name = 'updateWatcherPersist'
                  and occurrence.role = 'declaration'
                "#,
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        drop(conn);

        std::fs::create_dir_all(&layout.review_dir).unwrap();
        let mut ids = Vec::new();
        for (id, alias, confidence, created_at) in [
            (
                "canonical-watcher-alias",
                "saveWatcherState",
                0.95,
                "2026-01-01T00:00:00Z",
            ),
            (
                "conflicting-watcher-alias",
                "storeWatcherState",
                0.80,
                "2026-01-01T00:00:01Z",
            ),
        ] {
            let item = ReviewItem {
                id: id.to_string(),
                file: "src/device.js".to_string(),
                proposal: Proposal {
                    target: Some(ProposalTarget::Semantic(SemanticProposalTarget {
                        symbol_id: symbol_id.clone(),
                        occurrence_id: occurrence_id.clone(),
                    })),
                    category: "identifier".to_string(),
                    original_text: "updateWatcherPersist".to_string(),
                    sanitized_text: alias.to_string(),
                    confidence,
                    rationale: None,
                },
                status: ReviewStatus::Pending,
                flag: "clean".to_string(),
                created_at: created_at.to_string(),
            };
            crate::fsutil::atomic_write(
                &layout.review_dir.join(format!("{id}.json")),
                &serde_json::to_string_pretty(&item).unwrap(),
            )
            .unwrap();
            ids.push(id.to_string());
        }

        let approved = approve_reviews(repo.path(), &ids).unwrap();
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].id, "canonical-watcher-alias");
        let reviews = list_review(repo.path(), true).unwrap();
        let retired = reviews
            .iter()
            .find(|item| item.id == "conflicting-watcher-alias")
            .unwrap();
        assert_eq!(retired.status, ReviewStatus::Stale);
        assert!(
            retired
                .flag
                .contains("alternative alias for the same semantic target")
        );
        let mirror = crate::read_sanitized_file(repo.path(), Path::new("src/device.js")).unwrap();
        assert_eq!(mirror.matches("saveWatcherState").count(), 2);
        assert!(!mirror.contains("storeWatcherState"));
        assert!(!mirror.contains("updateWatcherPersist"));
    }

    #[test]
    fn batch_approval_refuses_pending_mirror_edits_before_mutation() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("src/device.js"),
            "(() => { function encGetEntropy() { return 7; } use(encGetEntropy()); })();\n",
        )
        .unwrap();
        crate::index_workspace(repo.path()).unwrap();
        let layout = Layout::new(repo.path());
        let conn = db::connect(&layout).unwrap();
        let (symbol_id, occurrence_id): (String, String) = conn
            .query_row(
                r#"
                select symbol.symbol_id, occurrence.occurrence_id
                from semantic_symbols symbol
                join semantic_occurrences occurrence on occurrence.symbol_id = symbol.symbol_id
                where symbol.rel_path = 'src/device.js'
                  and symbol.name = 'encGetEntropy'
                  and occurrence.role = 'declaration'
                "#,
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        drop(conn);
        let id = "pending-mirror-review";
        let item = ReviewItem {
            id: id.to_string(),
            file: "src/device.js".to_string(),
            proposal: Proposal {
                target: Some(ProposalTarget::Semantic(SemanticProposalTarget {
                    symbol_id,
                    occurrence_id,
                })),
                category: "identifier".to_string(),
                original_text: "encGetEntropy".to_string(),
                sanitized_text: "encodeRequest".to_string(),
                confidence: 1.0,
                rationale: None,
            },
            status: ReviewStatus::Pending,
            flag: "clean".to_string(),
            created_at: Utc::now().to_rfc3339(),
        };
        std::fs::create_dir_all(&layout.review_dir).unwrap();
        crate::fsutil::atomic_write(
            &layout.review_dir.join(format!("{id}.json")),
            &serde_json::to_string_pretty(&item).unwrap(),
        )
        .unwrap();
        let mirror_path = layout.mirror_dir.join("src/device.js");
        let mut mirror = std::fs::read(&mirror_path).unwrap();
        mirror.extend_from_slice(b"// pending agent edit\n");
        std::fs::write(&mirror_path, mirror).unwrap();

        let error = approve_reviews(repo.path(), &[id.to_string()]).unwrap_err();
        assert!(
            error.to_string().contains("pending agent edits"),
            "{error:#}"
        );
        let review = list_review(repo.path(), true)
            .unwrap()
            .into_iter()
            .find(|item| item.id == id)
            .unwrap();
        assert_eq!(review.status, ReviewStatus::Pending);
        let conn = db::connect(&layout).unwrap();
        let aliases: i64 = conn
            .query_row("select count(*) from semantic_aliases", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(aliases, 0);
    }

    #[test]
    fn prune_resolved_reviews_never_touches_pending() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        std::fs::create_dir_all(&layout.review_dir).unwrap();
        let write_item = |id: &str, status: ReviewStatus| {
            let item = ReviewItem {
                id: id.to_string(),
                file: "src/a.rs".to_string(),
                proposal: Proposal {
                    target: None,
                    category: "identifier".to_string(),
                    original_text: "helper".to_string(),
                    sanitized_text: "assistant".to_string(),
                    confidence: 1.0,
                    rationale: None,
                },
                status,
                flag: "clean".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
            };
            std::fs::write(
                layout.review_dir.join(format!("{id}.json")),
                serde_json::to_string(&item).unwrap(),
            )
            .unwrap();
        };
        write_item("01", ReviewStatus::Pending);
        write_item("02", ReviewStatus::Approved);
        write_item("03", ReviewStatus::Rejected);
        write_item("04", ReviewStatus::Approved);

        assert_eq!(prune_resolved_reviews(&layout, 0).unwrap(), 0);
        assert_eq!(prune_resolved_reviews(&layout, 1).unwrap(), 2);
        // The oldest resolved items went; the pending one stays regardless.
        assert!(layout.review_dir.join("01.json").exists());
        assert!(!layout.review_dir.join("02.json").exists());
        assert!(!layout.review_dir.join("03.json").exists());
        assert!(layout.review_dir.join("04.json").exists());
    }

    #[test]
    fn batch_approval_retires_lower_confidence_alias_collision() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("src/lib.rs"),
            "fn one() { let first_private = 1; let _ = first_private; }\n\
             fn two() { let second_private = 2; let _ = second_private; }\n",
        )
        .unwrap();
        crate::index_workspace(repo.path()).unwrap();
        let layout = Layout::new(repo.path());
        let conn = db::connect(&layout).unwrap();
        let target = |name: &str| {
            conn.query_row(
                r#"
                select symbol.symbol_id, occurrence.occurrence_id
                from semantic_symbols symbol
                join semantic_occurrences occurrence
                  on occurrence.symbol_id = symbol.symbol_id
                 and occurrence.role = 'declaration'
                where symbol.name = ?1 limit 1
                "#,
                [name],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .unwrap()
        };
        std::fs::create_dir_all(&layout.review_dir).unwrap();
        let mut ids = Vec::new();
        for (id, name) in [
            ("batch-one", "first_private"),
            ("batch-two", "second_private"),
        ] {
            let (symbol_id, occurrence_id) = target(name);
            let item = ReviewItem {
                id: id.to_string(),
                file: "src/lib.rs".to_string(),
                proposal: Proposal {
                    target: Some(ProposalTarget::Semantic(SemanticProposalTarget {
                        symbol_id,
                        occurrence_id,
                    })),
                    category: "identifier".to_string(),
                    original_text: name.to_string(),
                    sanitized_text: "neutral_value".to_string(),
                    confidence: if id == "batch-one" { 1.0 } else { 0.8 },
                    rationale: None,
                },
                status: ReviewStatus::Pending,
                flag: "clean".to_string(),
                created_at: Utc::now().to_rfc3339(),
            };
            crate::fsutil::atomic_write(
                &layout.review_dir.join(format!("{id}.json")),
                &serde_json::to_string_pretty(&item).unwrap(),
            )
            .unwrap();
            ids.push(id.to_string());
        }
        drop(conn);

        let approved = approve_reviews(repo.path(), &ids).unwrap();
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].id, "batch-one");
        let conn = db::connect(&layout).unwrap();
        let aliases: i64 = conn
            .query_row("select count(*) from semantic_aliases", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            aliases, 1,
            "only the canonical alias mapping should be accepted"
        );
        assert!(list_review(repo.path(), false).unwrap().is_empty());
        let all = list_review(repo.path(), true).unwrap();
        let retired = all.iter().find(|item| item.id == "batch-two").unwrap();
        assert_eq!(retired.status, ReviewStatus::Stale);
        assert!(retired.flag.contains("claimed by different originals"));
    }

    #[test]
    fn batch_approval_arbitrates_aliases_across_path_and_semantic_targets() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("src/securevault.rs"),
            "fn main() { let private_scope = 1; let _ = private_scope; }\n",
        )
        .unwrap();
        crate::index_workspace(repo.path()).unwrap();
        let layout = Layout::new(repo.path());
        let mut config = Config::load_or_default(&layout).unwrap();
        config.sanitizer.dictionary.clear();
        config.sanitizer.denylist.clear();
        config.sanitizer.alias_registry.clear();
        config.sanitizer.path_alias_registry.clear();
        config.save(&layout).unwrap();

        let conn = db::connect(&layout).unwrap();
        let (symbol_id, occurrence_id) = conn
            .query_row(
                r#"
                select symbol.symbol_id, occurrence.occurrence_id
                from semantic_symbols symbol
                join semantic_occurrences occurrence
                  on occurrence.symbol_id = symbol.symbol_id
                 and occurrence.role = 'declaration'
                where symbol.name = 'private_scope' limit 1
                "#,
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .unwrap();
        let projection =
            crate::path_projection::PathProjection::from_connection(&config, &conn).unwrap();
        let path_target = file_path_candidates("src/securevault.rs", &projection)
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.kind == "filename_stem")
            .unwrap();
        drop(conn);

        std::fs::create_dir_all(&layout.review_dir).unwrap();
        let shared_alias = "shared_neutral_alias";
        let path_item = ReviewItem {
            id: "path-winner".to_string(),
            file: "src/securevault.rs".to_string(),
            proposal: Proposal {
                target: Some(ProposalTarget::FilePath(FilePathProposalTarget {
                    path_id: path_target.path_id,
                })),
                category: "file_path".to_string(),
                original_text: "securevault".to_string(),
                sanitized_text: shared_alias.to_string(),
                confidence: 0.95,
                rationale: None,
            },
            status: ReviewStatus::Pending,
            flag: "clean".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let semantic_item = ReviewItem {
            id: "semantic-loser".to_string(),
            file: "src/securevault.rs".to_string(),
            proposal: Proposal {
                target: Some(ProposalTarget::Semantic(SemanticProposalTarget {
                    symbol_id,
                    occurrence_id,
                })),
                category: "identifier".to_string(),
                original_text: "private_scope".to_string(),
                sanitized_text: shared_alias.to_string(),
                confidence: 0.85,
                rationale: None,
            },
            status: ReviewStatus::Pending,
            flag: "clean".to_string(),
            created_at: "2026-01-01T00:00:01Z".to_string(),
        };
        for item in [&path_item, &semantic_item] {
            crate::fsutil::atomic_write(
                &layout.review_dir.join(format!("{}.json", item.id)),
                &serde_json::to_string_pretty(item).unwrap(),
            )
            .unwrap();
        }

        let approved = approve_reviews(
            repo.path(),
            &[path_item.id.clone(), semantic_item.id.clone()],
        )
        .unwrap();
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].id, path_item.id);
        let config = Config::load_or_default(&layout).unwrap();
        assert_eq!(
            config.sanitizer.path_alias_registry.get("securevault"),
            Some(&shared_alias.to_string())
        );
        let all = list_review(repo.path(), true).unwrap();
        let retired = all.iter().find(|item| item.id == semantic_item.id).unwrap();
        assert_eq!(retired.status, ReviewStatus::Stale);
        assert!(retired.flag.contains("claimed by different originals"));
    }

    #[test]
    fn successful_bulk_approval_uses_one_alias_revision_and_one_mirror_refresh() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("src/device.js"),
            "(() => { function firstPrivate() { return 1; } function secondPrivate() { return firstPrivate(); } use(secondPrivate()); })();\n",
        )
        .unwrap();
        crate::index_workspace(repo.path()).unwrap();
        let layout = Layout::new(repo.path());
        let conn = db::connect(&layout).unwrap();
        let base_revision = crate::semantic_store::current_revision(&conn).unwrap();
        let target = |name: &str| {
            conn.query_row(
                r#"
                select symbol.symbol_id, occurrence.occurrence_id
                from semantic_symbols symbol
                join semantic_occurrences occurrence
                  on occurrence.symbol_id = symbol.symbol_id
                 and occurrence.role = 'declaration'
                where symbol.name = ?1 limit 1
                "#,
                [name],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .unwrap()
        };
        std::fs::create_dir_all(&layout.review_dir).unwrap();
        let mut ids = Vec::new();
        for (id, name, alias) in [
            ("bulk-first", "firstPrivate", "loadPrimaryValue"),
            ("bulk-second", "secondPrivate", "loadSecondaryValue"),
        ] {
            let (symbol_id, occurrence_id) = target(name);
            let item = ReviewItem {
                id: id.to_string(),
                file: "src/device.js".to_string(),
                proposal: Proposal {
                    target: Some(ProposalTarget::Semantic(SemanticProposalTarget {
                        symbol_id,
                        occurrence_id,
                    })),
                    category: "identifier".to_string(),
                    original_text: name.to_string(),
                    sanitized_text: alias.to_string(),
                    confidence: 1.0,
                    rationale: None,
                },
                status: ReviewStatus::Pending,
                flag: "clean".to_string(),
                created_at: Utc::now().to_rfc3339(),
            };
            crate::fsutil::atomic_write(
                &layout.review_dir.join(format!("{id}.json")),
                &serde_json::to_string_pretty(&item).unwrap(),
            )
            .unwrap();
            ids.push(id.to_string());
        }
        drop(conn);

        let mut progress = Vec::new();
        let approved =
            approve_reviews_with_progress(repo.path(), &ids, |event| progress.push(event.detail))
                .unwrap();
        assert_eq!(approved.len(), 2);
        let conn = db::connect(&layout).unwrap();
        assert_eq!(
            crate::semantic_store::current_revision(&conn).unwrap(),
            base_revision + 1,
            "bulk semantic aliases must share one transaction/revision"
        );
        let mirror = std::fs::read_to_string(layout.mirror_dir.join("src/device.js")).unwrap();
        assert_eq!(mirror.matches("loadPrimaryValue").count(), 2);
        assert_eq!(mirror.matches("loadSecondaryValue").count(), 2);
        assert!(!mirror.contains("firstPrivate"));
        assert!(!mirror.contains("secondPrivate"));
        let ledger = load_decision_ledger(&layout).unwrap();
        assert_eq!(ledger.decisions.len(), 2);
        assert!(
            progress
                .iter()
                .any(|detail| detail == "refreshing sanitized mirror once")
        );
    }

    #[test]
    fn bulk_approval_retires_an_obsolete_selected_alias_and_continues() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("src/device.js"),
            "(() => { function firstPrivate() { return 1; } function secondPrivate() { return firstPrivate(); } use(secondPrivate()); })();\n",
        )
        .unwrap();
        crate::index_workspace(repo.path()).unwrap();
        let layout = Layout::new(repo.path());
        let conn = db::connect(&layout).unwrap();
        let target = |name: &str| {
            conn.query_row(
                r#"
                select symbol.symbol_id, occurrence.occurrence_id
                from semantic_symbols symbol
                join semantic_occurrences occurrence
                  on occurrence.symbol_id = symbol.symbol_id
                 and occurrence.role = 'declaration'
                where symbol.name = ?1 limit 1
                "#,
                [name],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .unwrap()
        };
        let first = target("firstPrivate");
        let second = target("secondPrivate");
        drop(conn);
        std::fs::create_dir_all(&layout.review_dir).unwrap();
        let write_review = |id: &str, original: &str, alias: &str, target: &(String, String)| {
            let item = ReviewItem {
                id: id.to_string(),
                file: "src/device.js".to_string(),
                proposal: Proposal {
                    target: Some(ProposalTarget::Semantic(SemanticProposalTarget {
                        symbol_id: target.0.clone(),
                        occurrence_id: target.1.clone(),
                    })),
                    category: "identifier".to_string(),
                    original_text: original.to_string(),
                    sanitized_text: alias.to_string(),
                    confidence: 1.0,
                    rationale: None,
                },
                status: ReviewStatus::Pending,
                flag: "clean".to_string(),
                created_at: Utc::now().to_rfc3339(),
            };
            crate::fsutil::atomic_write(
                &layout.review_dir.join(format!("{id}.json")),
                &serde_json::to_string_pretty(&item).unwrap(),
            )
            .unwrap();
        };

        write_review("accepted-first", "firstPrivate", "loadPrimaryValue", &first);
        resolve_review(repo.path(), "accepted-first", true).unwrap();
        write_review(
            "obsolete-first",
            "firstPrivate",
            "loadDifferentValue",
            &first,
        );
        write_review(
            "fresh-second",
            "secondPrivate",
            "loadSecondaryValue",
            &second,
        );

        let mut progress = Vec::new();
        let approved = approve_reviews_with_progress(
            repo.path(),
            &["obsolete-first".to_string(), "fresh-second".to_string()],
            |event| progress.push(event.detail),
        )
        .unwrap();
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].id, "fresh-second");
        let all = list_review(repo.path(), true).unwrap();
        let obsolete = all.iter().find(|item| item.id == "obsolete-first").unwrap();
        assert_eq!(obsolete.status, ReviewStatus::Stale);
        assert!(obsolete.flag.contains("already has accepted alias"));
        assert!(
            progress
                .iter()
                .any(|detail| detail == "retired 1 selected proposals during preflight")
        );
        let mirror = std::fs::read_to_string(layout.mirror_dir.join("src/device.js")).unwrap();
        assert!(mirror.contains("loadPrimaryValue"));
        assert!(mirror.contains("loadSecondaryValue"));
        assert!(!mirror.contains("loadDifferentValue"));
    }

    #[test]
    fn one_semantic_target_requires_one_exact_alias_spelling() {
        let mut selected_aliases = BTreeMap::new();
        let mut selected_targets = BTreeMap::new();
        register_selected_alias(
            &mut selected_aliases,
            &mut selected_targets,
            "retrieve_saved_secret",
            "semantic:sym_password",
            Some("get_stored_password"),
            "header proposal",
        )
        .unwrap();
        register_selected_alias(
            &mut selected_aliases,
            &mut selected_targets,
            "retrieve_saved_secret",
            "semantic:sym_password",
            Some("get_stored_password"),
            "implementation proposal",
        )
        .unwrap();

        let error = register_selected_alias(
            &mut selected_aliases,
            &mut selected_targets,
            "RetrieveSavedSecret",
            "semantic:sym_password",
            Some("get_stored_password"),
            "incompatible implementation proposal",
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("incompatible aliases"),
            "{error:#}"
        );

        register_selected_alias(
            &mut selected_aliases,
            &mut selected_targets,
            "retrieve_saved_secret",
            "semantic:sym_other",
            Some("get_stored_password"),
            "same textual mapping on an independent platform target",
        )
        .unwrap();

        let error = register_selected_alias(
            &mut selected_aliases,
            &mut selected_targets,
            "retrieve_saved_secret",
            "semantic:sym_unrelated",
            Some("read_browser_cookie"),
            "different semantic target and original spelling",
        )
        .unwrap_err();
        assert!(error.to_string().contains("different targets"), "{error:#}");
    }

    #[test]
    fn cpp_declarator_canonicalization_expands_namespace_aliases_and_ignores_names() {
        let implementation = "namespace fs = std::filesystem;\nbool f(const fs::path& /* app_path */, unsigned count = 4) const;\n";
        let masked = mask_cpp_signature_source(implementation);
        let aliases = cpp_namespace_aliases(&masked);
        assert_eq!(
            aliases.get("fs").map(String::as_str),
            Some("std::filesystem")
        );
        let implementation_tail = masked
            .split_once("bool f")
            .map(|(_, tail)| tail.trim_end_matches(";\n"))
            .unwrap();
        let header_tail = "(const std::filesystem::path& app_path, unsigned) const";
        assert_eq!(
            canonical_cpp_declarator_tail(implementation_tail, &aliases),
            canonical_cpp_declarator_tail(header_tail, &BTreeMap::new())
        );
        assert_eq!(
            canonical_cpp_declarator_tail(header_tail, &BTreeMap::new()),
            "(conststd::filesystem::path&,unsigned)const"
        );
    }

    #[test]
    fn prepared_compiler_closure_unifies_header_and_definition_alias_owners() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("src/encryption.hpp"),
            "int get_stored_password();\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("src/encryption.cpp"),
            "#include \"encryption.hpp\"\nint get_stored_password() { return 7; }\n",
        )
        .unwrap();
        crate::index_workspace(repo.path()).unwrap();
        let layout = Layout::new(repo.path());
        let conn = db::connect(&layout).unwrap();
        let mut statement = conn
            .prepare(
                "select symbol_id, rel_path from semantic_symbols \
                 where name = 'get_stored_password' order by rel_path",
            )
            .unwrap();
        let targets = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(targets.len(), 2);
        let locations = targets
            .iter()
            .take(1)
            .map(|(symbol_id, rel_path)| crate::lsp::LspLocation {
                rel_path: rel_path.clone(),
                range: crate::semantic_store::load_symbol(&conn, symbol_id)
                    .unwrap()
                    .unwrap()
                    .range,
            })
            .collect::<Vec<_>>();
        let prepared = vec![Some(PreparedCompilerProposalResolution {
            symbol_id: targets[0].0.clone(),
            document_fingerprint: crate::semantic_store::document_fingerprint(&conn).unwrap(),
            provider: "test-clangd".to_string(),
            locations,
            equivalent_symbol_ids: BTreeSet::new(),
        })];
        let representatives =
            semantic_alias_representatives(repo.path(), &conn, &prepared).unwrap();
        assert_eq!(
            representatives.get(&targets[0].0),
            representatives.get(&targets[1].0)
        );

        let proposal = |symbol_id: &str| Proposal {
            target: Some(ProposalTarget::Semantic(SemanticProposalTarget {
                symbol_id: symbol_id.to_string(),
                occurrence_id: "unused".to_string(),
            })),
            category: "identifier".to_string(),
            original_text: "get_stored_password".to_string(),
            sanitized_text: "retrieve_saved_secret".to_string(),
            confidence: 1.0,
            rationale: None,
        };
        assert_eq!(
            approval_alias_identity(&proposal(&targets[0].0), &representatives),
            approval_alias_identity(&proposal(&targets[1].0), &representatives)
        );
        let selected = targets
            .iter()
            .map(|(_, rel_path)| rel_path.clone())
            .collect::<BTreeSet<_>>();
        let candidates = targets
            .iter()
            .map(|(symbol_id, rel_path)| {
                (
                    rel_path.clone(),
                    vec![semantic_candidate(symbol_id, "get_stored_password")],
                )
            })
            .collect::<BTreeMap<_, _>>();
        let owners = semantic_candidate_owners(&selected, &candidates, &representatives);
        let implementation = targets
            .iter()
            .find(|(_, rel_path)| rel_path.ends_with(".cpp"))
            .unwrap();
        assert_eq!(owners, BTreeSet::from([implementation.0.clone()]));
    }

    #[test]
    fn cpp_syntax_equivalence_expands_namespace_aliases_across_files() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("src/update.hpp"),
            "#include <filesystem>\nbool app_needs_repatch(const std::filesystem::path& app_path);\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("src/update.mm"),
            "#include \"update.hpp\"\nnamespace fs = std::filesystem;\nbool app_needs_repatch(const fs::path& /* app_path */) { return true; }\n",
        )
        .unwrap();
        crate::index_workspace(repo.path()).unwrap();
        let layout = Layout::new(repo.path());
        let conn = db::connect(&layout).unwrap();
        let mut statement = conn
            .prepare(
                "select symbol_id, rel_path from semantic_symbols \
                 where name = 'app_needs_repatch' order by rel_path",
            )
            .unwrap();
        let targets = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        drop(statement);
        assert_eq!(targets.len(), 2);
        let implementation = targets
            .iter()
            .find(|(_, path)| path.ends_with(".mm"))
            .unwrap();
        let prepared = vec![Some(PreparedCompilerProposalResolution {
            symbol_id: implementation.0.clone(),
            document_fingerprint: crate::semantic_store::document_fingerprint(&conn).unwrap(),
            provider: "test-clangd".to_string(),
            locations: vec![crate::lsp::LspLocation {
                rel_path: implementation.1.clone(),
                range: crate::semantic_store::load_symbol(&conn, &implementation.0)
                    .unwrap()
                    .unwrap()
                    .range,
            }],
            equivalent_symbol_ids: BTreeSet::new(),
        })];
        let representatives =
            semantic_alias_representatives(repo.path(), &conn, &prepared).unwrap();
        assert_eq!(
            representatives.get(&targets[0].0),
            representatives.get(&targets[1].0),
            "namespace aliases and parameter spellings must not split one C++ target"
        );
    }

    #[test]
    fn bulk_approval_quarantines_an_incomplete_compiler_closure() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("src/scanner.cpp"),
            "int detect_framework(int value) { return value; }\nint use_framework() { return detect_framework(1); }\n",
        )
        .unwrap();
        crate::index_workspace(repo.path()).unwrap();
        let layout = Layout::new(repo.path());
        let conn = db::connect(&layout).unwrap();
        let (symbol_id, occurrence_id, range) = conn
            .query_row(
                r#"
                select symbol.symbol_id, occurrence.occurrence_id,
                       occurrence.start_byte, occurrence.end_byte,
                       occurrence.start_line, occurrence.start_column,
                       occurrence.end_line, occurrence.end_column
                from semantic_symbols symbol
                join semantic_occurrences occurrence
                  on occurrence.symbol_id = symbol.symbol_id
                 and occurrence.role = 'declaration'
                where symbol.name = 'detect_framework' limit 1
                "#,
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        crate::semantic::TextRange {
                            start_byte: row.get::<_, i64>(2)? as usize,
                            end_byte: row.get::<_, i64>(3)? as usize,
                            start_line: row.get::<_, i64>(4)? as usize,
                            start_column: row.get::<_, i64>(5)? as usize,
                            end_line: row.get::<_, i64>(6)? as usize,
                            end_column: row.get::<_, i64>(7)? as usize,
                        },
                    ))
                },
            )
            .unwrap();
        let item = ReviewItem {
            id: "incomplete-compiler-closure".to_string(),
            file: "src/scanner.cpp".to_string(),
            proposal: Proposal {
                target: Some(ProposalTarget::Semantic(SemanticProposalTarget {
                    symbol_id: symbol_id.clone(),
                    occurrence_id,
                })),
                category: "identifier".to_string(),
                original_text: "detect_framework".to_string(),
                sanitized_text: "classify_framework".to_string(),
                confidence: 1.0,
                rationale: None,
            },
            status: ReviewStatus::Pending,
            flag: "clean".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };
        std::fs::create_dir_all(&layout.review_dir).unwrap();
        crate::fsutil::atomic_write(
            &layout.review_dir.join(format!("{}.json", item.id)),
            &serde_json::to_string_pretty(&item).unwrap(),
        )
        .unwrap();
        let fingerprint = crate::semantic_store::document_fingerprint(&conn).unwrap();
        let prepared = vec![Some(PreparedCompilerProposalResolution {
            symbol_id,
            document_fingerprint: fingerprint.clone(),
            provider: "test-clangd".to_string(),
            locations: vec![crate::lsp::LspLocation {
                rel_path: item.file.clone(),
                range,
            }],
            equivalent_symbol_ids: BTreeSet::new(),
        })];
        let mut progress = Vec::new();
        let result = quarantine_unadmissible_compiler_resolutions(
            CompilerQuarantineContext {
                root: repo.path(),
                layout: &layout,
                conn: &conn,
                base_representatives: &BTreeMap::new(),
                current_fingerprint: &fingerprint,
            },
            vec![item.id.clone()],
            prepared,
            &mut |event| progress.push(event.detail),
        )
        .unwrap();
        assert!(result.ids.is_empty());
        assert_eq!(result.retired, 1);
        let retired = list_review(repo.path(), true)
            .unwrap()
            .into_iter()
            .find(|review| review.id == item.id)
            .unwrap();
        assert_eq!(retired.status, ReviewStatus::Stale);
        assert!(retired.flag.contains("compiler closure is incomplete"));
        assert!(retired.flag.contains("omitted indexed occurrence"));
        assert!(
            progress
                .iter()
                .any(|detail| detail.contains("incomplete compiler closures"))
        );
    }

    #[test]
    fn review_reconciliation_retires_duplicate_header_definition_proposals() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("src/encryption.hpp"),
            "int get_stored_password();\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("src/encryption.cpp"),
            "#include \"encryption.hpp\"\nint get_stored_password() { return 7; }\n",
        )
        .unwrap();
        crate::index_workspace(repo.path()).unwrap();

        let layout = Layout::new(repo.path());
        let conn = db::connect(&layout).unwrap();
        let mut statement = conn
            .prepare(
                r#"
                select symbol.symbol_id, symbol.rel_path, occurrence.occurrence_id
                from semantic_symbols symbol
                join semantic_occurrences occurrence
                  on occurrence.symbol_id = symbol.symbol_id
                 and occurrence.role = 'declaration'
                where symbol.name = 'get_stored_password'
                order by symbol.rel_path
                "#,
            )
            .unwrap();
        let targets = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(targets.len(), 2);
        drop(statement);
        drop(conn);

        std::fs::create_dir_all(&layout.review_dir).unwrap();
        for (index, (symbol_id, rel_path, occurrence_id)) in targets.iter().enumerate() {
            let id = format!("0{}-duplicate", index + 1);
            let item = ReviewItem {
                id: id.clone(),
                file: rel_path.clone(),
                proposal: Proposal {
                    target: Some(ProposalTarget::Semantic(SemanticProposalTarget {
                        symbol_id: symbol_id.clone(),
                        occurrence_id: occurrence_id.clone(),
                    })),
                    category: "identifier".to_string(),
                    original_text: "get_stored_password".to_string(),
                    sanitized_text: "retrieve_saved_secret".to_string(),
                    confidence: 1.0,
                    rationale: None,
                },
                status: ReviewStatus::Pending,
                flag: "clean".to_string(),
                created_at: Utc::now().to_rfc3339(),
            };
            crate::fsutil::atomic_write(
                &layout.review_dir.join(format!("{id}.json")),
                &serde_json::to_string_pretty(&item).unwrap(),
            )
            .unwrap();
            crate::semantic_store::record_proposal(
                &db::connect(&layout).unwrap(),
                &id,
                symbol_id,
                occurrence_id,
                &item.proposal.sanitized_text,
                &item.proposal.category,
                item.proposal.confidence,
                "",
                "pending",
                &item.created_at,
            )
            .unwrap();
        }

        assert_eq!(
            reconcile_review_queue_locked(repo.path(), &layout).unwrap(),
            1
        );
        let pending = list_review(repo.path(), false).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "01-duplicate");
        let all = list_review(repo.path(), true).unwrap();
        let duplicate = all.iter().find(|item| item.id == "02-duplicate").unwrap();
        assert_eq!(duplicate.status, ReviewStatus::Stale);
        assert!(duplicate.flag.contains("duplicate declaration/definition"));

        let conn = db::connect(&layout).unwrap();
        let revision = crate::semantic_store::current_revision(&conn).unwrap();
        conn.execute(
            r#"
            insert into semantic_aliases(
                symbol_id, original_name, sanitized_name, category, confidence,
                reason, status, source, created_revision
            ) values(?1, 'get_stored_password', 'load_persisted_value',
                     'identifier', 1.0, '', 'accepted', 'proposal-v2', ?2)
            "#,
            rusqlite::params![targets[1].0, revision as i64],
        )
        .unwrap();
        drop(conn);

        assert_eq!(
            reconcile_review_queue_locked(repo.path(), &layout).unwrap(),
            1
        );
        assert!(list_review(repo.path(), false).unwrap().is_empty());
        let all = list_review(repo.path(), true).unwrap();
        let obsolete = all.iter().find(|item| item.id == "01-duplicate").unwrap();
        assert_eq!(obsolete.status, ReviewStatus::Stale);
        assert!(obsolete.flag.contains("already has accepted alias"));
        assert!(obsolete.flag.contains("load_persisted_value"));
        assert!(obsolete.flag.contains("obsolete"));
    }

    #[test]
    fn cpp_syntax_fallback_does_not_merge_internal_linkage_functions() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("src/internal.hpp"),
            "static int hidden_helper(int value);\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("src/internal.cpp"),
            "static int hidden_helper(int value) { return value; }\n",
        )
        .unwrap();
        crate::index_workspace(repo.path()).unwrap();
        let layout = Layout::new(repo.path());
        let conn = db::connect(&layout).unwrap();
        let mut statement = conn
            .prepare(
                "select symbol_id, rel_path from semantic_symbols \
                 where name = 'hidden_helper' order by rel_path",
            )
            .unwrap();
        let targets = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(targets.len(), 2);
        let prepared = vec![Some(PreparedCompilerProposalResolution {
            symbol_id: targets[0].0.clone(),
            document_fingerprint: crate::semantic_store::document_fingerprint(&conn).unwrap(),
            provider: "test-clangd".to_string(),
            locations: vec![crate::lsp::LspLocation {
                rel_path: targets[0].1.clone(),
                range: crate::semantic_store::load_symbol(&conn, &targets[0].0)
                    .unwrap()
                    .unwrap()
                    .range,
            }],
            equivalent_symbol_ids: BTreeSet::new(),
        })];
        let representatives =
            semantic_alias_representatives(repo.path(), &conn, &prepared).unwrap();
        assert_ne!(
            representatives.get(&targets[0].0).unwrap_or(&targets[0].0),
            representatives.get(&targets[1].0).unwrap_or(&targets[1].0)
        );
    }

    #[test]
    fn legacy_header_definition_aliases_reconcile_to_header_contract() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("src/catalog.hpp"),
            "int known_standalone_wallets();\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("src/catalog.cpp"),
            "int known_standalone_wallets() { return 7; }\n",
        )
        .unwrap();
        crate::index_workspace(repo.path()).unwrap();
        let layout = Layout::new(repo.path());
        let mut conn = db::connect(&layout).unwrap();
        let mut statement = conn
            .prepare(
                "select symbol_id, rel_path from semantic_symbols \
                 where name = 'known_standalone_wallets' order by rel_path",
            )
            .unwrap();
        let targets = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        drop(statement);
        assert_eq!(targets.len(), 2);
        let header_symbol = targets
            .iter()
            .find(|(_, rel_path)| rel_path.ends_with(".hpp"))
            .unwrap()
            .0
            .clone();
        conn.execute(
            "insert into semantic_compiler_links(canonical_symbol_id, linked_symbol_id) values(?1, ?1)",
            [&header_symbol],
        )
        .unwrap();
        conn.execute(
            r#"
            insert into semantic_compiler_resolutions(
              canonical_symbol_id, provider, locations_fingerprint, resolved_revision
            ) values(?1, 'legacy-test', 'fingerprint', 1)
            "#,
            [&header_symbol],
        )
        .unwrap();
        for (symbol_id, rel_path) in &targets {
            let (alias, revision) = if rel_path.ends_with(".hpp") {
                ("known_source_catalog", 20_i64)
            } else {
                ("known_application_catalog", 5_i64)
            };
            conn.execute(
                r#"
                insert into semantic_aliases(
                    symbol_id, original_name, sanitized_name, category, confidence,
                    reason, status, source, created_revision
                ) values(?1, 'known_standalone_wallets', ?2, 'identifier', 1.0,
                         'legacy test', 'accepted', 'proposal-v2', ?3)
                "#,
                rusqlite::params![symbol_id, alias, revision],
            )
            .unwrap();
        }
        assert_eq!(
            reconcile_equivalent_semantic_aliases(repo.path(), &mut conn).unwrap(),
            2
        );
        let aliases = conn
            .prepare(
                "select sanitized_name from semantic_aliases \
                 where original_name = 'known_standalone_wallets' order by symbol_id",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            aliases,
            vec![
                "known_source_catalog".to_string(),
                "known_source_catalog".to_string()
            ]
        );
        for (symbol_id, _) in &targets {
            assert!(
                crate::semantic_store::symbol_projection_is_complete(&conn, symbol_id).unwrap()
            );
        }
    }

    #[test]
    fn proposal_alias_reservations_reject_real_collisions_but_flag_possible_cpp_pairs() {
        let proposal = |symbol_id: &str, original: &str| Proposal {
            target: Some(ProposalTarget::Semantic(SemanticProposalTarget {
                symbol_id: symbol_id.to_string(),
                occurrence_id: format!("occ-{symbol_id}"),
            })),
            category: "identifier".to_string(),
            original_text: original.to_string(),
            sanitized_text: "neutral_secret_reader".to_string(),
            confidence: 1.0,
            rationale: None,
        };
        let mut reservations = BTreeMap::new();
        let representatives = BTreeMap::new();
        assert_eq!(
            reserve_proposal_alias(
                &mut reservations,
                &proposal("header", "get_stored_password"),
                &representatives,
                "header proposal",
            )
            .unwrap(),
            None
        );
        assert!(
            reserve_proposal_alias(
                &mut reservations,
                &proposal("definition", "get_stored_password"),
                &representatives,
                "implementation proposal",
            )
            .unwrap()
            .unwrap()
            .contains("compiler identity")
        );
        assert!(
            reserve_proposal_alias(
                &mut reservations,
                &proposal("different", "read_browser_cookie"),
                &representatives,
                "different proposal",
            )
            .unwrap_err()
            .contains("workspace-unique")
        );
    }

    #[test]
    fn batch_preflight_retires_late_workspace_symbol_collision() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("src/a.rs"),
            "fn one() { let first_private = 1; let _ = first_private; }\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("src/b.rs"),
            "fn two() { let second_private = 2; let _ = second_private; }\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("src/c.rs"),
            "fn three() { let existing_neutral = 3; let _ = existing_neutral; }\n",
        )
        .unwrap();
        crate::index_workspace(repo.path()).unwrap();
        let layout = Layout::new(repo.path());
        let conn = db::connect(&layout).unwrap();
        let target = |name: &str| {
            conn.query_row(
                r#"
                select symbol.rel_path, symbol.symbol_id, occurrence.occurrence_id
                from semantic_symbols symbol
                join semantic_occurrences occurrence
                  on occurrence.symbol_id = symbol.symbol_id
                 and occurrence.role = 'declaration'
                where symbol.name = ?1 limit 1
                "#,
                [name],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .unwrap()
        };
        std::fs::create_dir_all(&layout.review_dir).unwrap();
        let mut ids = Vec::new();
        for (id, name, alias) in [
            ("batch-valid", "first_private", "neutral_first"),
            ("batch-collision", "second_private", "existing_neutral"),
        ] {
            let (file, symbol_id, occurrence_id) = target(name);
            let item = ReviewItem {
                id: id.to_string(),
                file,
                proposal: Proposal {
                    target: Some(ProposalTarget::Semantic(SemanticProposalTarget {
                        symbol_id,
                        occurrence_id,
                    })),
                    category: "identifier".to_string(),
                    original_text: name.to_string(),
                    sanitized_text: alias.to_string(),
                    confidence: 1.0,
                    rationale: None,
                },
                status: ReviewStatus::Pending,
                flag: "clean".to_string(),
                created_at: Utc::now().to_rfc3339(),
            };
            crate::fsutil::atomic_write(
                &layout.review_dir.join(format!("{id}.json")),
                &serde_json::to_string_pretty(&item).unwrap(),
            )
            .unwrap();
            ids.push(id.to_string());
        }
        drop(conn);

        let approved = approve_reviews(repo.path(), &ids).unwrap();
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].id, "batch-valid");
        let conn = db::connect(&layout).unwrap();
        let aliases: i64 = conn
            .query_row("select count(*) from semantic_aliases", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(aliases, 1);
        assert!(list_review(repo.path(), false).unwrap().is_empty());
        let all = list_review(repo.path(), true).unwrap();
        let retired = all
            .iter()
            .find(|item| item.id == "batch-collision")
            .unwrap();
        assert_eq!(retired.status, ReviewStatus::Stale);
        assert!(retired.flag.contains("existing source symbol name"));
    }

    #[test]
    fn low_confidence_is_queued_with_flag_not_rejected() {
        let config = Config::default();
        let proposal = Proposal {
            target: None,
            category: "identifier".to_string(),
            original_text: "helper".to_string(),
            sanitized_text: "assistant".to_string(),
            confidence: 0.3,
            rationale: None,
        };
        let flag = validate_proposal(
            Path::new("src/lib.rs"),
            &proposal,
            "fn helper() {}",
            &config,
        )
        .unwrap();
        assert!(flag.contains("confidence"));
    }

    #[test]
    fn file_path_target_json_is_distinct_and_backward_compatible() {
        let semantic: ProposalTarget = serde_json::from_value(serde_json::json!({
            "symbol_id": "sym_1",
            "occurrence_id": "occ_1"
        }))
        .unwrap();
        assert!(matches!(semantic, ProposalTarget::Semantic(_)));
        let path: ProposalTarget =
            serde_json::from_value(serde_json::json!({ "path_id": "path_1" })).unwrap();
        assert!(matches!(path, ProposalTarget::FilePath(_)));
        assert!(
            serde_json::from_value::<ProposalTarget>(serde_json::json!({
                "symbol_id": "sym_1",
                "path_id": "path_1"
            }))
            .is_err()
        );
    }

    #[test]
    fn file_path_alias_collision_is_rejected_before_review() {
        let mut config = Config::default();
        config.sanitizer.dictionary.clear();
        config.sanitizer.alias_registry.clear();
        config.sanitizer.path_alias_registry.clear();
        config.sanitizer.denylist.clear();
        let files = vec!["dangerous/a.rs".to_string(), "neutral/b.rs".to_string()];
        let projection =
            crate::path_projection::PathProjection::build(&config, files.iter()).unwrap();
        let candidates = file_path_candidates(&files[0], &projection).unwrap();
        let directory = candidates
            .iter()
            .find(|candidate| candidate.value == "dangerous")
            .unwrap();
        let proposal = Proposal {
            target: Some(ProposalTarget::FilePath(FilePathProposalTarget {
                path_id: directory.path_id.clone(),
            })),
            category: "file_path".to_string(),
            original_text: "dangerous".to_string(),
            sanitized_text: "neutral".to_string(),
            confidence: 0.99,
            rationale: None,
        };
        let reason =
            validate_file_path_proposal(&proposal, &candidates, &config, &files).unwrap_err();
        assert!(reason.contains("collapse two tracked"), "{reason}");
    }

    #[test]
    fn bulk_approval_retires_a_path_alias_that_became_sanitizable() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("src/update_watcher.cpp"),
            "int main() { return 0; }\n",
        )
        .unwrap();
        crate::index_workspace(repo.path()).unwrap();
        let layout = Layout::new(repo.path());
        let mut config = Config::load_or_default(&layout).unwrap();
        config.sanitizer.dictionary.clear();
        config.sanitizer.alias_registry.clear();
        config.sanitizer.path_alias_registry.clear();
        config.sanitizer.denylist = vec!["observe".to_string()];
        config.save(&layout).unwrap();
        let conn = db::connect(&layout).unwrap();
        let projection =
            crate::path_projection::PathProjection::from_connection(&config, &conn).unwrap();
        let target = file_path_candidates("src/update_watcher.cpp", &projection)
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.kind == "filename_stem")
            .unwrap();
        drop(conn);
        let item = ReviewItem {
            id: "invalid-path-alias".to_string(),
            file: "src/update_watcher.cpp".to_string(),
            proposal: Proposal {
                target: Some(ProposalTarget::FilePath(FilePathProposalTarget {
                    path_id: target.path_id,
                })),
                category: "file_path".to_string(),
                original_text: "update_watcher".to_string(),
                sanitized_text: "update_observer".to_string(),
                confidence: 1.0,
                rationale: None,
            },
            status: ReviewStatus::Pending,
            flag: "clean".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };
        std::fs::create_dir_all(&layout.review_dir).unwrap();
        crate::fsutil::atomic_write(
            &layout.review_dir.join("invalid-path-alias.json"),
            &serde_json::to_string_pretty(&item).unwrap(),
        )
        .unwrap();

        assert!(
            approve_reviews(repo.path(), std::slice::from_ref(&item.id))
                .unwrap()
                .is_empty()
        );
        let retired = list_review(repo.path(), true)
            .unwrap()
            .into_iter()
            .find(|review| review.id == item.id)
            .unwrap();
        assert_eq!(retired.status, ReviewStatus::Stale);
        assert!(retired.flag.contains("deterministic validation"));
        assert!(retired.flag.contains("sanitizable path term \"observe\""));
    }

    #[test]
    fn allowlisted_path_term_is_rejected_before_review() {
        let mut config = Config::default();
        config.sanitizer.dictionary.clear();
        config.sanitizer.alias_registry.clear();
        config.sanitizer.path_alias_registry.clear();
        config.sanitizer.denylist.clear();
        config.sanitizer.path_allowlist = vec!["weaponized".to_string()];
        let files = vec!["src/weaponized_loader.rs".to_string()];
        let projection =
            crate::path_projection::PathProjection::build(&config, files.iter()).unwrap();
        let candidates = file_path_candidates(&files[0], &projection).unwrap();
        let filename = candidates
            .iter()
            .find(|candidate| candidate.kind == "filename_stem")
            .unwrap();
        let proposal = Proposal {
            target: Some(ProposalTarget::FilePath(FilePathProposalTarget {
                path_id: filename.path_id.clone(),
            })),
            category: "file_path".to_string(),
            original_text: "weaponized".to_string(),
            sanitized_text: "neutral".to_string(),
            confidence: 0.99,
            rationale: None,
        };
        let reason =
            validate_file_path_proposal(&proposal, &candidates, &config, &files).unwrap_err();
        assert!(reason.contains("allowlisted"), "{reason}");
    }
}
