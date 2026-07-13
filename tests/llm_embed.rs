//! Integration tests for the OpenAI-compatible LLM proposal provider
//! (kou-router-style chat endpoint) and the OpenRouter-style embedding index,
//! against a local mock server speaking the OpenAI wire format.

use code_sanity::config::{Config, Layout, ProviderConfig};
use code_sanity::index_workspace;
use code_sanity::proposal::{
    ProposeProgress, ProviderAllow, propose_sanitize, propose_sanitize_with_progress,
};
use code_sanity::read_sanitized_file;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

type Handler = dyn Fn(&str, &Value) -> Value + Send + Sync;
type StatusHandler = dyn Fn(&str, &Value) -> (u16, Value) + Send + Sync;

/// Minimal HTTP/1.1 mock: one response per connection, JSON in/out. Returns
/// the base URL to point clients at. The listener thread lives until the test
/// process exits (it blocks on accept), which is fine for tests.
fn spawn_mock_server(handler: Arc<Handler>) -> String {
    spawn_mock_server_with_status(Arc::new(move |path: &str, request: &Value| {
        (200, handler(path, request))
    }))
}

fn spawn_mock_server_with_status(handler: Arc<StatusHandler>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let handler = Arc::clone(&handler);
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    return;
                }
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            content_length = value.trim().parse().unwrap_or(0);
                        }
                    }
                }
                let mut body = vec![0u8; content_length];
                if reader.read_exact(&mut body).is_err() {
                    return;
                }
                let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let (status, response) = handler(&path, &request);
                let response = response.to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 {} MockStatus\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    status,
                    response.len(),
                    response
                );
                let _ = stream.flush();
            });
        }
    });
    format!("http://{addr}/v1")
}

/// Deterministic fake embedding: one dimension per keyword, counting
/// occurrences. Cosine ranking then follows vocabulary overlap exactly.
const KEYWORDS: [&str; 6] = ["parser", "grammar", "token", "socket", "network", "stream"];

fn keyword_embedding(text: &str) -> Vec<f64> {
    let lower = text.to_lowercase();
    KEYWORDS
        .iter()
        .map(|keyword| lower.matches(keyword).count() as f64)
        .collect()
}

fn embeddings_response(request: &Value) -> Value {
    let inputs: Vec<String> = match &request["input"] {
        Value::Array(items) => items
            .iter()
            .map(|item| item.as_str().unwrap_or_default().to_string())
            .collect(),
        Value::String(single) => vec![single.clone()],
        _ => Vec::new(),
    };
    let data: Vec<Value> = inputs
        .iter()
        .enumerate()
        .map(|(index, text)| json!({ "index": index, "embedding": keyword_embedding(text) }))
        .collect();
    json!({ "data": data, "model": request["model"] })
}

fn chat_response(content: &str) -> Value {
    json!({
        "choices": [ { "message": { "role": "assistant", "content": content } } ]
    })
}

fn targeted_chat_response(request: &Value, proposals: &[(&str, &str, f64)]) -> Value {
    let user: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    let candidates = user["context"]["semantic_candidates"].as_array().unwrap();
    if candidates.is_empty() {
        return chat_response("{\"proposals\":[]}");
    }
    let proposals = proposals
        .iter()
        .map(|(original, replacement, confidence)| {
            let candidate = candidates
                .iter()
                .find(|candidate| candidate["name"].as_str() == Some(original))
                .unwrap_or_else(|| panic!("missing semantic candidate {original}: {candidates:?}"));
            json!({
                "target": {
                    "symbol_id": candidate["symbol_id"],
                    "occurrence_id": candidate["occurrence_id"],
                },
                "category": "identifier",
                "original_text": original,
                "sanitized_text": replacement,
                "confidence": confidence,
            })
        })
        .collect::<Vec<_>>();
    chat_response(&json!({ "proposals": proposals }).to_string())
}

#[test]
fn llm_provider_requires_endpoint_confirmation_and_queues_proposals() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(
        repo.path().join("src/lib.rs"),
        "// dangerous COMMENT_ONLY_TRIGGER implementation detail\nfn client_count() -> usize {\n    let megacorp_client = 1;\n    megacorp_client\n}\n",
    )
    .unwrap();
    index_workspace(repo.path()).unwrap();

    let chat_requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&chat_requests);
    let base_url = spawn_mock_server(Arc::new(move |path: &str, request: &Value| {
        assert!(
            path.ends_with("/chat/completions"),
            "unexpected path {path}"
        );
        let user = request["messages"][1]["content"].as_str().unwrap();
        let task: Value = serde_json::from_str(user).unwrap();
        counter.fetch_add(1, Ordering::SeqCst);
        if task["request_mode"] == "path-only" {
            assert_eq!(task["file"]["content"], "");
            assert!(
                task["context"]["semantic_candidates"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            assert!(
                !task["context"]["path_candidates"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            return chat_response("{\"proposals\":[]}");
        }

        // Source requests carry real file content but no path inventory.
        assert!(user.contains("megacorp_client"));
        assert!(
            !user.contains("dangerous"),
            "known deterministic terms must be redacted before the provider boundary"
        );
        assert!(
            task["context"]["path_candidates"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            task["file"]["content"]
                .as_str()
                .unwrap()
                .contains("megacorp_client")
        );
        assert!(
            !task["file"]["content"]
                .as_str()
                .unwrap()
                .contains("COMMENT_ONLY_TRIGGER")
        );
        assert!(task["task"].as_str().unwrap().contains("security/abuse"));
        assert!(
            task["rules"]
                .as_array()
                .unwrap()
                .iter()
                .any(|rule| rule.as_str().unwrap().contains("[A-Za-z_][A-Za-z0-9_]*"))
        );
        assert!(
            task["rules"]
                .as_array()
                .unwrap()
                .iter()
                .any(|rule| rule.as_str().unwrap().contains("byte-for-byte"))
        );
        assert!(task["policy"].get("content_allowlist").is_none());
        assert!(task["rules"].as_array().unwrap().iter().any(|rule| {
            rule.as_str()
                .unwrap()
                .contains("content allowlist is not a proposal exclusion")
        }));
        assert!(
            task["required_output_preflight"]
                .as_array()
                .unwrap()
                .iter()
                .any(|rule| rule.as_str().unwrap().contains("case-sensitive substring"))
        );
        let candidate = task["context"]["semantic_candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|candidate| candidate["name"] == "megacorp_client")
            .unwrap();
        chat_response(
            &json!({
                "proposals": [
                    {
                        "target": {
                            "symbol_id": candidate["symbol_id"],
                            "occurrence_id": candidate["occurrence_id"]
                        },
                        "category": "identifier",
                        "original_text": "megacorp_client",
                        "sanitized_text": "examplefirm_client",
                        "confidence": 0.95
                    },
                    {
                        "category": "identifier",
                        "original_text": "COMMENT_ONLY_TRIGGER",
                        "sanitized_text": "documentation_note",
                        "confidence": 0.95
                    },
                    {
                        "category": "identifier",
                        "original_text": "ghost_term",
                        "sanitized_text": "nothing",
                        "confidence": 0.9
                    }
                ]
            })
            .to_string(),
        )
    }));

    let layout = Layout::new(repo.path());
    let mut config = Config::load_or_default(&layout).unwrap();
    config.sanitizer.provider = ProviderConfig::Llm {
        base_url,
        model: "test-model".to_string(),
        api_key_env: "CODE_SANITY_TEST_KEY_UNSET".to_string(),
        timeout_secs: Some(10),
        json_mode: false,
    };
    config.save(&layout).unwrap();

    // A repo-configured endpoint receiving real content requires confirmation.
    let refused = propose_sanitize(
        repo.path(),
        Some(Path::new("src/lib.rs")),
        ProviderAllow::default(),
    )
    .unwrap_err();
    assert!(refused.to_string().contains("--allow-provider-endpoint"));
    assert_eq!(chat_requests.load(Ordering::SeqCst), 0);

    let report = propose_sanitize(
        repo.path(),
        Some(Path::new("src/lib.rs")),
        ProviderAllow {
            command: false,
            endpoint: true,
        },
    )
    .unwrap();
    assert_eq!(chat_requests.load(Ordering::SeqCst), 2);
    assert_eq!(report.proposed, 3);
    assert_eq!(report.queued, 1);
    assert_eq!(report.rejected.len(), 2);

    // The model never wrote the mirror; approval routes through the registry.
    let mirror = read_sanitized_file(repo.path(), Path::new("src/lib.rs")).unwrap();
    assert!(mirror.contains("megacorp_client"));

    let items = code_sanity::proposal::list_review(repo.path(), false).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].proposal.original_text, "megacorp_client");
    code_sanity::proposal::resolve_review(repo.path(), &items[0].id, true).unwrap();

    let conn = code_sanity::db::connect(&layout).unwrap();
    let projected =
        code_sanity::semantic_store::project_document(&conn, repo.path(), "src/lib.rs").unwrap();
    assert!(projected.content.contains("examplefirm_client"));
    assert!(!projected.content.contains("megacorp_client"));
    let mirror = read_sanitized_file(repo.path(), Path::new("src/lib.rs")).unwrap();
    assert!(
        mirror.contains("examplefirm_client"),
        "the shared mirror must include the accepted semantic projection"
    );
    assert!(!mirror.contains("megacorp_client"));
    assert!(code_sanity::verify_workspace(repo.path()).is_ok());
}

#[test]
fn llm_can_independently_propose_a_filename_term() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(
        repo.path().join("src/weaponized_loader.rs"),
        "fn ordinary_loader() -> usize { 1 }\n",
    )
    .unwrap();
    index_workspace(repo.path()).unwrap();

    let base_url = spawn_mock_server(Arc::new(|path: &str, request: &Value| {
        assert!(path.ends_with("/chat/completions"));
        let user: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
        if user["request_mode"] == "source" {
            assert_eq!(user["file"]["rel"], "src/weaponized_loader.rs");
            assert!(
                user["context"]["path_candidates"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            return chat_response("{\"proposals\":[]}");
        }

        assert_eq!(user["request_mode"], "path-only");
        assert_eq!(user["file"]["rel"], "path-inventory");
        assert_eq!(user["file"]["content"], "");
        let candidate = user["context"]["path_candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|candidate| candidate["value"] == "weaponized_loader")
            .expect("filename stem candidate");
        assert_eq!(candidate["kind"], "filename_stem");
        assert_eq!(candidate["path"], "src/weaponized_loader.rs");
        assert!(
            user["rules"]
                .as_array()
                .unwrap()
                .iter()
                .any(|rule| rule.as_str().unwrap().contains("file_path proposal"))
        );
        assert!(
            user["rules"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_str)
                .filter(|rule| { rule.contains("original_text") && rule.contains("file.content") })
                .all(|rule| rule.contains("identifier")),
            "source-membership rules must not suppress path-only proposals"
        );
        chat_response(
            &json!({
                "proposals": [
                    {
                        "target": { "path_id": candidate["path_id"] },
                        "category": "file_path",
                        "original_text": "weaponized",
                        "sanitized_text": "1bad-name",
                        "confidence": 0.99,
                        "rationale": "invalid first alternative"
                    },
                    {
                        "target": { "path_id": candidate["path_id"] },
                        "category": "filename",
                        "original_text": "weaponized",
                        "sanitized_text": "neutral",
                        "confidence": 0.96,
                        "rationale": "risk-loaded filename term"
                    }
                ]
            })
            .to_string(),
        )
    }));
    let layout = Layout::new(repo.path());
    let mut config = Config::load_or_default(&layout).unwrap();
    config.sanitizer.provider = ProviderConfig::Llm {
        base_url,
        model: "test-model".to_string(),
        api_key_env: "CODE_SANITY_TEST_KEY_UNSET".to_string(),
        timeout_secs: Some(5),
        json_mode: false,
    };
    config.save(&layout).unwrap();

    let report = propose_sanitize(
        repo.path(),
        Some(Path::new("src/weaponized_loader.rs")),
        ProviderAllow {
            command: false,
            endpoint: true,
        },
    )
    .unwrap();
    assert_eq!(report.queued, 1, "{report:?}");
    assert_eq!(report.rejected.len(), 1, "{report:?}");
    let reviews = code_sanity::proposal::list_review(repo.path(), false).unwrap();
    assert_eq!(reviews[0].proposal.category, "file_path");
    assert!(matches!(
        reviews[0].proposal.target.as_ref(),
        Some(code_sanity::proposal::ProposalTarget::FilePath(_))
    ));
}

#[test]
fn json_mode_sends_response_format_opt_in() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("lib.rs"), "fn megacorp_helper() {}\n").unwrap();
    index_workspace(repo.path()).unwrap();

    let saw_response_format = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&saw_response_format);
    let base_url = spawn_mock_server(Arc::new(move |_path: &str, request: &Value| {
        if request["response_format"]["type"] == "json_object" {
            counter.fetch_add(1, Ordering::SeqCst);
        }
        chat_response("{\"proposals\":[]}")
    }));

    let layout = Layout::new(repo.path());
    let mut config = Config::load_or_default(&layout).unwrap();
    config.sanitizer.provider = ProviderConfig::Llm {
        base_url,
        model: "test-model".to_string(),
        api_key_env: "CODE_SANITY_TEST_KEY_UNSET".to_string(),
        timeout_secs: Some(10),
        json_mode: true,
    };
    config.save(&layout).unwrap();

    propose_sanitize(
        repo.path(),
        Some(Path::new("lib.rs")),
        ProviderAllow {
            command: false,
            endpoint: true,
        },
    )
    .unwrap();
    assert_eq!(
        saw_response_format.load(Ordering::SeqCst),
        2,
        "json_mode = true must cover source and path-only requests"
    );
}

#[test]
fn openrouter_preset_routes_through_the_same_gate_and_client() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("lib.rs"), "fn megacorp_helper() {}\n").unwrap();
    index_workspace(repo.path()).unwrap();

    let chat_requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&chat_requests);
    let base_url = spawn_mock_server(Arc::new(move |path: &str, request: &Value| {
        assert!(path.ends_with("/chat/completions"));
        counter.fetch_add(1, Ordering::SeqCst);
        targeted_chat_response(request, &[("megacorp_helper", "examplefirm_helper", 0.95)])
    }));

    let layout = Layout::new(repo.path());
    let mut config = Config::load_or_default(&layout).unwrap();
    config.sanitizer.provider = ProviderConfig::Openrouter {
        model: "anthropic/claude-sonnet-4.5".to_string(),
        // Point the preset at the mock; a loopback URL also exempts the
        // unset-key preflight, mirroring a keyless local gateway.
        base_url: Some(base_url.clone()),
        api_key_env: Some("CODE_SANITY_TEST_KEY_UNSET".to_string()),
        timeout_secs: Some(10),
        json_mode: false,
    };
    config.save(&layout).unwrap();

    // The preset is still a repo-configured endpoint receiving real content:
    // the same confirmation gate applies.
    let refused = propose_sanitize(
        repo.path(),
        Some(Path::new("lib.rs")),
        ProviderAllow::default(),
    )
    .unwrap_err();
    assert!(refused.to_string().contains("--allow-provider-endpoint"));
    assert!(refused.to_string().contains(&base_url));
    assert_eq!(chat_requests.load(Ordering::SeqCst), 0);

    let report = propose_sanitize(
        repo.path(),
        Some(Path::new("lib.rs")),
        ProviderAllow {
            command: false,
            endpoint: true,
        },
    )
    .unwrap();
    assert_eq!(chat_requests.load(Ordering::SeqCst), 2);
    assert_eq!(report.queued, 1);
}

#[test]
fn embed_index_is_incremental_and_semantic_search_ranks_by_content() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    // "dangerous" is in the default dictionary -> the mirror (and therefore
    // everything sent to the embedding endpoint) says "neutral_parser".
    std::fs::write(
        repo.path().join("src/parser.rs"),
        "// the dangerous_parser turns token streams into a grammar tree\n\
         fn dangerous_parser(input: &str) -> usize {\n    input.len()\n}\n",
    )
    .unwrap();
    std::fs::write(
        repo.path().join("src/net.rs"),
        "// socket helpers for the network layer\n\
         fn connect_socket(addr: &str) -> usize {\n    addr.len()\n}\n",
    )
    .unwrap();
    index_workspace(repo.path()).unwrap();

    let embed_requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&embed_requests);
    let seen_real_term = Arc::new(AtomicUsize::new(0));
    let leak_counter = Arc::clone(&seen_real_term);
    let base_url = spawn_mock_server(Arc::new(move |path: &str, request: &Value| {
        assert!(path.ends_with("/embeddings"), "unexpected path {path}");
        counter.fetch_add(1, Ordering::SeqCst);
        if request["input"].to_string().contains("dangerous") {
            leak_counter.fetch_add(1, Ordering::SeqCst);
        }
        embeddings_response(request)
    }));

    let layout = Layout::new(repo.path());
    let mut config = Config::load_or_default(&layout).unwrap();
    config.embeddings.enabled = true;
    config.embeddings.base_url = base_url;
    config.embeddings.model = "test-embed".to_string();
    config.save(&layout).unwrap();

    // First run embeds every tracked file (init also tracks the generated
    // .gitignore), one request per file batch.
    let report = code_sanity::embed_index(repo.path()).unwrap();
    assert_eq!(report.embedded, 3);
    assert_eq!(report.unchanged, 0);
    assert!(report.chunks >= 3);
    assert_eq!(embed_requests.load(Ordering::SeqCst), 3);

    // Second run is a no-op without any HTTP traffic.
    let report = code_sanity::embed_index(repo.path()).unwrap();
    assert_eq!(report.embedded, 0);
    assert_eq!(report.unchanged, 3);
    assert_eq!(embed_requests.load(Ordering::SeqCst), 3);

    // Ranking follows content: a parser query lands on the parser file.
    let hits = code_sanity::semantic_search(repo.path(), "parser grammar token", 2).unwrap();
    assert_eq!(hits[0].rel_path, "src/parser.rs");
    assert!(hits[0].score > 0.0);
    let hits = code_sanity::semantic_search(repo.path(), "network socket", 2).unwrap();
    assert_eq!(hits[0].rel_path, "src/net.rs");

    // Only sanitized mirror content was embedded and stored: the endpoint
    // never saw the real term, and neither did the vector index.
    assert_eq!(seen_real_term.load(Ordering::SeqCst), 0);
    let conn = code_sanity::db::connect(&layout).unwrap();
    for (_, _, _, text, _) in code_sanity::db::all_embedding_chunks(&conn).unwrap() {
        assert!(!text.contains("dangerous"));
    }
    drop(conn);

    // Editing one file re-embeds exactly that file.
    std::fs::write(
        repo.path().join("src/net.rs"),
        "// socket helpers for the network layer, now with retries\n\
         fn connect_socket(addr: &str) -> usize {\n    addr.len() + 1\n}\n",
    )
    .unwrap();
    index_workspace(repo.path()).unwrap();
    let report = code_sanity::embed_index(repo.path()).unwrap();
    assert_eq!(report.embedded, 1);
    assert_eq!(report.unchanged, 2);

    // A deleted file takes its vectors with it.
    std::fs::remove_file(repo.path().join("src/net.rs")).unwrap();
    index_workspace(repo.path()).unwrap();
    let report = code_sanity::embed_index(repo.path()).unwrap();
    // index_workspace already dropped the db rows (including embeddings via
    // remove_file); a stale embedding_state row would also be swept here.
    assert_eq!(report.embedded, 0);
    let conn = code_sanity::db::connect(&layout).unwrap();
    let remaining: Vec<String> = code_sanity::db::embedded_files(&conn).unwrap();
    assert_eq!(
        remaining,
        vec![".gitignore".to_string(), "src/parser.rs".to_string()]
    );
}

#[test]
fn transient_http_errors_are_retried_and_hard_errors_are_not() {
    use code_sanity::llm::OpenAiClient;

    // 429 then 200: the call succeeds on the retry.
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    let base_url = spawn_mock_server_with_status(Arc::new(move |_path: &str, request: &Value| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            (429, json!({ "error": "rate limited" }))
        } else {
            (200, embeddings_response(request))
        }
    }));
    let client = OpenAiClient::new(&base_url, "CODE_SANITY_TEST_KEY_UNSET", 5).unwrap();
    let vectors = client.embed("test-embed", &["hello".to_string()]).unwrap();
    assert_eq!(vectors.len(), 1);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    // A hard 400 fails immediately, without retries.
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    let base_url = spawn_mock_server_with_status(Arc::new(move |_path: &str, _request: &Value| {
        counter.fetch_add(1, Ordering::SeqCst);
        (400, json!({ "error": "bad request" }))
    }));
    let client = OpenAiClient::new(&base_url, "CODE_SANITY_TEST_KEY_UNSET", 5).unwrap();
    let err = client
        .embed("test-embed", &["hello".to_string()])
        .unwrap_err();
    assert!(err.to_string().contains("HTTP 400"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[test]
fn embed_index_sweeps_untracked_embedding_rows() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("main.rs"), "fn main() {}\n").unwrap();
    index_workspace(repo.path()).unwrap();

    let base_url = spawn_mock_server(Arc::new(|_path: &str, request: &Value| {
        embeddings_response(request)
    }));
    let layout = Layout::new(repo.path());
    let mut config = Config::load_or_default(&layout).unwrap();
    config.embeddings.enabled = true;
    config.embeddings.base_url = base_url;
    config.save(&layout).unwrap();

    // Plant vectors for a path nothing tracks (e.g. leftovers from an
    // interrupted run whose file was deleted before its rows were swept).
    let mut conn = code_sanity::db::connect(&layout).unwrap();
    code_sanity::db::ensure_schema(&conn).unwrap();
    code_sanity::db::replace_embeddings(
        &mut conn,
        "ghost.txt",
        "sha-of-nothing",
        "fp",
        &[(1, 1, "ghost", code_sanity::embed::vector_to_blob(&[1.0]))],
    )
    .unwrap();
    drop(conn);

    let report = code_sanity::embed_index(repo.path()).unwrap();
    assert_eq!(report.removed, 1);
    let conn = code_sanity::db::connect(&layout).unwrap();
    assert!(
        !code_sanity::db::embedded_files(&conn)
            .unwrap()
            .contains(&"ghost.txt".to_string())
    );
}

#[test]
fn embed_index_refuses_when_disabled() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("main.rs"), "fn main() {}\n").unwrap();
    index_workspace(repo.path()).unwrap();
    let err = code_sanity::embed_index(repo.path()).unwrap_err();
    assert!(err.to_string().contains("embeddings are disabled"));
    let err = code_sanity::semantic_search(repo.path(), "anything", 5).unwrap_err();
    assert!(err.to_string().contains("embeddings are disabled"));
}

#[test]
fn semantic_search_refuses_stale_fingerprints_before_any_http() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("main.rs"), "fn parser() {}\n").unwrap();
    index_workspace(repo.path()).unwrap();

    let embed_requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&embed_requests);
    let base_url = spawn_mock_server(Arc::new(move |_path: &str, request: &Value| {
        counter.fetch_add(1, Ordering::SeqCst);
        embeddings_response(request)
    }));
    let layout = Layout::new(repo.path());
    let mut config = Config::load_or_default(&layout).unwrap();
    config.embeddings.enabled = true;
    config.embeddings.base_url = base_url;
    config.embeddings.model = "test-embed".to_string();
    config.save(&layout).unwrap();
    code_sanity::embed_index(repo.path()).unwrap();
    let after_index = embed_requests.load(Ordering::SeqCst);
    code_sanity::semantic_search(repo.path(), "parser", 3).unwrap();
    assert_eq!(embed_requests.load(Ordering::SeqCst), after_index + 1);

    // The embeddings model changes but embed-index was not re-run: the query
    // would be scored against a different vector space. Must refuse BEFORE
    // paying for a query embedding.
    let mut config = Config::load_or_default(&layout).unwrap();
    config.embeddings.model = "other-model".to_string();
    config.save(&layout).unwrap();
    let before = embed_requests.load(Ordering::SeqCst);
    let err = code_sanity::semantic_search(repo.path(), "parser", 3).unwrap_err();
    assert!(err.to_string().contains("embed-index"), "{err:#}");
    assert_eq!(
        embed_requests.load(Ordering::SeqCst),
        before,
        "stale-fingerprint refusal must not cost an HTTP request"
    );

    // Re-embedding under the new model reconciles everything.
    let report = code_sanity::embed_index(repo.path()).unwrap();
    assert!(report.embedded > 0);
    assert_eq!(report.unchanged, 0, "all vectors must be recomputed");
    code_sanity::semantic_search(repo.path(), "parser", 3).unwrap();
}

#[test]
fn provider_error_on_one_file_does_not_abort_the_run() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(repo.path().join("src/bad.rs"), "fn megacorp_a() {}\n").unwrap();
    std::fs::write(repo.path().join("src/good.rs"), "fn megacorp_b() {}\n").unwrap();
    index_workspace(repo.path()).unwrap();

    // The mock rejects the request that carries bad.rs and answers good.rs.
    let base_url = spawn_mock_server_with_status(Arc::new(|_path: &str, request: &Value| {
        let user = request["messages"][1]["content"].as_str().unwrap_or("");
        let task: Value = serde_json::from_str(user).unwrap();
        if task["request_mode"] == "path-only" {
            (200, chat_response("{\"proposals\":[]}"))
        } else if task["file"]["rel"] == "src/bad.rs" {
            (400, json!({ "error": "context overflow" }))
        } else if task["file"]["rel"] == "src/good.rs" {
            (
                200,
                targeted_chat_response(request, &[("megacorp_b", "examplefirm_b", 0.95)]),
            )
        } else {
            (200, chat_response("{\"proposals\":[]}"))
        }
    }));
    let layout = Layout::new(repo.path());
    let mut config = Config::load_or_default(&layout).unwrap();
    config.sanitizer.provider = ProviderConfig::Llm {
        base_url,
        model: "test-model".to_string(),
        api_key_env: "CODE_SANITY_TEST_KEY_UNSET".to_string(),
        timeout_secs: Some(5),
        json_mode: false,
    };
    config.save(&layout).unwrap();

    let report = propose_sanitize(
        repo.path(),
        None,
        ProviderAllow {
            command: false,
            endpoint: true,
        },
    )
    .unwrap();
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(report.errors[0].contains("bad.rs"), "{:?}", report.errors);
    assert!(report.queued >= 1, "good file's proposals must be queued");
}

#[test]
fn proposal_provider_runs_with_bounded_concurrency_and_reports_progress() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    for index in 0..6 {
        std::fs::write(
            repo.path().join(format!("src/file_{index}.rs")),
            format!("fn helper_{index}() {{}}\n"),
        )
        .unwrap();
    }
    index_workspace(repo.path()).unwrap();

    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let active_for_handler = Arc::clone(&active);
    let max_for_handler = Arc::clone(&max_active);
    let base_url = spawn_mock_server(Arc::new(move |_path: &str, _request: &Value| {
        let now = active_for_handler.fetch_add(1, Ordering::SeqCst) + 1;
        max_for_handler.fetch_max(now, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(80));
        active_for_handler.fetch_sub(1, Ordering::SeqCst);
        chat_response("{\"proposals\":[]}")
    }));

    let layout = Layout::new(repo.path());
    let mut config = Config::load_or_default(&layout).unwrap();
    config.sanitizer.provider = ProviderConfig::Llm {
        base_url,
        model: "test-model".to_string(),
        api_key_env: "CODE_SANITY_TEST_KEY_UNSET".to_string(),
        timeout_secs: Some(5),
        json_mode: false,
    };
    config.save(&layout).unwrap();

    let events = Mutex::new(Vec::new());
    let report = propose_sanitize_with_progress(
        repo.path(),
        None,
        ProviderAllow {
            command: false,
            endpoint: true,
        },
        Some(3),
        |event| events.lock().unwrap().push(event),
    )
    .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(
        max_active.load(Ordering::SeqCst) >= 2,
        "provider calls should overlap"
    );
    assert!(
        max_active.load(Ordering::SeqCst) <= 3,
        "jobs must bound provider concurrency"
    );

    let events = events.into_inner().unwrap();
    let total = events
        .iter()
        .find_map(|event| match event {
            ProposeProgress::Started { total, jobs, .. } => {
                assert_eq!(*jobs, 3);
                Some(*total)
            }
            _ => None,
        })
        .expect("started event");
    let finished_files = events
        .iter()
        .filter(|event| matches!(event, ProposeProgress::FileFinished { .. }))
        .count();
    assert_eq!(finished_files, total);
    assert!(matches!(
        events.last(),
        Some(ProposeProgress::Finished { .. })
    ));
}

#[test]
fn one_large_file_is_chunked_parallel_and_overlap_findings_are_deduplicated() {
    let repo = tempfile::tempdir().unwrap();
    let content = format!(
        "fn hwid() {{}}\n{}",
        (0..40)
            .map(|index| format!("fn helper_{index}() {{ hwid(); }}\n"))
            .collect::<String>()
    );
    std::fs::write(repo.path().join("large.rs"), content).unwrap();
    index_workspace(repo.path()).unwrap();

    let requests = Arc::new(AtomicUsize::new(0));
    let source_requests = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let requests_for_handler = Arc::clone(&requests);
    let source_for_handler = Arc::clone(&source_requests);
    let active_for_handler = Arc::clone(&active);
    let max_for_handler = Arc::clone(&max_active);
    let base_url = spawn_mock_server(Arc::new(move |_path: &str, request: &Value| {
        requests_for_handler.fetch_add(1, Ordering::SeqCst);
        let user: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
        if user["request_mode"] == "path-only" {
            return chat_response("{\"proposals\":[]}");
        }
        source_for_handler.fetch_add(1, Ordering::SeqCst);
        let now = active_for_handler.fetch_add(1, Ordering::SeqCst) + 1;
        max_for_handler.fetch_max(now, Ordering::SeqCst);
        assert!(user["file"]["chunk"]["total"].as_u64().unwrap() > 1);
        assert!(user["file"]["content"].as_str().unwrap().contains("hwid"));
        std::thread::sleep(std::time::Duration::from_millis(80));
        active_for_handler.fetch_sub(1, Ordering::SeqCst);
        targeted_chat_response(request, &[("hwid", "device_ref", 0.9)])
    }));

    let layout = Layout::new(repo.path());
    let mut config = Config::load_or_default(&layout).unwrap();
    config.sanitizer.propose_chunk_bytes = 160;
    config.sanitizer.propose_chunk_overlap_lines = 1;
    config.sanitizer.provider = ProviderConfig::Llm {
        base_url,
        model: "test-model".to_string(),
        api_key_env: "CODE_SANITY_TEST_KEY_UNSET".to_string(),
        timeout_secs: Some(5),
        json_mode: true,
    };
    config.save(&layout).unwrap();

    let report = propose_sanitize_with_progress(
        repo.path(),
        Some(Path::new("large.rs")),
        ProviderAllow {
            command: false,
            endpoint: true,
        },
        Some(3),
        |_| {},
    )
    .unwrap();
    let request_count = requests.load(Ordering::SeqCst);
    let source_request_count = source_requests.load(Ordering::SeqCst);
    assert!(request_count > 2);
    assert_eq!(request_count, source_request_count + 1);
    assert!(max_active.load(Ordering::SeqCst) >= 2);
    assert!(max_active.load(Ordering::SeqCst) <= 3);
    assert_eq!(report.proposed, source_request_count);
    assert_eq!(report.queued, 1);
    assert_eq!(report.duplicates, source_request_count - 1);
    assert_eq!(
        code_sanity::proposal::list_review(repo.path(), false)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn overlap_context_is_never_owned_even_when_the_model_returns_it() {
    let repo = tempfile::tempdir().unwrap();
    let content = (0..30)
        .map(|index| format!("fn marker_{index:02}() {{}}\n"))
        .collect::<String>();
    std::fs::write(repo.path().join("overlap.rs"), content).unwrap();
    index_workspace(repo.path()).unwrap();

    let context_responses = Arc::new(AtomicUsize::new(0));
    let responses_for_handler = Arc::clone(&context_responses);
    let base_url = spawn_mock_server(Arc::new(move |_path: &str, request: &Value| {
        let user: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
        let context_before = user["file"]["context_before"].as_str().unwrap();
        let candidate = context_before
            .split(|ch: char| !(ch == '_' || ch.is_ascii_alphanumeric()))
            .find(|word| word.starts_with("marker_"));
        let Some(candidate) = candidate else {
            return chat_response("{\"proposals\":[]}");
        };
        responses_for_handler.fetch_add(1, Ordering::SeqCst);
        chat_response(&format!(
            "{{\"proposals\":[{{\"category\":\"identifier\",\"original_text\":\"{candidate}\",\
             \"sanitized_text\":\"neutral_ref\",\"confidence\":0.9}}]}}"
        ))
    }));

    let layout = Layout::new(repo.path());
    let mut config = Config::load_or_default(&layout).unwrap();
    config.sanitizer.propose_chunk_bytes = 100;
    config.sanitizer.propose_chunk_overlap_lines = 1;
    config.sanitizer.provider = ProviderConfig::Llm {
        base_url,
        model: "test-model".to_string(),
        api_key_env: "CODE_SANITY_TEST_KEY_UNSET".to_string(),
        timeout_secs: Some(5),
        json_mode: true,
    };
    config.save(&layout).unwrap();

    let report = propose_sanitize_with_progress(
        repo.path(),
        Some(Path::new("overlap.rs")),
        ProviderAllow {
            command: false,
            endpoint: true,
        },
        Some(2),
        |_| {},
    )
    .unwrap();
    let context_responses = context_responses.load(Ordering::SeqCst);
    assert!(context_responses > 0);
    assert_eq!(report.proposed, context_responses);
    assert_eq!(report.duplicates, context_responses);
    assert_eq!(report.queued, 0);
    assert!(report.rejected.is_empty());
}

#[test]
fn repeated_provider_scan_deduplicates_pending_review_items() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("lib.rs"), "fn megacorp_helper() {}\n").unwrap();
    index_workspace(repo.path()).unwrap();

    let base_url = spawn_mock_server(Arc::new(|_path: &str, request: &Value| {
        let user: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
        let already_seen = user["context"]["already_decided_symbol_ids"]
            .as_array()
            .unwrap()
            .iter()
            .next()
            .is_some();
        if already_seen {
            chat_response("{\"proposals\":[]}")
        } else {
            targeted_chat_response(request, &[("megacorp_helper", "examplefirm_helper", 0.95)])
        }
    }));
    let layout = Layout::new(repo.path());
    let mut config = Config::load_or_default(&layout).unwrap();
    config.sanitizer.provider = ProviderConfig::Llm {
        base_url,
        model: "test-model".to_string(),
        api_key_env: "CODE_SANITY_TEST_KEY_UNSET".to_string(),
        timeout_secs: Some(5),
        json_mode: false,
    };
    config.save(&layout).unwrap();

    let allow = ProviderAllow {
        command: false,
        endpoint: true,
    };
    let first = propose_sanitize(repo.path(), Some(Path::new("lib.rs")), allow).unwrap();
    assert_eq!(first.queued, 1);
    assert_eq!(first.duplicates, 0);

    let second = propose_sanitize(repo.path(), Some(Path::new("lib.rs")), allow).unwrap();
    assert_eq!(second.queued, 0);
    assert_eq!(second.proposed, 0);
    assert_eq!(second.duplicates, 0);
    assert_eq!(
        code_sanity::proposal::list_review(repo.path(), false)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn same_spelling_symbols_are_proposed_independently_by_symbol_id() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(repo.path().join("src/a.rs"), "fn shadowfax() {}\n").unwrap();
    std::fs::write(repo.path().join("src/b.rs"), "fn shadowfax() {}\n").unwrap();
    index_workspace(repo.path()).unwrap();

    let source_requests = Arc::new(AtomicUsize::new(0));
    let source_for_handler = Arc::clone(&source_requests);
    let base_url = spawn_mock_server(Arc::new(move |_path: &str, request: &Value| {
        let task: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
        if task["request_mode"] == "path-only" {
            return chat_response("{\"proposals\":[]}");
        }
        source_for_handler.fetch_add(1, Ordering::SeqCst);
        let candidate = task["context"]["semantic_candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|candidate| candidate["name"] == "shadowfax")
            .expect("each same-spelling declaration remains independently eligible");
        assert!(
            !task["context"]["already_decided_symbol_ids"]
                .as_array()
                .unwrap()
                .contains(&candidate["symbol_id"]),
            "a decision for another symbol must not suppress this ID"
        );
        let alias = if task["file"]["rel"] == "src/a.rs" {
            "neutral_a"
        } else {
            "neutral_b"
        };
        chat_response(
            &json!({
                "proposals": [{
                    "target": {
                        "symbol_id": candidate["symbol_id"],
                        "occurrence_id": candidate["occurrence_id"]
                    },
                    "category": "identifier",
                    "original_text": "shadowfax",
                    "sanitized_text": alias,
                    "confidence": 0.9
                }]
            })
            .to_string(),
        )
    }));
    let layout = Layout::new(repo.path());
    let mut config = Config::load_or_default(&layout).unwrap();
    config.sanitizer.provider = ProviderConfig::Llm {
        base_url,
        model: "test-model".to_string(),
        api_key_env: "CODE_SANITY_TEST_KEY_UNSET".to_string(),
        timeout_secs: Some(5),
        json_mode: true,
    };
    config.save(&layout).unwrap();

    let report = propose_sanitize_with_progress(
        repo.path(),
        Some(Path::new("src")),
        ProviderAllow {
            command: false,
            endpoint: true,
        },
        Some(1),
        |_| {},
    )
    .unwrap();
    assert_eq!(source_requests.load(Ordering::SeqCst), 2);
    assert_eq!(report.eligibility.owned_symbols, 2);
    assert_eq!(report.eligibility.sent_symbol_candidates, 2);
    assert_eq!(report.queued, 2, "{report:?}");
    let reviews = code_sanity::proposal::list_review(repo.path(), false).unwrap();
    let ids = reviews
        .iter()
        .map(|item| match item.proposal.target.as_ref().unwrap() {
            code_sanity::proposal::ProposalTarget::Semantic(target) => target.symbol_id.clone(),
            code_sanity::proposal::ProposalTarget::FilePath(_) => panic!("unexpected path review"),
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(ids.len(), 2);
}

#[test]
fn rejected_alias_does_not_suppress_a_later_valid_alias() {
    let repo = tempfile::tempdir().unwrap();
    let source = format!(
        "fn shadowfax() {{}}\n{}fn invoke() {{ shadowfax(); }}\n",
        (0..30)
            .map(|index| format!("fn filler_{index}() {{}}\n"))
            .collect::<String>()
    );
    std::fs::write(repo.path().join("lib.rs"), source).unwrap();
    index_workspace(repo.path()).unwrap();

    let candidate_requests = Arc::new(AtomicUsize::new(0));
    let candidate_for_handler = Arc::clone(&candidate_requests);
    let base_url = spawn_mock_server(Arc::new(move |_path: &str, request: &Value| {
        let task: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
        if task["request_mode"] == "path-only" {
            return chat_response("{\"proposals\":[]}");
        }
        let Some(candidate) = task["context"]["semantic_candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|candidate| candidate["name"] == "shadowfax")
        else {
            return chat_response("{\"proposals\":[]}");
        };
        let attempt = candidate_for_handler.fetch_add(1, Ordering::SeqCst);
        let alias = if attempt == 0 {
            "1invalid"
        } else {
            "neutral_helper"
        };
        chat_response(
            &json!({
                "proposals": [{
                    "target": {
                        "symbol_id": candidate["symbol_id"],
                        "occurrence_id": candidate["occurrence_id"]
                    },
                    "category": "identifier",
                    "original_text": "shadowfax",
                    "sanitized_text": alias,
                    "confidence": 0.9
                }]
            })
            .to_string(),
        )
    }));
    let layout = Layout::new(repo.path());
    let mut config = Config::load_or_default(&layout).unwrap();
    config.sanitizer.propose_chunk_bytes = 96;
    config.sanitizer.propose_chunk_overlap_lines = 1;
    config.sanitizer.provider = ProviderConfig::Llm {
        base_url,
        model: "test-model".to_string(),
        api_key_env: "CODE_SANITY_TEST_KEY_UNSET".to_string(),
        timeout_secs: Some(5),
        json_mode: true,
    };
    config.save(&layout).unwrap();

    let report = propose_sanitize_with_progress(
        repo.path(),
        Some(Path::new("lib.rs")),
        ProviderAllow {
            command: false,
            endpoint: true,
        },
        Some(1),
        |_| {},
    )
    .unwrap();
    assert!(candidate_requests.load(Ordering::SeqCst) >= 2);
    assert_eq!(report.queued, 1, "{report:?}");
    assert!(
        report
            .rejected
            .iter()
            .any(|reason| reason.contains("valid identifier")),
        "{report:?}"
    );
    let reviews = code_sanity::proposal::list_review(repo.path(), false).unwrap();
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].proposal.sanitized_text, "neutral_helper");
}

#[test]
fn security_adjacent_terms_can_be_proposed_for_review() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(
        repo.path().join("lib.rs"),
        "fn collect_hwid() {\n    let launcher = build_launcher();\n}\n",
    )
    .unwrap();
    index_workspace(repo.path()).unwrap();

    let base_url = spawn_mock_server(Arc::new(|_path: &str, request: &Value| {
        let user = request["messages"][1]["content"].as_str().unwrap();
        let task: Value = serde_json::from_str(user).unwrap();
        if task["request_mode"] == "path-only" {
            return chat_response("{\"proposals\":[]}");
        }
        assert!(user.contains("collect_hwid"));
        assert!(user.contains("launcher"));
        assert!(user.contains("public third-party companies"));
        assert!(user.contains("indexed_external_identifiers"));
        targeted_chat_response(
            request,
            &[
                ("collect_hwid", "collect_device_ref", 0.9),
                ("launcher", "starter", 0.85),
            ],
        )
    }));
    let layout = Layout::new(repo.path());
    let mut config = Config::load_or_default(&layout).unwrap();
    config.sanitizer.provider = ProviderConfig::Llm {
        base_url,
        model: "test-model".to_string(),
        api_key_env: "CODE_SANITY_TEST_KEY_UNSET".to_string(),
        timeout_secs: Some(5),
        json_mode: true,
    };
    config.save(&layout).unwrap();

    let report = propose_sanitize(
        repo.path(),
        Some(Path::new("lib.rs")),
        ProviderAllow {
            command: false,
            endpoint: true,
        },
    )
    .unwrap();
    assert_eq!(report.proposed, 2);
    assert_eq!(report.queued, 2);
    assert!(report.rejected.is_empty(), "{:?}", report.rejected);
}

#[test]
fn oversized_files_still_get_path_only_requests() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(repo.path().join("src/small.rs"), "fn megacorp_small() {}\n").unwrap();
    std::fs::write(
        repo.path().join("src/large.rs"),
        format!(
            "// filler\n{}fn megacorp_large() {{}}\n",
            "// x\n".repeat(40)
        ),
    )
    .unwrap();
    index_workspace(repo.path()).unwrap();

    let chat_requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&chat_requests);
    let saw_large_path = Arc::new(AtomicUsize::new(0));
    let saw_large_for_handler = Arc::clone(&saw_large_path);
    let base_url = spawn_mock_server(Arc::new(move |_path: &str, request: &Value| {
        let user: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
        if user["request_mode"] == "path-only" {
            assert_eq!(user["file"]["content"], "");
            assert_eq!(
                user["context"]["semantic_candidates"]
                    .as_array()
                    .unwrap()
                    .len(),
                0
            );
            let candidates = user["context"]["path_candidates"].as_array().unwrap();
            assert!(!candidates.is_empty());
            if candidates
                .iter()
                .any(|candidate| candidate["path"] == "src/large.rs")
            {
                saw_large_for_handler.fetch_add(1, Ordering::SeqCst);
            }
        } else {
            assert_ne!(user["file"]["rel"], "src/large.rs");
            assert!(
                user["context"]["path_candidates"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
        counter.fetch_add(1, Ordering::SeqCst);
        chat_response("{\"proposals\":[]}")
    }));
    let layout = Layout::new(repo.path());
    let mut config = Config::load_or_default(&layout).unwrap();
    config.sanitizer.propose_max_file_bytes = 64;
    config.sanitizer.provider = ProviderConfig::Llm {
        base_url,
        model: "test-model".to_string(),
        api_key_env: "CODE_SANITY_TEST_KEY_UNSET".to_string(),
        timeout_secs: Some(5),
        json_mode: false,
    };
    config.save(&layout).unwrap();

    let report = propose_sanitize(
        repo.path(),
        None,
        ProviderAllow {
            command: false,
            endpoint: true,
        },
    )
    .unwrap();
    assert_eq!(report.skipped.len(), 1, "{:?}", report.skipped);
    assert!(
        report.skipped[0].contains("large.rs"),
        "{:?}",
        report.skipped
    );
    assert!(
        report.skipped[0].contains("propose_max_file_bytes"),
        "{:?}",
        report.skipped
    );
    // Files under the cap send source; a separate deduplicated path-only batch
    // still covers the oversized file without leaking its content
    // (small.rs + .gitignore source requests + one path inventory request).
    assert_eq!(chat_requests.load(Ordering::SeqCst), 3);
    assert_eq!(saw_large_path.load(Ordering::SeqCst), 1);
}

#[test]
fn malformed_llm_json_is_retried_once_without_losing_the_chunk() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("main.rs"), "fn shadowfax() {}\n").unwrap();
    index_workspace(repo.path()).unwrap();

    let source_attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_handler = Arc::clone(&source_attempts);
    let base_url = spawn_mock_server(Arc::new(move |_path: &str, request: &Value| {
        let task: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
        if task["request_mode"] == "path-only" {
            return chat_response("{\"proposals\":[]}");
        }
        let attempt = attempts_for_handler.fetch_add(1, Ordering::SeqCst);
        if attempt == 0 {
            return chat_response("{\"proposals\":[");
        }
        assert!(task.get("retry_instruction").is_some());
        targeted_chat_response(request, &[("shadowfax", "neutral_helper", 0.9)])
    }));
    let layout = Layout::new(repo.path());
    let mut config = Config::load_or_default(&layout).unwrap();
    config.sanitizer.provider = ProviderConfig::Llm {
        base_url,
        model: "test-model".to_string(),
        api_key_env: "CODE_SANITY_TEST_KEY_UNSET".to_string(),
        timeout_secs: Some(5),
        json_mode: true,
    };
    config.save(&layout).unwrap();

    let report = propose_sanitize(
        repo.path(),
        Some(Path::new("main.rs")),
        ProviderAllow {
            command: false,
            endpoint: true,
        },
    )
    .unwrap();
    assert_eq!(source_attempts.load(Ordering::SeqCst), 2);
    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(report.queued, 1);
}

#[test]
fn truncated_chat_reply_is_a_clear_error_not_a_parse_failure() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("main.rs"), "fn megacorp_entry() {}\n").unwrap();
    index_workspace(repo.path()).unwrap();

    let base_url = spawn_mock_server(Arc::new(|_path: &str, _request: &Value| {
        json!({
            "choices": [{
                "finish_reason": "length",
                "message": { "role": "assistant", "content": "{\"proposals\":[{\"cat" }
            }]
        })
    }));
    let layout = Layout::new(repo.path());
    let mut config = Config::load_or_default(&layout).unwrap();
    config.sanitizer.provider = ProviderConfig::Llm {
        base_url,
        model: "test-model".to_string(),
        api_key_env: "CODE_SANITY_TEST_KEY_UNSET".to_string(),
        timeout_secs: Some(5),
        json_mode: false,
    };
    config.save(&layout).unwrap();

    let err = propose_sanitize(
        repo.path(),
        Some(Path::new("main.rs")),
        ProviderAllow {
            command: false,
            endpoint: true,
        },
    )
    .unwrap_err();
    let chain = format!("{err:#}");
    assert!(chain.contains("finish_reason"), "{chain}");
    assert!(chain.contains("cut off"), "{chain}");
    assert!(!chain.contains("expected a ProposalBatch"), "{chain}");
}
