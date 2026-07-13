//! Small synchronous LSP client used only for semantic refactors.
//!
//! Requests run without the workspace lock. Callers must compare the semantic
//! revision again before persisting or committing the returned workspace edit.

use crate::config::normalize_safe_rel_path;
use crate::semantic::{LanguageId, TextRange};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
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
const REFERENCE_READY_ATTEMPTS: usize = 120;
const REFERENCE_RETRY_DELAY: Duration = Duration::from_millis(250);
const REFERENCE_STABLE_POLLS: usize = 3;
const REFERENCE_NO_PROGRESS_GRACE: Duration = Duration::from_secs(2);
const REFERENCE_BATCH_SIZE: usize = 8;
const REFERENCE_VALIDATION_WINDOW: usize = 64;
const REFERENCE_BATCH_TIMEOUT: Duration = Duration::from_secs(120);

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

#[derive(Clone)]
pub(crate) struct ReferenceBatchRequest {
    pub rel_path: PathBuf,
    pub source: Arc<str>,
    pub language: LanguageId,
    pub declaration: TextRange,
    pub minimum_references: usize,
}

pub fn references(
    root: &Path,
    rel_path: &Path,
    source: &str,
    language: LanguageId,
    declaration: &TextRange,
    minimum_references: usize,
) -> Result<Vec<LspLocation>> {
    let request = ReferenceBatchRequest {
        rel_path: rel_path.to_path_buf(),
        source: Arc::from(source),
        language,
        declaration: declaration.clone(),
        minimum_references,
    };
    references_batch(root, &[request], |_, _| {})?
        .into_iter()
        .next()
        .context("language server reference batch omitted its only result")
}

/// Resolve many declaration reference closures through one initialized
/// language-server session. Documents are opened once and reference requests
/// are multiplexed in bounded batches; every result still has to remain stable
/// until the same background-index quiescence rule used by single requests is
/// satisfied.
pub(crate) fn references_batch(
    root: &Path,
    requests: &[ReferenceBatchRequest],
    mut progress: impl FnMut(usize, usize),
) -> Result<Vec<Vec<LspLocation>>> {
    if requests.is_empty() {
        return Ok(Vec::new());
    }
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalize workspace root {}", root.display()))?;
    let mut raw_results = vec![None::<Value>; requests.len()];
    let mut completed = 0usize;
    progress(0, requests.len());

    for server in [Server::Clangd, Server::RustAnalyzer] {
        let mut group = Vec::new();
        for (index, request) in requests.iter().enumerate() {
            if Server::kind_for_language(request.language)? == server {
                group.push((index, request));
            }
        }
        if group.is_empty() {
            continue;
        }
        if !server.available() {
            bail!(
                "{} is unavailable; semantic rename is disabled",
                server.command()
            );
        }
        let group_results = with_client(server, &root, |client| {
            let mut documents = BTreeMap::<String, (LanguageId, Arc<str>)>::new();
            for (_, request) in &group {
                let target = root.join(&request.rel_path);
                let uri = file_uri(&target);
                match documents.entry(uri) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert((request.language, Arc::clone(&request.source)));
                    }
                    std::collections::btree_map::Entry::Occupied(entry)
                        if entry.get().0 != request.language
                            || entry.get().1.as_ref() != request.source.as_ref() =>
                    {
                        bail!(
                            "one LSP document was requested with incompatible language/content snapshots"
                        );
                    }
                    std::collections::btree_map::Entry::Occupied(_) => {}
                }
            }
            for (uri, (language, source)) in &documents {
                client.notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": language_id(*language),
                            "version": 1,
                            "text": source.as_ref(),
                        }
                    }),
                )?;
            }
            client.wait_for_documents(&documents.keys().cloned().collect())?;

            let params = group
                .iter()
                .map(|(_, request)| {
                    let target = root.join(&request.rel_path);
                    Ok(json!({
                        "textDocument": { "uri": file_uri(&target) },
                        "position": {
                            "line": request.declaration.start_line.saturating_sub(1),
                            "character": byte_column_to_utf16(
                                &request.source,
                                request.declaration.start_line.saturating_sub(1),
                                request.declaration.start_column.saturating_sub(1),
                            )?,
                        },
                        "context": { "includeDeclaration": true }
                    }))
                })
                .collect::<Result<Vec<_>>>()?;
            let minimums = group
                .iter()
                .map(|(_, request)| request.minimum_references)
                .collect::<Vec<_>>();
            let result = (|| -> Result<Vec<Value>> {
                let mut results = Vec::with_capacity(params.len());
                for start in (0..params.len()).step_by(REFERENCE_VALIDATION_WINDOW) {
                    let end = (start + REFERENCE_VALIDATION_WINDOW).min(params.len());
                    let window = client.request_references_batch_until(
                        &params[start..end],
                        &minimums[start..end],
                        |window_done| progress(completed + start + window_done, requests.len()),
                    )?;
                    results.extend(window);
                }
                Ok(results)
            })();
            for uri in documents.keys() {
                let _ = client.notify(
                    "textDocument/didClose",
                    json!({ "textDocument": { "uri": uri } }),
                );
            }
            result
        })?;
        for ((index, _), result) in group.into_iter().zip(group_results) {
            raw_results[index] = Some(result);
        }
        completed = raw_results.iter().filter(|result| result.is_some()).count();
        progress(completed, requests.len());
    }

    let mut source_cache = HashMap::new();
    raw_results
        .into_iter()
        .enumerate()
        .map(|(index, result)| {
            parse_locations_cached(
                &root,
                &result.with_context(|| format!("reference result {index} was not produced"))?,
                &mut source_cache,
            )
        })
        .collect()
}

pub fn rename(
    root: &Path,
    rel_path: &Path,
    source: &str,
    language: LanguageId,
    declaration: &TextRange,
    new_name: &str,
    minimum_references: usize,
) -> Result<Vec<WorkspaceTextEdit>> {
    let server = Server::for_language(language)?;
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalize workspace root {}", root.display()))?;
    let target = root.join(rel_path);
    let uri = file_uri(&target);
    let position = json!({
        "line": declaration.start_line.saturating_sub(1),
        "character": byte_column_to_utf16(
            source,
            declaration.start_line.saturating_sub(1),
            declaration.start_column.saturating_sub(1),
        )?,
    });
    let params = json!({
        "textDocument": { "uri": file_uri(&target) },
        "position": position,
        "newName": new_name,
    });
    let (references, result) = with_client(server, &root, |client| {
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
        let references = client.request_references_until(
            json!({
                "textDocument": { "uri": file_uri(&target) },
                "position": position,
                "context": { "includeDeclaration": true }
            }),
            minimum_references,
        )?;
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
        Ok((
            references,
            result.context("language server did not return a rename result")?,
        ))
    })?;
    let mut edits = parse_workspace_edit(&root, &result)?;
    let references = parse_locations(&root, &references)?;
    let original_name = source
        .get(declaration.start_byte..declaration.end_byte)
        .context("symbol declaration range is not valid UTF-8")?;
    complete_rename_edits(&root, &references, &mut edits, original_name, new_name)?;
    Ok(edits)
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
    struct CachedClient {
        generation: String,
        client: Arc<Mutex<Client>>,
    }
    static CLIENTS: OnceLock<Mutex<HashMap<String, CachedClient>>> = OnceLock::new();
    let key = format!("{}:{}", server.command(), root.display());
    let generation = lsp_workspace_generation(root);
    let client = {
        let mut clients = CLIENTS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .map_err(|_| anyhow!("LSP client cache lock was poisoned"))?;
        match clients.get(&key) {
            Some(cached) if cached.generation == generation => Arc::clone(&cached.client),
            _ => {
                let client = Arc::new(Mutex::new(Client::spawn(server, root)?));
                clients.insert(
                    key,
                    CachedClient {
                        generation,
                        client: Arc::clone(&client),
                    },
                );
                client
            }
        }
    };
    let mut client = client
        .lock()
        .map_err(|_| anyhow!("LSP workspace client lock was poisoned"))?;
    operation(&mut client)
}

fn lsp_workspace_generation(root: &Path) -> String {
    let layout = crate::config::Layout::new(root);
    let documents = if layout.db_path.is_file() {
        crate::db::connect(&layout)
            .and_then(|conn| crate::semantic_store::document_fingerprint(&conn))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let compile_commands = discover_compilation_database(root)
        .and_then(|directory| fs::metadata(directory.join("compile_commands.json")).ok())
        .map(|metadata| {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |duration| duration.as_nanos());
            format!("{}:{modified}", metadata.len())
        })
        .unwrap_or_default();
    format!("{documents}:{compile_commands}")
}

impl Server {
    fn for_language(language: LanguageId) -> Result<Self> {
        let server = Self::kind_for_language(language)?;
        if !server.available() {
            bail!(
                "{} is unavailable; semantic rename is disabled",
                server.command()
            );
        }
        Ok(server)
    }

    fn kind_for_language(language: LanguageId) -> Result<Self> {
        match language {
            LanguageId::Rust => Ok(Self::RustAnalyzer),
            LanguageId::Cpp | LanguageId::ObjectiveC | LanguageId::ObjectiveCpp => Ok(Self::Clangd),
            _ => bail!("no LSP rename backend for {language:?}"),
        }
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

    fn args(self, root: &Path) -> Vec<OsString> {
        match self {
            Self::RustAnalyzer => Vec::new(),
            Self::Clangd => {
                let mut args = vec![
                    OsString::from("--background-index"),
                    OsString::from("--background-index-priority=low"),
                    OsString::from("--clang-tidy=false"),
                    OsString::from("--header-insertion=never"),
                    OsString::from("--pch-storage=memory"),
                ];
                if let Some(directory) = discover_compilation_database(root) {
                    let mut argument = OsString::from("--compile-commands-dir=");
                    argument.push(directory);
                    args.push(argument);
                }
                args
            }
        }
    }
}

fn reference_result_fingerprint(value: &Value) -> Result<String> {
    let locations = value
        .as_array()
        .context("language server returned invalid references result")?;
    let mut normalized = locations
        .iter()
        .map(|location| {
            format!(
                "{}:{}:{}:{}:{}",
                location
                    .get("uri")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                location
                    .pointer("/range/start/line")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
                location
                    .pointer("/range/start/character")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
                location
                    .pointer("/range/end/line")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
                location
                    .pointer("/range/end/character")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>();
    normalized.sort();
    Ok(normalized.join("\n"))
}

fn discover_compilation_database(root: &Path) -> Option<PathBuf> {
    for candidate in [
        root.to_path_buf(),
        root.join("build"),
        root.join("out/build"),
        root.join("cmake-build-debug"),
        root.join("cmake-build-release"),
    ] {
        if candidate.join("compile_commands.json").is_file() {
            return candidate.canonicalize().ok().or(Some(candidate));
        }
    }
    find_compilation_database(root, 0, 3)
}

fn find_compilation_database(directory: &Path, depth: usize, maximum: usize) -> Option<PathBuf> {
    if depth > maximum {
        return None;
    }
    let mut entries = fs::read_dir(directory)
        .ok()?
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if matches!(
            name.as_ref(),
            ".git" | ".code-sanity" | "node_modules" | "target" | "vendor" | "third_party"
        ) {
            continue;
        }
        let path = entry.path();
        if path.join("compile_commands.json").is_file() {
            return path.canonicalize().ok().or(Some(path));
        }
        if let Some(found) = find_compilation_database(&path, depth + 1, maximum) {
            return Some(found);
        }
    }
    None
}

struct Client {
    server: Server,
    child: Child,
    stdin: ChildStdin,
    messages: Receiver<std::result::Result<Value, String>>,
    next_id: u64,
    active_progress: BTreeSet<String>,
    saw_progress: bool,
    last_progress_end: Option<std::time::Instant>,
}

impl Client {
    fn spawn(server: Server, root: &Path) -> Result<Self> {
        let mut child = Command::new(server.command())
            .args(server.args(root))
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
            server,
            child,
            stdin,
            messages,
            next_id: 1,
            active_progress: BTreeSet::new(),
            saw_progress: false,
            last_progress_end: None,
        };
        client.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": file_uri(root),
                "workspaceFolders": [{ "uri": file_uri(root), "name": "workspace" }],
                "capabilities": {
                    "workspace": { "workspaceEdit": { "documentChanges": true } },
                    "textDocument": { "rename": { "prepareSupport": false } },
                    "window": { "workDoneProgress": true }
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
            self.observe_notification(&message);
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

    fn request_references_until(&mut self, params: Value, minimum: usize) -> Result<Value> {
        let mut observed = 0usize;
        let mut last_fingerprint = None::<String>;
        let mut stable_polls = 0usize;
        let mut satisfactory_since = None::<std::time::Instant>;
        for attempt in 0..REFERENCE_READY_ATTEMPTS {
            let value =
                self.request_retry_content_modified("textDocument/references", params.clone())?;
            // LSP explicitly permits `null` while a server has no references
            // ready. Treat it as the transient empty set so the bounded
            // readiness loop can converge instead of turning startup timing
            // into a flaky hard error.
            let value = if value.is_null() {
                Value::Array(Vec::new())
            } else {
                value
            };
            observed = value.as_array().map_or(0, Vec::len);
            let fingerprint = reference_result_fingerprint(&value)?;
            if last_fingerprint.as_deref() == Some(fingerprint.as_str()) {
                stable_polls += 1;
            } else {
                last_fingerprint = Some(fingerprint);
                stable_polls = 1;
            }
            if observed >= minimum {
                let started = *satisfactory_since.get_or_insert_with(std::time::Instant::now);
                let progress_ready = if self.server == Server::Clangd {
                    if self.saw_progress {
                        self.active_progress.is_empty()
                            && self.last_progress_end.map_or_else(
                                || started.elapsed() >= REFERENCE_NO_PROGRESS_GRACE,
                                |ended| ended.elapsed() >= REFERENCE_RETRY_DELAY,
                            )
                    } else {
                        started.elapsed() >= REFERENCE_NO_PROGRESS_GRACE
                    }
                } else {
                    // rust-analyzer does not consistently publish progress for
                    // a newly-created module. A result can look stable while
                    // its workspace index still contains only the declaration,
                    // so give it the same bounded quiescence grace as a server
                    // without progress notifications.
                    started.elapsed() >= REFERENCE_NO_PROGRESS_GRACE
                };
                if stable_polls >= REFERENCE_STABLE_POLLS && progress_ready {
                    return Ok(value);
                }
            } else {
                satisfactory_since = None;
            }
            if attempt + 1 < REFERENCE_READY_ATTEMPTS {
                std::thread::sleep(REFERENCE_RETRY_DELAY);
            }
        }
        bail!(
            "language server reported only {observed} stable reference(s), below the semantic index minimum {minimum} or before background indexing became quiescent; refusing incomplete result"
        )
    }

    fn request_references_batch_until(
        &mut self,
        params: &[Value],
        minimums: &[usize],
        mut progress: impl FnMut(usize),
    ) -> Result<Vec<Value>> {
        if params.len() != minimums.len() {
            bail!("reference batch parameter/minimum counts differ");
        }
        struct PollState {
            observed: usize,
            last_fingerprint: Option<String>,
            stable_polls: usize,
            satisfactory_since: Option<std::time::Instant>,
            last_value: Option<Value>,
            content_modified_retries: usize,
            done: bool,
        }
        let mut states = (0..params.len())
            .map(|_| PollState {
                observed: 0,
                last_fingerprint: None,
                stable_polls: 0,
                satisfactory_since: None,
                last_value: None,
                content_modified_retries: 0,
                done: false,
            })
            .collect::<Vec<_>>();
        progress(0);
        let deadline = std::time::Instant::now() + REFERENCE_BATCH_TIMEOUT;

        for attempt in 0..REFERENCE_READY_ATTEMPTS {
            if std::time::Instant::now() >= deadline {
                break;
            }
            self.wait_for_clangd_background_quiescence(deadline)?;
            let pending = states
                .iter()
                .enumerate()
                .filter_map(|(index, state)| (!state.done).then_some(index))
                .collect::<Vec<_>>();
            for chunk in pending.chunks(REFERENCE_BATCH_SIZE) {
                self.wait_for_clangd_background_quiescence(deadline)?;
                let mut request_ids = HashMap::<u64, usize>::with_capacity(chunk.len());
                for &index in chunk {
                    let id = self.next_id;
                    self.next_id += 1;
                    self.write(&json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "method": "textDocument/references",
                        "params": params[index],
                    }))?;
                    request_ids.insert(id, index);
                }
                while !request_ids.is_empty() {
                    let message = self
                        .messages
                        .recv_timeout(RESPONSE_TIMEOUT)
                        .map_err(|err| {
                            anyhow!(
                                "LSP reference batch timed out or disconnected with {} response(s) pending: {err}",
                                request_ids.len()
                            )
                        })?
                        .map_err(|err| anyhow!("read LSP response: {err}"))?;
                    self.observe_notification(&message);
                    let response_id = message.get("id").and_then(Value::as_u64);
                    let Some(index) = response_id.and_then(|id| request_ids.remove(&id)) else {
                        if message.get("method").and_then(Value::as_str).is_some()
                            && message.get("id").is_some()
                        {
                            self.respond_to_server_request(&message)?;
                        }
                        continue;
                    };
                    if let Some(error) = message.get("error") {
                        if error.to_string().contains("content modified")
                            && states[index].content_modified_retries < 2
                        {
                            states[index].content_modified_retries += 1;
                            continue;
                        }
                        bail!("LSP textDocument/references failed for batch item {index}: {error}");
                    }
                    states[index].content_modified_retries = 0;
                    let value = message.get("result").cloned().unwrap_or(Value::Null);
                    let value = if value.is_null() {
                        Value::Array(Vec::new())
                    } else {
                        value
                    };
                    let observed = value.as_array().map_or(0, Vec::len);
                    let fingerprint = reference_result_fingerprint(&value)?;
                    let state = &mut states[index];
                    state.observed = observed;
                    if state.last_fingerprint.as_deref() == Some(fingerprint.as_str()) {
                        state.stable_polls += 1;
                    } else {
                        state.last_fingerprint = Some(fingerprint);
                        state.stable_polls = 1;
                    }
                    if observed >= minimums[index] {
                        state
                            .satisfactory_since
                            .get_or_insert_with(std::time::Instant::now);
                    } else {
                        state.satisfactory_since = None;
                    }
                    state.last_value = Some(value);
                }
            }

            for (index, state) in states.iter_mut().enumerate() {
                if state.done || state.last_value.is_none() || state.observed < minimums[index] {
                    continue;
                }
                let Some(started) = state.satisfactory_since else {
                    continue;
                };
                let progress_ready = if self.server == Server::Clangd && self.saw_progress {
                    self.active_progress.is_empty()
                        && self.last_progress_end.map_or_else(
                            || started.elapsed() >= REFERENCE_NO_PROGRESS_GRACE,
                            |ended| ended.elapsed() >= REFERENCE_RETRY_DELAY,
                        )
                } else {
                    started.elapsed() >= REFERENCE_NO_PROGRESS_GRACE
                };
                if state.stable_polls >= REFERENCE_STABLE_POLLS && progress_ready {
                    state.done = true;
                }
            }
            let done = states.iter().filter(|state| state.done).count();
            progress(done);
            if done == states.len() {
                return states
                    .into_iter()
                    .enumerate()
                    .map(|(index, state)| {
                        state.last_value.with_context(|| {
                            format!("stable reference batch item {index} omitted its result")
                        })
                    })
                    .collect();
            }
            if attempt + 1 < REFERENCE_READY_ATTEMPTS {
                std::thread::sleep(REFERENCE_RETRY_DELAY);
            }
        }
        let (index, state) = states
            .iter()
            .enumerate()
            .find(|(_, state)| !state.done)
            .expect("unfinished reference batch has an item");
        bail!(
            "language server reported only {} stable reference(s) for batch item {index}, below the semantic index minimum {} or before background indexing became quiescent; refusing incomplete result",
            state.observed,
            minimums[index]
        )
    }

    fn wait_for_clangd_background_quiescence(
        &mut self,
        deadline: std::time::Instant,
    ) -> Result<()> {
        if self.server != Server::Clangd || !self.saw_progress {
            return Ok(());
        }
        let started = std::time::Instant::now();
        loop {
            let ready = self.active_progress.is_empty()
                && self.last_progress_end.map_or_else(
                    || started.elapsed() >= REFERENCE_NO_PROGRESS_GRACE,
                    |ended| ended.elapsed() >= REFERENCE_RETRY_DELAY,
                );
            if ready {
                return Ok(());
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                bail!(
                    "clangd background indexing did not become quiescent within {} seconds; refusing potentially incomplete references",
                    REFERENCE_BATCH_TIMEOUT.as_secs()
                );
            }
            let wait = deadline
                .saturating_duration_since(now)
                .min(REFERENCE_RETRY_DELAY);
            match self.messages.recv_timeout(wait) {
                Ok(message) => {
                    let message = message.map_err(|err| anyhow!("read LSP response: {err}"))?;
                    self.observe_notification(&message);
                    if message.get("method").and_then(Value::as_str).is_some()
                        && message.get("id").is_some()
                    {
                        self.respond_to_server_request(&message)?;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(error) => {
                    return Err(anyhow!(
                        "LSP disconnected while waiting for background indexing: {error}"
                    ));
                }
            }
        }
    }

    fn observe_notification(&mut self, message: &Value) {
        if message.get("method").and_then(Value::as_str) != Some("$/progress") {
            return;
        }
        let token = message
            .pointer("/params/token")
            .map(Value::to_string)
            .unwrap_or_default();
        match message
            .pointer("/params/value/kind")
            .and_then(Value::as_str)
        {
            Some("begin") => {
                self.saw_progress = true;
                self.active_progress.insert(token);
            }
            Some("report") => {
                self.saw_progress = true;
            }
            Some("end") => {
                self.saw_progress = true;
                self.active_progress.remove(&token);
                self.last_progress_end = Some(std::time::Instant::now());
            }
            _ => {}
        }
    }

    fn wait_for_document(&mut self, uri: &str) -> Result<()> {
        self.wait_for_documents(&BTreeSet::from([uri.to_string()]))
    }

    fn wait_for_documents(&mut self, uris: &BTreeSet<String>) -> Result<()> {
        let mut pending = uris.clone();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !pending.is_empty() && std::time::Instant::now() < deadline {
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
            self.observe_notification(&message);
            if message.get("method").and_then(Value::as_str)
                == Some("textDocument/publishDiagnostics")
            {
                if let Some(uri) = message.pointer("/params/uri").and_then(Value::as_str) {
                    pending.remove(uri);
                }
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
    let root = root
        .canonicalize()
        .context("canonicalize LSP workspace root")?;
    parse_locations_cached(&root, value, &mut HashMap::new())
}

fn parse_locations_cached(
    canonical_root: &Path,
    value: &Value,
    source_cache: &mut HashMap<String, (String, Arc<str>)>,
) -> Result<Vec<LspLocation>> {
    let locations = value
        .as_array()
        .context("language server returned invalid references result")?;
    let mut out = Vec::with_capacity(locations.len());
    for location in locations {
        let uri = location
            .get("uri")
            .and_then(Value::as_str)
            .context("LSP location omitted uri")?;
        let (relative, source) = match source_cache.get(uri) {
            Some(cached) => cached.clone(),
            None => {
                let path = path_from_file_uri(uri)?;
                let canonical = path.canonicalize().with_context(|| {
                    format!("canonicalize LSP reference path {}", path.display())
                })?;
                let relative = canonical.strip_prefix(canonical_root).map_err(|_| {
                    anyhow!(
                        "language server returned reference outside workspace: {}",
                        canonical.display()
                    )
                })?;
                let relative = normalize_safe_rel_path(relative, "LSP reference path")?
                    .to_string_lossy()
                    .replace('\\', "/");
                let source: Arc<str> =
                    Arc::from(fs::read_to_string(&canonical).with_context(|| {
                        format!("read LSP reference target {}", canonical.display())
                    })?);
                source_cache.insert(uri.to_string(), (relative.clone(), Arc::clone(&source)));
                (relative, source)
            }
        };
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
            rel_path: relative,
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

fn complete_rename_edits(
    root: &Path,
    references: &[LspLocation],
    edits: &mut Vec<WorkspaceTextEdit>,
    original_name: &str,
    new_name: &str,
) -> Result<()> {
    for reference in references {
        let covered = edits.iter().any(|edit| {
            edit.rel_path == reference.rel_path
                && edit.start_byte <= reference.range.start_byte
                && edit.end_byte >= reference.range.end_byte
        });
        if covered {
            continue;
        }
        let rel = normalize_safe_rel_path(Path::new(&reference.rel_path), "LSP reference path")?;
        let source = fs::read_to_string(root.join(&rel))
            .with_context(|| format!("read uncovered LSP reference {}", reference.rel_path))?;
        let referenced_text = source
            .get(reference.range.start_byte..reference.range.end_byte)
            .context("LSP reference range is not valid UTF-8")?;
        if referenced_text != original_name {
            bail!(
                "language server omitted a non-trivial rename edit for {}:{}; refusing partial rename",
                reference.rel_path,
                reference.range.start_line
            );
        }
        edits.push(WorkspaceTextEdit {
            rel_path: reference.rel_path.clone(),
            start_byte: reference.range.start_byte,
            end_byte: reference.range.end_byte,
            new_text: new_name.to_string(),
        });
    }
    edits.sort_by(|left, right| {
        left.rel_path
            .cmp(&right.rel_path)
            .then(left.start_byte.cmp(&right.start_byte))
            .then(left.end_byte.cmp(&right.end_byte))
    });
    edits.dedup();
    for pair in edits.windows(2) {
        if pair[0].rel_path == pair[1].rel_path && pair[0].end_byte > pair[1].start_byte {
            bail!("completed language-server rename edits overlap");
        }
    }
    Ok(())
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
    fn clangd_discovers_nested_cmake_compilation_database() {
        let root = tempfile::tempdir().unwrap();
        let build = root.path().join("build");
        fs::create_dir_all(&build).unwrap();
        fs::write(build.join("compile_commands.json"), "[]\n").unwrap();
        assert_eq!(
            discover_compilation_database(root.path()).unwrap(),
            build.canonicalize().unwrap()
        );
        let arguments = Server::Clangd.args(root.path());
        assert!(arguments.iter().any(|argument| {
            argument
                .to_string_lossy()
                .starts_with("--compile-commands-dir=")
        }));
        assert!(arguments.contains(&OsString::from("--background-index")));
    }

    #[test]
    fn batched_clangd_references_share_one_open_document() {
        if !Server::Clangd.available() {
            return;
        }
        let repo = tempfile::tempdir().unwrap();
        fs::write(repo.path().join("compile_flags.txt"), "-std=c++20\n").unwrap();
        let source: Arc<str> = Arc::from(
            "int first(int value) { return value + 1; }\n\
             int second(int value) { return value + 2; }\n\
             int main() { return first(1) + second(2); }\n",
        );
        fs::write(repo.path().join("main.cpp"), source.as_ref()).unwrap();
        let declaration = |name: &str| {
            let start_byte = source.find(name).unwrap();
            let end_byte = start_byte + name.len();
            let start = byte_to_line_column(&source, start_byte).unwrap();
            let end = byte_to_line_column(&source, end_byte).unwrap();
            TextRange {
                start_byte,
                end_byte,
                start_line: start.0,
                start_column: start.1,
                end_line: end.0,
                end_column: end.1,
            }
        };
        let requests = ["first", "second"].map(|name| ReferenceBatchRequest {
            rel_path: PathBuf::from("main.cpp"),
            source: Arc::clone(&source),
            language: LanguageId::Cpp,
            declaration: declaration(name),
            minimum_references: 2,
        });
        let mut observed_progress = Vec::new();
        let references = references_batch(repo.path(), &requests, |completed, total| {
            observed_progress.push((completed, total));
        })
        .unwrap();
        assert_eq!(references.len(), 2);
        assert!(references.iter().all(|locations| locations.len() == 2));
        assert_eq!(observed_progress.last(), Some(&(2, 2)));
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

    #[test]
    fn rename_completion_adds_only_exact_compiler_references() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("main.rs"),
            "let hwid = get();\nuse_it(hwid);\n",
        )
        .unwrap();
        let references = vec![
            LspLocation {
                rel_path: "main.rs".to_string(),
                range: TextRange {
                    start_byte: 4,
                    end_byte: 8,
                    start_line: 1,
                    start_column: 5,
                    end_line: 1,
                    end_column: 9,
                },
            },
            LspLocation {
                rel_path: "main.rs".to_string(),
                range: TextRange {
                    start_byte: 25,
                    end_byte: 29,
                    start_line: 2,
                    start_column: 8,
                    end_line: 2,
                    end_column: 12,
                },
            },
        ];
        let mut edits = vec![WorkspaceTextEdit {
            rel_path: "main.rs".to_string(),
            start_byte: 4,
            end_byte: 8,
            new_text: "device_id".to_string(),
        }];
        complete_rename_edits(root.path(), &references, &mut edits, "hwid", "device_id").unwrap();
        assert_eq!(edits.len(), 2);
        assert_eq!((edits[1].start_byte, edits[1].end_byte), (25, 29));
    }
}
