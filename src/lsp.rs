//! Small synchronous LSP client used only for semantic refactors.
//!
//! Requests run without the workspace lock. Callers must compare the semantic
//! revision again before persisting or committing the returned workspace edit.

use crate::config::normalize_safe_rel_path;
use crate::semantic::{LanguageId, TextRange};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceTextEdit {
    pub rel_path: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub new_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LspLocation {
    pub rel_path: String,
    pub range: TextRange,
}

pub fn references(
    root: &Path,
    rel_path: &Path,
    source: &str,
    language: LanguageId,
    declaration: &TextRange,
) -> Result<Vec<LspLocation>> {
    let server = Server::for_language(language)?;
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalize workspace root {}", root.display()))?;
    let target = root.join(rel_path);
    let uri = file_uri(&target);
    let result = with_client(server, &root, |client| {
        client.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id(language),
                    "version": 1,
                    "text": source,
                }
            }),
        )?;
        client.wait_for_document(&file_uri(&target))?;
        let result = client.request_retry_content_modified(
            "textDocument/references",
            json!({
                "textDocument": { "uri": file_uri(&target) },
                "position": {
                    "line": declaration.start_line.saturating_sub(1),
                    "character": byte_column_to_utf16(
                        source,
                        declaration.start_line.saturating_sub(1),
                        declaration.start_column.saturating_sub(1),
                    )?,
                },
                "context": { "includeDeclaration": true }
            }),
        );
        let _ = client.notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": file_uri(&target) } }),
        );
        result
    })?;
    parse_locations(&root, &result)
}

pub fn rename(
    root: &Path,
    rel_path: &Path,
    source: &str,
    language: LanguageId,
    declaration: &TextRange,
    new_name: &str,
) -> Result<Vec<WorkspaceTextEdit>> {
    let server = Server::for_language(language)?;
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalize workspace root {}", root.display()))?;
    let target = root.join(rel_path);
    let uri = file_uri(&target);
    let params = json!({
        "textDocument": { "uri": file_uri(&target) },
        "position": {
            "line": declaration.start_line.saturating_sub(1),
            "character": byte_column_to_utf16(
                source,
                declaration.start_line.saturating_sub(1),
                declaration.start_column.saturating_sub(1),
            )?,
        },
        "newName": new_name,
    });
    let result = with_client(server, &root, |client| {
        client.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id(language),
                    "version": 1,
                    "text": source,
                }
            }),
        )?;
        client.wait_for_document(&file_uri(&target))?;
        let mut result = None;
        for attempt in 0..3 {
            match client.request("textDocument/rename", params.clone()) {
                Ok(value) => {
                    result = Some(value);
                    break;
                }
                Err(err) if err.to_string().contains("content modified") && attempt < 2 => {
                    std::thread::sleep(Duration::from_millis(200 * (attempt + 1)));
                }
                Err(err) => return Err(err),
            }
        }
        let _ = client.notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": file_uri(&target) } }),
        );
        result.context("language server did not return a rename result")
    })?;
    parse_workspace_edit(&root, &result)
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Server {
    RustAnalyzer,
    Clangd,
}

fn with_client<T>(
    server: Server,
    root: &Path,
    operation: impl FnOnce(&mut Client) -> Result<T>,
) -> Result<T> {
    static CLIENTS: OnceLock<Mutex<HashMap<String, Arc<Mutex<Client>>>>> = OnceLock::new();
    let key = format!("{}:{}", server.command(), root.display());
    let client = {
        let mut clients = CLIENTS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .map_err(|_| anyhow!("LSP client cache lock was poisoned"))?;
        match clients.get(&key) {
            Some(client) => Arc::clone(client),
            None => {
                let client = Arc::new(Mutex::new(Client::spawn(server, root)?));
                clients.insert(key, Arc::clone(&client));
                client
            }
        }
    };
    let mut client = client
        .lock()
        .map_err(|_| anyhow!("LSP workspace client lock was poisoned"))?;
    operation(&mut client)
}

impl Server {
    fn for_language(language: LanguageId) -> Result<Self> {
        let server = match language {
            LanguageId::Rust => Self::RustAnalyzer,
            LanguageId::Cpp | LanguageId::ObjectiveC | LanguageId::ObjectiveCpp => Self::Clangd,
            _ => bail!("no LSP rename backend for {language:?}"),
        };
        if !server.available() {
            bail!(
                "{} is unavailable; semantic rename is disabled",
                server.command()
            );
        }
        Ok(server)
    }

    fn command(self) -> &'static str {
        match self {
            Self::RustAnalyzer => "rust-analyzer",
            Self::Clangd => "clangd",
        }
    }

    fn available(self) -> bool {
        Command::new(self.command())
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn args(self) -> &'static [&'static str] {
        match self {
            Self::RustAnalyzer => &[],
            Self::Clangd => &[
                "--background-index=false",
                "--clang-tidy=false",
                "--header-insertion=never",
            ],
        }
    }
}

struct Client {
    child: Child,
    stdin: ChildStdin,
    messages: Receiver<std::result::Result<Value, String>>,
    next_id: u64,
}

impl Client {
    fn spawn(server: Server, root: &Path) -> Result<Self> {
        let mut child = Command::new(server.command())
            .args(server.args())
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawn {}", server.command()))?;
        let stdin = child.stdin.take().context("LSP stdin unavailable")?;
        let stdout = child.stdout.take().context("LSP stdout unavailable")?;
        let (sender, messages) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_message(&mut reader) {
                    Ok(Some(message)) => {
                        if sender.send(Ok(message)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        let _ = sender.send(Err(format!("{err:#}")));
                        break;
                    }
                }
            }
        });
        let mut client = Self {
            child,
            stdin,
            messages,
            next_id: 1,
        };
        client.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": file_uri(root),
                "workspaceFolders": [{ "uri": file_uri(root), "name": "workspace" }],
                "capabilities": {
                    "workspace": { "workspaceEdit": { "documentChanges": true } },
                    "textDocument": { "rename": { "prepareSupport": false } }
                },
            }),
        )?;
        client.notify("initialized", json!({}))?;
        Ok(client)
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        loop {
            let message = self
                .messages
                .recv_timeout(RESPONSE_TIMEOUT)
                .map_err(|err| anyhow!("LSP {method} timed out or disconnected: {err}"))?
                .map_err(|err| anyhow!("read LSP response: {err}"))?;
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                if message.get("method").and_then(Value::as_str).is_some()
                    && message.get("id").is_some()
                {
                    self.respond_to_server_request(&message)?;
                }
                continue;
            }
            if let Some(error) = message.get("error") {
                bail!("LSP {method} failed: {error}");
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.write(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    fn request_retry_content_modified(&mut self, method: &str, params: Value) -> Result<Value> {
        for attempt in 0..3 {
            match self.request(method, params.clone()) {
                Ok(value) => return Ok(value),
                Err(err) if err.to_string().contains("content modified") && attempt < 2 => {
                    std::thread::sleep(Duration::from_millis(200 * (attempt + 1)));
                }
                Err(err) => return Err(err),
            }
        }
        bail!("LSP {method} did not return a stable result")
    }

    fn wait_for_document(&mut self, uri: &str) -> Result<()> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let message = match self.messages.recv_timeout(remaining) {
                Ok(message) => {
                    message.map_err(|err| anyhow!("read LSP readiness message: {err}"))?
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(err) => {
                    return Err(anyhow!("LSP disconnected before document was ready: {err}"));
                }
            };
            if message.get("method").and_then(Value::as_str)
                == Some("textDocument/publishDiagnostics")
                && message.pointer("/params/uri").and_then(Value::as_str) == Some(uri)
            {
                return Ok(());
            }
            if message.get("method").and_then(Value::as_str).is_some()
                && message.get("id").is_some()
            {
                self.respond_to_server_request(&message)?;
            }
        }
        // Some servers suppress empty diagnostics. A bounded grace period is
        // preferable to holding a lock; rename itself remains authoritative.
        Ok(())
    }

    fn respond_to_server_request(&mut self, request: &Value) -> Result<()> {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let result =
            if request.get("method").and_then(Value::as_str) == Some("workspace/configuration") {
                let count = request
                    .pointer("/params/items")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                Value::Array((0..count).map(|_| Value::Null).collect())
            } else {
                Value::Null
            };
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    fn write(&mut self, message: &Value) -> Result<()> {
        let body = serde_json::to_vec(message).context("serialize LSP request")?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len()).context("write LSP header")?;
        self.stdin.write_all(&body).context("write LSP body")?;
        self.stdin.flush().context("flush LSP request")
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_message(reader: &mut impl BufRead) -> Result<Option<Value>> {
    let mut content_length = None;
    loop {
        let mut header = String::new();
        let read = reader.read_line(&mut header).context("read LSP header")?;
        if read == 0 {
            return Ok(None);
        }
        if header == "\r\n" || header == "\n" {
            break;
        }
        if let Some(value) = header.trim().strip_prefix("Content-Length:").map(str::trim) {
            content_length = Some(value.parse::<usize>().context("parse LSP content length")?);
        }
    }
    let length = content_length.context("LSP response omitted Content-Length")?;
    if length > MAX_MESSAGE_BYTES {
        bail!("LSP response exceeds {MAX_MESSAGE_BYTES} bytes");
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).context("read LSP body")?;
    serde_json::from_slice(&body)
        .context("parse LSP JSON")
        .map(Some)
}

fn parse_workspace_edit(root: &Path, value: &Value) -> Result<Vec<WorkspaceTextEdit>> {
    if value.is_null() {
        bail!("language server returned no rename edit");
    }
    let mut raw = Vec::<(&str, &Value)>::new();
    if let Some(changes) = value.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            raw.push((uri, edits));
        }
    }
    if let Some(changes) = value.get("documentChanges").and_then(Value::as_array) {
        for change in changes {
            let uri = change
                .pointer("/textDocument/uri")
                .and_then(Value::as_str)
                .context("LSP resource operations are not accepted for rename")?;
            let edits = change.get("edits").context("LSP text edit omitted edits")?;
            raw.push((uri, edits));
        }
    }
    if raw.is_empty() {
        bail!("language server returned an empty rename edit");
    }

    let root = root
        .canonicalize()
        .context("canonicalize LSP workspace root")?;
    let mut out = Vec::new();
    for (uri, edits) in raw {
        let path = path_from_file_uri(uri)?;
        let canonical = path
            .canonicalize()
            .with_context(|| format!("canonicalize LSP edit path {}", path.display()))?;
        let relative = canonical.strip_prefix(&root).map_err(|_| {
            anyhow!(
                "language server attempted to edit path outside workspace: {}",
                canonical.display()
            )
        })?;
        let relative = normalize_safe_rel_path(relative, "LSP edit path")?;
        let source = fs::read_to_string(&canonical)
            .with_context(|| format!("read LSP edit target {}", canonical.display()))?;
        for edit in edits
            .as_array()
            .context("LSP document edits must be an array")?
        {
            let range = edit.get("range").context("LSP edit omitted range")?;
            let start = lsp_position_to_byte(
                &source,
                range.pointer("/start/line").and_then(Value::as_u64),
                range.pointer("/start/character").and_then(Value::as_u64),
            )?;
            let end = lsp_position_to_byte(
                &source,
                range.pointer("/end/line").and_then(Value::as_u64),
                range.pointer("/end/character").and_then(Value::as_u64),
            )?;
            if start > end {
                bail!("LSP rename edit has an inverted range");
            }
            out.push(WorkspaceTextEdit {
                rel_path: relative.to_string_lossy().replace('\\', "/"),
                start_byte: start,
                end_byte: end,
                new_text: edit
                    .get("newText")
                    .and_then(Value::as_str)
                    .context("LSP edit omitted newText")?
                    .to_string(),
            });
        }
    }
    out.sort_by(|left, right| {
        left.rel_path
            .cmp(&right.rel_path)
            .then(left.start_byte.cmp(&right.start_byte))
    });
    for pair in out.windows(2) {
        if pair[0].rel_path == pair[1].rel_path && pair[0].end_byte > pair[1].start_byte {
            bail!("language server returned overlapping rename edits");
        }
    }
    Ok(out)
}

fn parse_locations(root: &Path, value: &Value) -> Result<Vec<LspLocation>> {
    let locations = value
        .as_array()
        .context("language server returned invalid references result")?;
    let root = root
        .canonicalize()
        .context("canonicalize LSP workspace root")?;
    let mut out = Vec::with_capacity(locations.len());
    for location in locations {
        let uri = location
            .get("uri")
            .and_then(Value::as_str)
            .context("LSP location omitted uri")?;
        let path = path_from_file_uri(uri)?;
        let canonical = path
            .canonicalize()
            .with_context(|| format!("canonicalize LSP reference path {}", path.display()))?;
        let relative = canonical.strip_prefix(&root).map_err(|_| {
            anyhow!(
                "language server returned reference outside workspace: {}",
                canonical.display()
            )
        })?;
        let relative = normalize_safe_rel_path(relative, "LSP reference path")?;
        let source = fs::read_to_string(&canonical)
            .with_context(|| format!("read LSP reference target {}", canonical.display()))?;
        let range = location
            .get("range")
            .context("LSP location omitted range")?;
        let start_byte = lsp_position_to_byte(
            &source,
            range.pointer("/start/line").and_then(Value::as_u64),
            range.pointer("/start/character").and_then(Value::as_u64),
        )?;
        let end_byte = lsp_position_to_byte(
            &source,
            range.pointer("/end/line").and_then(Value::as_u64),
            range.pointer("/end/character").and_then(Value::as_u64),
        )?;
        let start = byte_to_line_column(&source, start_byte)?;
        let end = byte_to_line_column(&source, end_byte)?;
        out.push(LspLocation {
            rel_path: relative.to_string_lossy().replace('\\', "/"),
            range: TextRange {
                start_byte,
                end_byte,
                start_line: start.0,
                start_column: start.1,
                end_line: end.0,
                end_column: end.1,
            },
        });
    }
    out.sort_by(|left, right| {
        left.rel_path
            .cmp(&right.rel_path)
            .then(left.range.start_byte.cmp(&right.range.start_byte))
    });
    Ok(out)
}

fn byte_column_to_utf16(source: &str, zero_line: usize, byte_column: usize) -> Result<usize> {
    let line = source
        .lines()
        .nth(zero_line)
        .context("source line is outside document")?;
    if byte_column > line.len() || !line.is_char_boundary(byte_column) {
        bail!("source column is not on a UTF-8 boundary");
    }
    Ok(line[..byte_column].encode_utf16().count())
}

fn lsp_position_to_byte(source: &str, line: Option<u64>, character: Option<u64>) -> Result<usize> {
    let line = usize::try_from(line.context("LSP position omitted line")?)?;
    let character = usize::try_from(character.context("LSP position omitted character")?)?;
    let mut line_start = 0usize;
    for _ in 0..line {
        let newline = source[line_start..]
            .find('\n')
            .context("LSP position line is outside document")?;
        line_start += newline + 1;
    }
    let line_end = source[line_start..]
        .find('\n')
        .map(|offset| line_start + offset)
        .unwrap_or(source.len());
    let line_text = source[line_start..line_end]
        .strip_suffix('\r')
        .unwrap_or(&source[line_start..line_end]);
    let mut utf16 = 0usize;
    for (byte, ch) in line_text.char_indices() {
        if utf16 == character {
            return Ok(line_start + byte);
        }
        utf16 += ch.len_utf16();
        if utf16 > character {
            bail!("LSP position splits a UTF-16 surrogate pair");
        }
    }
    if utf16 == character {
        return Ok(line_start + line_text.len());
    }
    bail!("LSP position character is outside line")
}

fn byte_to_line_column(source: &str, offset: usize) -> Result<(usize, usize)> {
    if offset > source.len() || !source.is_char_boundary(offset) {
        bail!("LSP byte offset is not on a UTF-8 boundary");
    }
    let before = &source[..offset];
    let line = before.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    Ok((line, offset - line_start + 1))
}

fn file_uri(path: &Path) -> String {
    format!("file://{}", percent_encode(path.as_os_str()))
}

fn path_from_file_uri(uri: &str) -> Result<PathBuf> {
    let encoded = uri
        .strip_prefix("file://")
        .ok_or_else(|| anyhow!("LSP edit URI is not file://: {uri}"))?;
    let bytes = percent_decode(encoded.as_bytes())?;
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

fn percent_encode(value: &OsStr) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(*byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn percent_decode(value: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(value.len());
    let mut index = 0usize;
    while index < value.len() {
        if value[index] != b'%' {
            out.push(value[index]);
            index += 1;
            continue;
        }
        if index + 2 >= value.len() {
            bail!("invalid percent escape in file URI");
        }
        let hex = std::str::from_utf8(&value[index + 1..index + 3])?;
        out.push(u8::from_str_radix(hex, 16).context("decode file URI escape")?);
        index += 3;
    }
    Ok(out)
}

fn language_id(language: LanguageId) -> &'static str {
    match language {
        LanguageId::Rust => "rust",
        LanguageId::Cpp => "cpp",
        LanguageId::ObjectiveC => "objective-c",
        LanguageId::ObjectiveCpp => "objective-cpp",
        _ => "plaintext",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_positions_roundtrip_multibyte_text() {
        let source = "a😀b\nпривет\n";
        assert_eq!(lsp_position_to_byte(source, Some(0), Some(3)).unwrap(), 5);
        assert_eq!(lsp_position_to_byte(source, Some(1), Some(3)).unwrap(), 13);
        assert!(lsp_position_to_byte(source, Some(0), Some(2)).is_err());
    }

    #[test]
    fn file_uri_roundtrips_unicode_and_spaces() {
        let path = Path::new("/tmp/space dir/файл.rs");
        assert_eq!(path_from_file_uri(&file_uri(path)).unwrap(), path);
    }

    #[test]
    fn parses_workspace_edit_and_rejects_outside_paths() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("main.rs");
        fs::write(&file, "fn old() {}\n").unwrap();
        let value = json!({
            "changes": {
                (file_uri(&file)): [{
                    "range": {
                        "start": { "line": 0, "character": 3 },
                        "end": { "line": 0, "character": 6 }
                    },
                    "newText": "new"
                }]
            }
        });
        let edits = parse_workspace_edit(root.path(), &value).unwrap();
        assert_eq!(edits[0].rel_path, "main.rs");
        assert_eq!((edits[0].start_byte, edits[0].end_byte), (3, 6));
    }
}
