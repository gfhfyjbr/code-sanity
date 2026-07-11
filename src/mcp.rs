//! Minimal Model Context Protocol server over stdio.
//!
//! MCP stdio transport is JSON-RPC 2.0 with one message per line (no embedded
//! newlines, no Content-Length framing), so this is a dependency-free line loop
//! rather than an async runtime. Every tool reuses the existing bridge: reads
//! and search only ever touch the sanitized mirror, and `apply_patch` goes
//! through the same span-aware, conflict-safe path as the CLI.

use crate::patch::{ApplyOptions, apply_patch_text_with_options};
use crate::search::{list_mirror_files, read_sanitized_file};
use crate::verify::verify_workspace;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};
use std::path::Path;

const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";

/// Protocol revisions this server actually implements. `initialize` echoes the
/// client's requested version only when it is one of these; anything else
/// (including a made-up future string) negotiates down to the default instead
/// of claiming support the server does not have.
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

/// One request line at most; large patches fit comfortably, while a client
/// that never sends a newline cannot grow the buffer without bound.
const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;

struct ToolOutput {
    text: String,
    structured: Option<Value>,
}

impl ToolOutput {
    fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            structured: None,
        }
    }

    fn structured(value: Value) -> Result<Self> {
        Ok(Self {
            text: serde_json::to_string_pretty(&value).context("serialize tool output")?,
            structured: Some(value),
        })
    }
}

/// Serve MCP over the process stdio, blocking until stdin reaches EOF.
pub fn serve_stdio(root: &Path) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    serve(root, stdin.lock(), stdout.lock())
}

/// Serve MCP over arbitrary line-oriented streams (used directly in tests).
///
/// A long-lived session must survive malformed framing: an oversized or
/// non-UTF-8 request line is answered with a JSON-RPC parse error and the
/// loop keeps serving, instead of tearing down the whole server over one bad
/// byte from a buggy client.
pub fn serve<R: BufRead, W: Write>(root: &Path, mut reader: R, mut writer: W) -> Result<()> {
    loop {
        let line = match read_bounded_line(&mut reader, MAX_REQUEST_BYTES)
            .context("read MCP request line")?
        {
            None => break,
            Some(Ok(line)) => line,
            Some(Err(reason)) => {
                let response =
                    error_response(Value::Null, -32700, &format!("parse error: {reason}"));
                write_response(&mut writer, &response)?;
                continue;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(response) = handle_message(root, trimmed) {
            write_response(&mut writer, &response)?;
        }
    }
    Ok(())
}

fn write_response<W: Write>(writer: &mut W, response: &Value) -> Result<()> {
    let serialized = serde_json::to_string(response).context("serialize MCP response")?;
    writeln!(writer, "{serialized}").context("write MCP response")?;
    writer.flush().context("flush MCP response")
}

/// Read one `\n`-terminated request as raw bytes, bounded by `max`.
///
/// Returns `None` at clean EOF, `Some(Ok(line))` for a valid UTF-8 line
/// (terminator excluded), and `Some(Err(reason))` for a line that overflowed
/// the cap or is not UTF-8 — in both failure cases the input is consumed up to
/// the next newline (or EOF), so the caller can answer and keep the session.
fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    max: usize,
) -> io::Result<Option<std::result::Result<String, String>>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut overflowed = false;
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        if available.is_empty() {
            // EOF: nothing buffered at all is a clean end of session.
            if buf.is_empty() && !overflowed {
                return Ok(None);
            }
            break;
        }
        let newline = available.iter().position(|&byte| byte == b'\n');
        let chunk_len = newline.unwrap_or(available.len());
        if !overflowed {
            if buf.len() + chunk_len > max {
                // Stop buffering but keep draining to the newline so the next
                // request starts on a clean frame boundary.
                overflowed = true;
                buf = Vec::new();
            } else {
                buf.extend_from_slice(&available[..chunk_len]);
            }
        }
        let consume = chunk_len + usize::from(newline.is_some());
        reader.consume(consume);
        if newline.is_some() {
            break;
        }
    }
    if overflowed {
        return Ok(Some(Err(format!("request exceeds {max} bytes"))));
    }
    match String::from_utf8(buf) {
        Ok(line) => Ok(Some(Ok(line))),
        Err(_) => Ok(Some(Err("request is not valid UTF-8".to_string()))),
    }
}

/// The `tools/list` result, pretty-printed for `serve --once` inspection.
pub fn tools_manifest_json() -> String {
    serde_json::to_string_pretty(&json!({ "tools": tools_manifest() }))
        .unwrap_or_else(|_| "{}".to_string())
}

fn handle_message(root: &Path, raw: &str) -> Option<Value> {
    let message: Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(err) => {
            return Some(error_response(
                Value::Null,
                -32700,
                &format!("parse error: {err}"),
            ));
        }
    };
    // Valid JSON that is not a single request object (a batch array, a bare
    // scalar) was previously dropped without a reply; per JSON-RPC that is an
    // Invalid Request the client should hear about.
    if !message.is_object() {
        return Some(error_response(
            Value::Null,
            -32600,
            "invalid request: expected a single JSON-RPC object (batches are not supported)",
        ));
    }
    let id = message.get("id").cloned();
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return Some(error_response(
            id.unwrap_or(Value::Null),
            -32600,
            "invalid request: missing method",
        ));
    };
    let is_notification = id.is_none();

    match method {
        "initialize" => Some(result_response(
            id.unwrap_or(Value::Null),
            initialize_result(&message),
        )),
        "ping" => Some(result_response(id.unwrap_or(Value::Null), json!({}))),
        "tools/list" => Some(result_response(
            id.unwrap_or(Value::Null),
            json!({ "tools": tools_manifest() }),
        )),
        "tools/call" => {
            let id = id.unwrap_or(Value::Null);
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            let result = match call_tool(root, name, &args) {
                Ok(output) => {
                    let mut result = json!({
                        "content": [{ "type": "text", "text": output.text }],
                        "isError": false,
                    });
                    if let Some(structured) = output.structured {
                        result["structuredContent"] = structured;
                    }
                    result
                }
                Err(err) => json!({
                    "content": [{ "type": "text", "text": redact_error(root, &format!("error: {err:#}")) }],
                    "isError": true,
                }),
            };
            Some(result_response(id, result))
        }
        _ if is_notification => None,
        _ => Some(error_response(
            id.unwrap_or(Value::Null),
            -32601,
            &format!("method not found: {method}"),
        )),
    }
}

/// Tool successes return sanitized-mirror content, but errors interpolate
/// whatever the failure touched (paths, io text, hunk context) — pass them
/// through the workspace redactor before they leave toward the agent. Fails
/// closed: if the redactor cannot be built, a generic message goes out
/// instead of an unredacted one.
fn redact_error(root: &Path, message: &str) -> String {
    const GENERIC: &str = "error: tool failed and the output redactor is unavailable; \
                           details withheld — run `code-sanity verify` on the host for diagnostics";
    let layout = crate::config::Layout::new(root);
    // Never conjure a .code-sanity dir from the error path (acquiring the
    // lock would create tmp/): an uninitialized workspace gets the generic
    // message, which is also fail-closed.
    if layout.require_initialized().is_err() {
        return GENERIC.to_string();
    }
    let Ok(_lock) = crate::lock::WorkspaceLock::acquire_shared(&layout) else {
        return GENERIC.to_string();
    };
    // The workspace root's own directory names (e.g. a company-named repo
    // folder) are not dictionary terms, so the term redactor cannot remove
    // them from interpolated absolute paths — scrub the prefix first.
    let message = message.replace(&root.display().to_string(), ".");
    match crate::redact::Redactor::for_workspace(root) {
        Ok(redactor) => redactor.redact(&message),
        Err(_) => GENERIC.to_string(),
    }
}

fn initialize_result(message: &Value) -> Value {
    // Version negotiation, not an echo: a requested revision we implement is
    // confirmed; anything unknown answers with the default so the server
    // never claims support for a protocol it has not seen.
    let requested = message
        .get("params")
        .and_then(|params| params.get("protocolVersion"))
        .and_then(Value::as_str)
        .filter(|version| SUPPORTED_PROTOCOL_VERSIONS.contains(version))
        .unwrap_or(DEFAULT_PROTOCOL_VERSION);
    json!({
        "protocolVersion": requested,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "code-sanity", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "Use workspace_snapshot, find_code, and read_code for AST/symbol context. Mutate only through edit_node/rename_symbol or preview_transaction followed by commit_transaction with expected_revision. Legacy mirror tools remain available for v1 clients.",
    })
}

fn tools_manifest() -> Value {
    json!([
        {
            "name": "workspace_snapshot",
            "description": "Return the semantic workspace revision and index counts used for optimistic concurrency.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "find_code",
            "description": "Find indexed symbols by name or qualified name. Returns stable symbol_id/node_id values.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000 }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        },
        {
            "name": "read_code",
            "description": "Read a semantic projection with stable symbols, occurrences, capabilities, and revision.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
                "additionalProperties": false
            }
        },
        {
            "name": "references",
            "description": "List occurrences bound to one stable symbol_id; unresolved text matches are never included.",
            "inputSchema": {
                "type": "object",
                "properties": { "symbol_id": { "type": "string" } },
                "required": ["symbol_id"],
                "additionalProperties": false
            }
        },
        {
            "name": "edit_node",
            "description": "Preview one AST-node edit. Declaration-containing nodes are rejected; use rename_symbol.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "node_id": { "type": "string" },
                    "replacement": { "type": "string" },
                    "expected_revision": { "type": "integer", "minimum": 0 }
                },
                "required": ["node_id", "replacement", "expected_revision"],
                "additionalProperties": false
            }
        },
        {
            "name": "rename_symbol",
            "description": "Preview a compiler/LSP-backed semantic rename for one symbol_id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol_id": { "type": "string" },
                    "new_name": { "type": "string" },
                    "expected_revision": { "type": "integer", "minimum": 0 }
                },
                "required": ["symbol_id", "new_name", "expected_revision"],
                "additionalProperties": false
            }
        },
        {
            "name": "preview_transaction",
            "description": "Validate and persist a multi-intent AST/LSP transaction without writing source files.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "expected_revision": { "type": "integer", "minimum": 0 },
                    "intents": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "oneOf": [
                                {
                                    "type": "object",
                                    "properties": {
                                        "kind": { "const": "edit_node" },
                                        "node_id": { "type": "string" },
                                        "replacement": { "type": "string" }
                                    },
                                    "required": ["kind", "node_id", "replacement"],
                                    "additionalProperties": false
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "kind": { "const": "rename_symbol" },
                                        "symbol_id": { "type": "string" },
                                        "new_name": { "type": "string" }
                                    },
                                    "required": ["kind", "symbol_id", "new_name"],
                                    "additionalProperties": false
                                }
                            ]
                        }
                    }
                },
                "required": ["expected_revision", "intents"],
                "additionalProperties": false
            }
        },
        {
            "name": "commit_transaction",
            "description": "Atomically commit a previously previewed transaction using revision CAS and crash-safe rollback.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "transaction_id": { "type": "string" },
                    "expected_revision": { "type": "integer", "minimum": 0 },
                    "agent": { "type": "string" },
                    "session_id": { "type": "string" }
                },
                "required": ["transaction_id", "expected_revision"],
                "additionalProperties": false
            }
        },
        {
            "name": "read_file",
            "description": "Read a file from the sanitized mirror. Returns sanitized content only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Repo-relative path, e.g. src/lib.rs" }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        },
        {
            "name": "search",
            "description": "Search the sanitized mirror. Returns path:line:column:text lines (sanitized).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Substring to search for" },
                    "glob": { "type": "string", "description": "Glob filter: without '/' matches file names at any depth (*.rs); with '/' matches the repo-relative path (src/**/*.rs)" },
                    "max_results": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": crate::search::HARD_MAX_RESULTS,
                        "description": "Result cap (default 200, hard max 1000)"
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        },
        {
            "name": "list_files",
            "description": "List repo-relative paths tracked in the sanitized mirror.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "glob": { "type": "string", "description": "Glob filter: without '/' matches file names at any depth (*.rs); with '/' matches the repo-relative path (src/**)" }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "semantic_search",
            "description": "Semantic (embedding) search over the sanitized mirror. Requires embeddings enabled in config and a populated vector index (embed-index). Returns path:start-end score preview lines (sanitized).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Natural-language or code query" },
                    "k": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Top chunks to return (default 10)" }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        },
        {
            "name": "apply_patch",
            "description": "Apply a unified diff written against sanitized mirror paths. Projected back onto the real repo through the span-aware bridge; edits inside replacement spans conflict.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "patch": { "type": "string", "description": "Unified diff against a/ b/ mirror paths" },
                    "agent": { "type": "string" },
                    "session_id": { "type": "string" },
                    "dry_run": { "type": "boolean", "description": "Validate and plan only; do not write. Reports the files that would change." }
                },
                "required": ["patch"],
                "additionalProperties": false
            }
        },
        {
            "name": "verify",
            "description": "Verify mirror/map/hash consistency for all tracked files.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }
    ])
}

fn call_tool(root: &Path, name: &str, args: &Value) -> Result<ToolOutput> {
    match name {
        "read_file" => {
            let path = required_str(args, "path")?;
            Ok(ToolOutput::text(read_sanitized_file(
                root,
                Path::new(&path),
            )?))
        }
        "search" => {
            let query = required_str(args, "query")?;
            let glob = optional_str(args, "glob");
            let max_results = args
                .get("max_results")
                .and_then(Value::as_u64)
                .map(|value| value as usize);
            let (hits, truncated) =
                crate::search::search_mirror_limited(root, &query, glob.as_deref(), max_results)?;
            let mut out = hits
                .iter()
                .map(|hit| {
                    format!(
                        "{}:{}:{}:{}",
                        hit.rel_path, hit.line, hit.column, hit.line_text
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            if truncated {
                out.push_str(&format!(
                    "\n[truncated to {} results; refine the query or raise max_results]",
                    hits.len()
                ));
            }
            Ok(ToolOutput::text(out))
        }
        "list_files" => {
            let glob = optional_str(args, "glob");
            Ok(ToolOutput::text(
                list_mirror_files(root, glob.as_deref())?.join("\n"),
            ))
        }
        "semantic_search" => {
            let query = required_str(args, "query")?;
            let k = args
                .get("k")
                .and_then(Value::as_u64)
                .map(|value| (value as usize).clamp(1, 100))
                .unwrap_or(10);
            let hits = crate::embed::semantic_search(root, &query, k)?;
            Ok(ToolOutput::text(
                hits.iter()
                    .map(|hit| {
                        format!(
                            "{}:{}-{}\t{:.3}\t{}",
                            hit.rel_path, hit.start_line, hit.end_line, hit.score, hit.preview
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ))
        }
        "apply_patch" => {
            let patch = required_str(args, "patch")?;
            let report = apply_patch_text_with_options(
                root,
                &patch,
                ApplyOptions {
                    agent: optional_str(args, "agent"),
                    session_id: optional_str(args, "session_id"),
                    dry_run: args
                        .get("dry_run")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                },
            )?;
            match &report.journal_path {
                // Workspace-relative: the absolute root path (directory names
                // the dictionary redactor cannot know about) must never reach
                // the agent, success or not.
                Some(journal) => Ok(ToolOutput::text(format!(
                    "applied files={} journal={}",
                    report.files.join(","),
                    relative_journal(root, journal)
                ))),
                None => Ok(ToolOutput::text(format!(
                    "dry-run ok: would apply files={} (no changes written)",
                    report.files.join(",")
                ))),
            }
        }
        "workspace_snapshot" => with_semantic_read(root, |conn| {
            ToolOutput::structured(serde_json::to_value(crate::semantic_store::snapshot(
                conn,
            )?)?)
        }),
        "find_code" => {
            let query = required_str(args, "query")?;
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize;
            with_semantic_read(root, |conn| {
                let symbols = crate::semantic_store::find_symbols(conn, &query, limit)?;
                ToolOutput::structured(json!({
                    "revision": crate::semantic_store::current_revision(conn)?,
                    "symbols": symbols.into_iter().map(|(path, symbol)| json!({
                        "path": path,
                        "symbol": symbol,
                    })).collect::<Vec<_>>()
                }))
            })
        }
        "read_code" => {
            let path = required_str(args, "path")?;
            with_semantic_read(root, |conn| {
                let projected = crate::semantic_store::project_document(conn, root, &path)?;
                ToolOutput::structured(serde_json::to_value(projected)?)
            })
        }
        "references" => {
            let symbol_id = required_str(args, "symbol_id")?;
            semantic_references(root, &symbol_id)
        }
        "edit_node" => {
            let node_id = required_str(args, "node_id")?;
            let replacement = required_str(args, "replacement")?;
            let expected_revision = required_u64(args, "expected_revision")?;
            let preview = crate::transaction::preview_transaction(
                root,
                expected_revision,
                vec![crate::transaction::EditIntent::EditNode {
                    node_id,
                    replacement,
                }],
            )?;
            ToolOutput::structured(serde_json::to_value(preview)?)
        }
        "rename_symbol" => {
            let symbol_id = required_str(args, "symbol_id")?;
            let new_name = required_str(args, "new_name")?;
            let expected_revision = required_u64(args, "expected_revision")?;
            let preview = crate::transaction::preview_transaction(
                root,
                expected_revision,
                vec![crate::transaction::EditIntent::RenameSymbol {
                    symbol_id,
                    new_name,
                }],
            )?;
            ToolOutput::structured(serde_json::to_value(preview)?)
        }
        "preview_transaction" => {
            let expected_revision = required_u64(args, "expected_revision")?;
            let intents = serde_json::from_value::<Vec<crate::transaction::EditIntent>>(
                args.get("intents")
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("missing intents"))?,
            )
            .context("parse structured edit intents")?;
            let preview =
                crate::transaction::preview_transaction(root, expected_revision, intents)?;
            ToolOutput::structured(serde_json::to_value(preview)?)
        }
        "commit_transaction" => {
            let transaction_id = required_str(args, "transaction_id")?;
            let expected_revision = required_u64(args, "expected_revision")?;
            let report = crate::transaction::commit_transaction(
                root,
                &transaction_id,
                expected_revision,
                optional_str(args, "agent"),
                optional_str(args, "session_id"),
            )?;
            ToolOutput::structured(serde_json::to_value(report)?)
        }
        "verify" => {
            let report = verify_workspace(root)?;
            Ok(ToolOutput::text(format!(
                "verified tracked_files={}",
                report.checked
            )))
        }
        other => bail!("unknown tool: {other}"),
    }
}

fn with_semantic_read(
    root: &Path,
    operation: impl FnOnce(&rusqlite::Connection) -> Result<ToolOutput>,
) -> Result<ToolOutput> {
    let layout = crate::config::Layout::new(root);
    layout.require_initialized()?;
    let _lock = crate::lock::WorkspaceLock::acquire_shared(&layout)?;
    let conn = crate::db::connect(&layout)?;
    crate::db::check_schema(&conn)?;
    operation(&conn)
}

fn semantic_references(root: &Path, symbol_id: &str) -> Result<ToolOutput> {
    let layout = crate::config::Layout::new(root);
    layout.require_initialized()?;
    let (revision, rel_path, symbol, document, source, minimum_references) = {
        let _lock = crate::lock::WorkspaceLock::acquire_shared(&layout)?;
        let conn = crate::db::connect(&layout)?;
        crate::db::check_schema(&conn)?;
        let revision = crate::semantic_store::current_revision(&conn)?;
        let (rel_path, symbol) = crate::semantic_store::load_symbol_with_path(&conn, symbol_id)?
            .ok_or_else(|| anyhow::anyhow!("unknown symbol_id {symbol_id}"))?;
        let document = crate::semantic_store::load_document(&conn, &rel_path)?
            .ok_or_else(|| anyhow::anyhow!("semantic document disappeared for {symbol_id}"))?;
        if !document.capabilities.references {
            bail!("compiler/LSP references are unavailable for {rel_path}");
        }
        let rel = crate::config::normalize_safe_rel_path(Path::new(&rel_path), "symbol path")?;
        let source = std::fs::read_to_string(root.join(rel))?;
        if crate::map::sha256_hex(source.as_bytes()) != document.content_hash {
            bail!("{rel_path} changed since semantic index; run code-sanity index");
        }
        let minimum_references =
            crate::semantic_store::occurrences_for_symbol(&conn, symbol_id)?.len();
        (
            revision,
            rel_path,
            symbol,
            document,
            source,
            minimum_references,
        )
    };
    let rel = crate::config::normalize_safe_rel_path(Path::new(&rel_path), "symbol path")?;
    let locations = crate::lsp::references(
        root,
        &rel,
        &source,
        document.language,
        &symbol.range,
        minimum_references,
    )?;
    {
        let _lock = crate::lock::WorkspaceLock::acquire_shared(&layout)?;
        let conn = crate::db::connect(&layout)?;
        let current = crate::semantic_store::current_revision(&conn)?;
        if current != revision {
            bail!("stale semantic revision: expected {revision}, current {current}");
        }
    }
    ToolOutput::structured(json!({
        "revision": revision,
        "symbol_id": symbol_id,
        "locations": locations,
    }))
}

/// Render a journal path workspace-relative for agent-facing output; fail
/// closed to the bare file name — never an absolute host prefix.
fn relative_journal(root: &Path, journal: &Path) -> String {
    journal
        .strip_prefix(root)
        .map(|relative| relative.display().to_string())
        .unwrap_or_else(|_| {
            journal
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
}

fn required_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!("missing required string argument: {key}"))
}

fn optional_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(ToOwned::to_owned)
}

fn required_u64(args: &Value, key: &str) -> Result<u64> {
    args.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("missing required non-negative integer argument: {key}"))
}

fn result_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn call(root: &Path, requests: &[&str]) -> Vec<Value> {
        let input = requests.join("\n");
        let mut output = Vec::new();
        serve(root, Cursor::new(input.into_bytes()), &mut output).unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect()
    }

    #[test]
    fn initialize_and_list_tools() {
        let repo = tempfile::tempdir().unwrap();
        let responses = call(
            repo.path(),
            &[
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            ],
        );
        // The notification produces no response.
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(responses[0]["result"]["serverInfo"]["name"], "code-sanity");
        let tools = responses[1]["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "workspace_snapshot",
                "find_code",
                "read_code",
                "references",
                "edit_node",
                "rename_symbol",
                "preview_transaction",
                "commit_transaction",
                "read_file",
                "search",
                "list_files",
                "semantic_search",
                "apply_patch",
                "verify"
            ]
        );
    }

    #[test]
    fn semantic_v2_tools_preview_and_commit_structured_edit() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("src/lib.rs"),
            "fn value() -> u32 {\n    1\n}\n",
        )
        .unwrap();
        crate::index_workspace(repo.path()).unwrap();

        let snapshot_request = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "workspace_snapshot", "arguments": {} }
        }))
        .unwrap();
        let read_request = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "read_code", "arguments": { "path": "src/lib.rs" } }
        }))
        .unwrap();
        let responses = call(repo.path(), &[&snapshot_request, &read_request]);
        let revision = responses[0]["result"]["structuredContent"]["revision"]
            .as_u64()
            .unwrap();
        let node_id = responses[1]["result"]["structuredContent"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["kind"] == "integer_literal")
            .unwrap()["node_id"]
            .as_str()
            .unwrap()
            .to_string();

        let preview_request = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "edit_node",
                "arguments": {
                    "node_id": node_id,
                    "replacement": "2",
                    "expected_revision": revision
                }
            }
        }))
        .unwrap();
        let preview = call(repo.path(), &[&preview_request]);
        let transaction_id = preview[0]["result"]["structuredContent"]["transaction_id"]
            .as_str()
            .unwrap();
        let commit_request = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "commit_transaction",
                "arguments": {
                    "transaction_id": transaction_id,
                    "expected_revision": revision
                }
            }
        }))
        .unwrap();
        let committed = call(repo.path(), &[&commit_request]);
        assert_eq!(committed[0]["result"]["isError"], false);
        assert_eq!(
            std::fs::read_to_string(repo.path().join("src/lib.rs")).unwrap(),
            "fn value() -> u32 {\n    2\n}\n"
        );
    }

    /// Raw-bytes variant of `call` for inputs that are not valid UTF-8 lines.
    fn call_bytes(root: &Path, input: Vec<u8>) -> Vec<Value> {
        let mut output = Vec::new();
        serve(root, Cursor::new(input), &mut output).unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect()
    }

    #[test]
    fn non_utf8_input_gets_parse_error_and_session_survives() {
        let repo = tempfile::tempdir().unwrap();
        let mut input = vec![0xff, 0xfe, 0x0a]; // invalid UTF-8, then newline
        input.extend_from_slice(br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
        input.push(b'\n');
        let responses = call_bytes(repo.path(), input);
        assert_eq!(responses.len(), 2, "bad bytes must not end the session");
        assert_eq!(responses[0]["error"]["code"], -32700);
        assert_eq!(responses[0]["id"], Value::Null);
        assert_eq!(responses[1]["id"], 1);
        assert!(responses[1]["result"].is_object());
    }

    #[test]
    fn oversized_line_is_bounded_and_drained_to_next_frame() {
        // Unit-level: the cap and the drain-to-newline recovery, with a small
        // max so the test does not allocate MAX_REQUEST_BYTES.
        let input = format!("{}\nnext\n", "x".repeat(64));
        let mut reader = Cursor::new(input.into_bytes());
        let first = read_bounded_line(&mut reader, 16).unwrap().unwrap();
        assert!(first.unwrap_err().contains("exceeds 16 bytes"));
        let second = read_bounded_line(&mut reader, 16).unwrap().unwrap();
        assert_eq!(second.unwrap(), "next");
        assert!(read_bounded_line(&mut reader, 16).unwrap().is_none());
        // Oversized final line without a trailing newline still errors.
        let mut reader = Cursor::new(b"yyyyyyyyyyyyyyyyyyyyyyyy".to_vec());
        assert!(
            read_bounded_line(&mut reader, 16)
                .unwrap()
                .unwrap()
                .is_err()
        );
    }

    #[test]
    fn non_object_and_methodless_messages_are_invalid_requests() {
        let repo = tempfile::tempdir().unwrap();
        let responses = call(
            repo.path(),
            &[
                r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#,
                r#""just a string""#,
                r#"{"jsonrpc":"2.0","id":7}"#,
                r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#,
            ],
        );
        assert_eq!(responses.len(), 4);
        assert_eq!(responses[0]["error"]["code"], -32600);
        assert_eq!(responses[1]["error"]["code"], -32600);
        assert_eq!(responses[2]["error"]["code"], -32600);
        assert_eq!(responses[2]["id"], 7, "id is echoed when present");
        assert!(responses[3]["result"].is_object(), "session continues");
    }

    #[test]
    fn initialize_negotiates_unknown_protocol_version_down() {
        let repo = tempfile::tempdir().unwrap();
        let responses = call(
            repo.path(),
            &[
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"9999-99-99"}}"#,
            ],
        );
        assert_eq!(
            responses[0]["result"]["protocolVersion"],
            DEFAULT_PROTOCOL_VERSION
        );
    }

    #[test]
    fn unknown_method_is_json_rpc_error() {
        let repo = tempfile::tempdir().unwrap();
        let responses = call(
            repo.path(),
            &[r#"{"jsonrpc":"2.0","id":9,"method":"does/not/exist"}"#],
        );
        assert_eq!(responses[0]["error"]["code"], -32601);
    }

    #[test]
    fn tool_errors_are_redacted_and_flagged() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("real.rs"), "fn acme_helper() {}\n").unwrap();
        let layout = crate::config::Layout::new(repo.path());
        crate::index::init_workspace(repo.path()).unwrap();
        let mut config = crate::config::Config::load_or_default(&layout).unwrap();
        config
            .sanitizer
            .dictionary
            .insert("acme".to_string(), "client".to_string());
        config.save(&layout).unwrap();
        crate::index::index_workspace(repo.path()).unwrap();

        let responses = call(
            repo.path(),
            &[
                // A failing read whose requested path carries a real term:
                // the error text must leave redacted.
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"missing_acme_file.rs"}}}"#,
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"no_such_tool","arguments":{}}}"#,
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search","arguments":{}}}"#,
            ],
        );
        assert_eq!(responses[0]["result"]["isError"], true);
        let text = responses[0]["result"]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(
            !text.to_lowercase().contains("acme"),
            "real term leaked: {text}"
        );
        assert!(
            text.contains("client"),
            "expected redacted alias in: {text}"
        );

        // Unknown tool and missing required argument surface as tool errors,
        // not transport errors.
        assert_eq!(responses[1]["result"]["isError"], true);
        assert_eq!(responses[2]["result"]["isError"], true);
    }
}
