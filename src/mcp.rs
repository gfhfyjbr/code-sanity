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

/// Serve MCP over the process stdio, blocking until stdin reaches EOF.
pub fn serve_stdio(root: &Path) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    serve(root, stdin.lock(), stdout.lock())
}

/// Serve MCP over arbitrary line-oriented streams (used directly in tests).
pub fn serve<R: BufRead, W: Write>(root: &Path, mut reader: R, mut writer: W) -> Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .context("read MCP request line")?;
        if read == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(response) = handle_message(root, trimmed) {
            let serialized = serde_json::to_string(&response).context("serialize MCP response")?;
            writeln!(writer, "{serialized}").context("write MCP response")?;
            writer.flush().context("flush MCP response")?;
        }
    }
    Ok(())
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
    let id = message.get("id").cloned();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
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
                Ok(text) => json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": false,
                }),
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
    let requested = message
        .get("params")
        .and_then(|params| params.get("protocolVersion"))
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_PROTOCOL_VERSION);
    json!({
        "protocolVersion": requested,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "code-sanity", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "Sanitized mirror tools. read_file/search/list_files return sanitized content only; apply_patch projects a sanitized patch back onto the real repo through the bridge.",
    })
}

fn tools_manifest() -> Value {
    json!([
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

fn call_tool(root: &Path, name: &str, args: &Value) -> Result<String> {
    match name {
        "read_file" => {
            let path = required_str(args, "path")?;
            read_sanitized_file(root, Path::new(&path))
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
            Ok(out)
        }
        "list_files" => {
            let glob = optional_str(args, "glob");
            Ok(list_mirror_files(root, glob.as_deref())?.join("\n"))
        }
        "semantic_search" => {
            let query = required_str(args, "query")?;
            let k = args
                .get("k")
                .and_then(Value::as_u64)
                .map(|value| (value as usize).clamp(1, 100))
                .unwrap_or(10);
            let hits = crate::embed::semantic_search(root, &query, k)?;
            Ok(hits
                .iter()
                .map(|hit| {
                    format!(
                        "{}:{}-{}\t{:.3}\t{}",
                        hit.rel_path, hit.start_line, hit.end_line, hit.score, hit.preview
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
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
                Some(journal) => Ok(format!(
                    "applied files={} journal={}",
                    report.files.join(","),
                    relative_journal(root, journal)
                )),
                None => Ok(format!(
                    "dry-run ok: would apply files={} (no changes written)",
                    report.files.join(",")
                )),
            }
        }
        "verify" => {
            let report = verify_workspace(root)?;
            Ok(format!("verified tracked_files={}", report.checked))
        }
        other => bail!("unknown tool: {other}"),
    }
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
