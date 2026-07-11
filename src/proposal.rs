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
use std::sync::mpsc;
use std::time::Instant;

/// A single sanitization proposal. This is the model-facing schema: a provider
/// returns these, the engine validates and (on approval) records them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
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

#[derive(Debug, Clone, Default, Serialize)]
pub struct ProposeReport {
    pub proposed: usize,
    pub queued: usize,
    /// Valid proposals already represented by a pending review item.
    pub duplicates: usize,
    pub rejected: Vec<String>,
    /// Per-file failures (read error, provider error): the run continues past
    /// them; only an all-files-failed run is a hard error.
    pub errors: Vec<String>,
    /// Files larger than `sanitizer.propose_max_file_bytes`, never sent.
    pub skipped: Vec<String>,
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
        let mut provider_config = config.clone();
        provider_config.sanitizer.denylist.clear();
        let provider_content =
            crate::redact::Redactor::terms_only(&provider_config).redact(content);
        let core_offset = proposal_core_offset(&provider_content, chunk);
        let (context_before, provider_content) = provider_content.split_at(core_offset);
        let user = serde_json::to_string(&serde_json::json!({
            "task": "Review this source file for two independent classes of lexical normalization candidates: (1) project-specific private organization, product, project, customer, and internal names; (2) security- or abuse-adjacent vocabulary used in a benign developer context that could be misclassified by an automated safety filter when read without context. Suggest clear context-neutral replacement terms while preserving the code's meaning.",
            "rules": [
                "original_text must be copied byte-for-byte from one contiguous case-sensitive substring of file.content",
                "Never change original_text capitalization, spelling, inflection, plurality, or separators; never join source tokens separated by whitespace or punctuation",
                "Never invent a label for a concept merely implied by the code; if the exact candidate is not present, omit it",
                "For security- or abuse-adjacent vocabulary, include terms associated with offensive-security behavior, credential or device identity abuse, evasion, injection, unauthorized automation, harmful payloads, or similar risk-loaded concepts when the file uses them benignly",
                "Do not suggest ordinary technical terms unless their standalone or compound meaning is plausibly risk-loaded and could be misclassified without context",
                "Do not suggest changes to imports, public APIs, protocol constants, SQL, or shell syntax",
                "Do not suggest any term listed in policy.allowlist",
                "Both original_text and sanitized_text must each be exactly one ASCII word run matching [A-Za-z0-9_]+; never return spaces, hyphens, dots, slashes, or punctuation",
                "For a candidate embedded in a larger identifier, return the smallest meaningful risk-loaded or private-name substring rather than the entire identifier",
                "Treat file.context_before only as read-only context; original_text must occur in file.content, which is the current chunk's owned analysis region",
                "Do not return an original_text already represented in context.already_proposed_originals, including capitalization or separator variants",
                "When category is identifier, sanitized_text must additionally match [A-Za-z_][A-Za-z0-9_]*",
                "Replacement text must not contain a term listed in policy.denylist",
                "Return strict JSON only, without prose or markdown"
            ],
            "required_output_preflight": [
                "For every proposal, verify file.content contains original_text using an exact case-sensitive substring check",
                "Remove every proposal that fails the exact substring check",
                "Remove every proposal already represented in context.already_proposed_originals",
                "Remove every proposal whose original_text or sanitized_text fails the single ASCII word-run rule"
            ],
            "output_schema": {
                "proposals": [{
                    "category": "identifier|comment|string",
                    "original_text": "string",
                    "sanitized_text": "string",
                    "confidence": "number from 0 to 1",
                    "rationale": "state whether this is a private-name or security/abuse-adjacent candidate and why"
                }]
            },
            "empty_response": { "proposals": [] },
            "context": context,
            "policy": {
                "denylist": config.sanitizer.denylist,
                "allowlist": config.sanitizer.allowlist,
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
    let batch_err = match serde_json::from_value::<ProposalBatch>(value.clone()) {
        Ok(batch) => return Ok(batch.proposals),
        Err(err) => err,
    };
    // Report BOTH failures: the batch shape is what providers are told to
    // emit, so swallowing its error left only the (less relevant) array
    // error to debug a near-miss batch against.
    serde_json::from_value::<Vec<Proposal>>(value).map_err(|array_err| {
        anyhow!(
            "parse proposals: not a ProposalBatch ({batch_err}) and not a \
             proposal array ({array_err})"
        )
    })
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
            json_mode: endpoint.json_mode,
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
    chunk: ProposalChunk,
}

struct FileState {
    position: usize,
    file: String,
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
    Skipped(String),
    Error(String),
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
}

struct FileProviderOutput {
    proposals: Vec<Proposal>,
    proposed: usize,
    duplicates: usize,
    rejected: Vec<String>,
    errors: Vec<String>,
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

    let started_at = Instant::now();
    let mut report = ProposeReport::default();
    let pending_items = list_review(root, false)?;
    let mut pending_keys: BTreeSet<(String, String)> = pending_items
        .iter()
        .map(|item| {
            (
                item.file.clone(),
                normalize_term(&item.proposal.original_text),
            )
        })
        .collect();
    let mut already_proposed_by_file = BTreeMap::<String, BTreeMap<String, String>>::new();
    for item in pending_items {
        already_proposed_by_file
            .entry(item.file)
            .or_default()
            .entry(normalize_term(&item.proposal.original_text))
            .or_insert(item.proposal.original_text);
    }
    let total = files.len();
    let chunk_llm_files = config.sanitizer.provider.llm_endpoint().is_some();
    let mut file_states = Vec::with_capacity(total);
    let mut work_items = Vec::new();
    for (file_index, file) in files.into_iter().enumerate() {
        let position = file_index + 1;
        let read = std::fs::read_to_string(root.join(&file));
        let (real, chunks, immediate) = match read {
            Err(err) => (
                String::new(),
                Vec::new(),
                Some(ImmediateFileResult::Error(format!("read failed: {err}"))),
            ),
            Ok(real) if real.len() as u64 > config.sanitizer.propose_max_file_bytes => {
                let reason = format!(
                    "{} bytes exceeds sanitizer.propose_max_file_bytes ({})",
                    real.len(),
                    config.sanitizer.propose_max_file_bytes
                );
                (real, Vec::new(), Some(ImmediateFileResult::Skipped(reason)))
            }
            Ok(real) => {
                let chunks = if chunk_llm_files {
                    split_proposal_chunks(
                        &real,
                        config.sanitizer.propose_chunk_bytes,
                        config.sanitizer.propose_chunk_overlap_lines,
                    )
                } else {
                    vec![ProposalChunk {
                        meta: ProposalChunkMeta::single(&real),
                        content: real.clone(),
                    }]
                };
                (real, chunks, None)
            }
        };
        let chunk_count = chunks.len();
        for chunk in chunks {
            work_items.push(WorkItem {
                file_index,
                file: file.clone(),
                chunk,
            });
        }
        file_states.push(FileState {
            position,
            file,
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

    let request_total = work_items.len();
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
            file: state.file.clone(),
            chunks: 0,
        });
        let outcome = match immediate {
            ImmediateFileResult::Skipped(reason) => {
                report.skipped.push(format!("{}: {reason}", state.file));
                ProposeFileOutcome::Skipped
            }
            ImmediateFileResult::Error(reason) => {
                report.errors.push(format!("{}: {reason}", state.file));
                ProposeFileOutcome::Error
            }
        };
        completed_files += 1;
        progress(ProposeProgress::FileFinished {
            completed: completed_files,
            total,
            file: state.file.clone(),
            elapsed_ms: 0,
            outcome,
            proposed: 0,
            queued: 0,
            duplicates: 0,
            rejected: 0,
        });
    }

    let mut completed_requests = 0usize;
    for wave in work_items.chunks(jobs) {
        let contexts = wave
            .iter()
            .map(|item| ProposalRequestContext {
                already_proposed_originals: already_proposed_by_file
                    .get(&item.file)
                    .map(|items| items.values().cloned().collect())
                    .unwrap_or_default(),
            })
            .collect::<Vec<_>>();

        for item in wave {
            let state = &mut file_states[item.file_index];
            if state.started_at.is_none() {
                state.started_at = Some(Instant::now());
                progress(ProposeProgress::FileStarted {
                    position: state.position,
                    total,
                    file: item.file.clone(),
                    chunks: state.chunks,
                });
            }
            progress(ProposeProgress::ChunkStarted {
                file: item.file.clone(),
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
                        Path::new(&item.file),
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
                        file: item.file.clone(),
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
                file,
                meta,
                result,
                ..
            } = event;
            let state = &mut file_states[file_index];
            match result {
                WorkerResult::Proposals(proposals) => {
                    state.proposed += proposals.len();
                    let context_keys = contexts[wave_index]
                        .already_proposed_originals
                        .iter()
                        .map(|item| normalize_term(item))
                        .collect::<BTreeSet<_>>();
                    let chunk = &wave[wave_index].chunk;
                    let core_content = proposal_core_content(&chunk.content, meta);
                    for proposal in proposals {
                        let normalized = normalize_term(&proposal.original_text);
                        if context_keys.contains(&normalized)
                            || (!core_content.contains(&proposal.original_text)
                                && chunk.content.contains(&proposal.original_text))
                        {
                            state.duplicates += 1;
                            continue;
                        }
                        if !core_content.contains(&proposal.original_text) {
                            state.rejected.push(format!(
                                "{}: original text does not appear in the chunk's owned content",
                                proposal.original_text
                            ));
                            continue;
                        }
                        if validate_proposal(Path::new(&file), &proposal, &state.real, &config)
                            .is_ok()
                        {
                            already_proposed_by_file
                                .entry(file.clone())
                                .or_default()
                                .entry(normalized)
                                .or_insert_with(|| proposal.original_text.clone());
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
                &config,
                &mut pending_keys,
                &mut report,
            )?;
            completed_files += 1;
            progress(ProposeProgress::FileFinished {
                completed: completed_files,
                total,
                file,
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
    if total > 0 && report.errors.len() == total {
        bail!(
            "provider failed for all {total} file(s); first: {}",
            report.errors[0]
        );
    }
    Ok(report)
}

fn commit_file_proposals(
    layout: &Layout,
    file: &str,
    real: &str,
    provider_output: FileProviderOutput,
    config: &Config,
    pending_keys: &mut BTreeSet<(String, String)>,
    report: &mut ProposeReport,
) -> Result<FileCompletion> {
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
            normalize_term(&proposal.original_text),
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
    report.rejected.extend(pre_rejected);
    for proposal in unique.into_values() {
        match validate_proposal(Path::new(file), &proposal, real, config) {
            Ok(flag) => {
                let key = (file.to_string(), normalize_term(&proposal.original_text));
                if pending_keys.insert(key) {
                    enqueue_review(layout, file, &proposal, &flag)?;
                    report.queued += 1;
                    file_queued += 1;
                } else {
                    report.duplicates += 1;
                    file_duplicates += 1;
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
        report
            .errors
            .push(format!("{file}: {}", provider_errors.join("; ")));
        ProposeFileOutcome::Error
    };
    Ok(FileCompletion {
        outcome,
        proposed,
        queued: file_queued,
        duplicates: file_duplicates,
        rejected: file_rejected,
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

    if collect_protected_identifiers(rel_path, content).contains(&proposal.original_text) {
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
        validate_proposal(Path::new(&item.file), &item.proposal, &real, &config)
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
    // Best-effort retention of the resolved history (same knob as the
    // journal); the resolution itself already landed. Lenient config load: a
    // sanitizer policy violation must not fail a reject.
    let keep = Config::load_or_default_lenient(&layout)
        .map(|config| config.journal.max_entries)
        .unwrap_or(0);
    if let Err(err) = prune_resolved_reviews(&layout, keep) {
        log::warn!("review-queue pruning failed: {err:#}");
    }
    Ok(item)
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
    fn rejects_invalid_identifier_alias() {
        let config = Config::default();
        let proposal = Proposal {
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
    fn rejects_alias_containing_denylisted_term() {
        let config = config_with_denylist(&["secret"]);
        let proposal = Proposal {
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
    fn prune_resolved_reviews_never_touches_pending() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        std::fs::create_dir_all(&layout.review_dir).unwrap();
        let write_item = |id: &str, status: ReviewStatus| {
            let item = ReviewItem {
                id: id.to_string(),
                file: "src/a.rs".to_string(),
                proposal: Proposal {
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
    fn low_confidence_is_queued_with_flag_not_rejected() {
        let config = Config::default();
        let proposal = Proposal {
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
}
