//! Embed-index under concurrency: a slow embedding endpoint concurrent with
//! sync/edit storms (the A3-style suite for the embedding lock scheme).
//! Threads acquire the flock on separate file descriptions, so they contend
//! exactly like separate processes.

use code_sanity::config::{Config, Layout};
use code_sanity::db;
use code_sanity::map::sha256_hex;
use code_sanity::{embed_index, index_workspace, semantic_search, verify_workspace};
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Deterministic fake embedding, same scheme as tests/llm_embed.rs.
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

/// Mock /embeddings server that sleeps `delay` between receiving a request and
/// answering it, so the test can act while an HTTP call is provably in flight.
/// Increments `requests` BEFORE sleeping.
fn spawn_slow_embed_server(delay: Duration, requests: Arc<AtomicUsize>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let requests = Arc::clone(&requests);
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    return;
                }
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        content_length = value.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; content_length];
                if reader.read_exact(&mut body).is_err() {
                    return;
                }
                let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                requests.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(delay);
                let response = embeddings_response(&request).to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    response.len(),
                    response
                );
                let _ = stream.flush();
            });
        }
    });
    format!("http://{addr}/v1")
}

fn enable_embeddings(root: &Path, base_url: &str) {
    let layout = Layout::new(root);
    let mut config = Config::load_or_default(&layout).unwrap();
    config.embeddings.enabled = true;
    config.embeddings.base_url = base_url.to_string();
    config.embeddings.model = "test-embed".to_string();
    config.embeddings.timeout_secs = 30;
    config.save(&layout).unwrap();
}

/// Every committed embedding_state row must record exactly the sha of the
/// mirror file it claims to have embedded.
fn assert_committed_state_matches_mirror(root: &Path) {
    let layout = Layout::new(root);
    let conn = db::connect(&layout).unwrap();
    let embedded = db::embedded_files(&conn).unwrap();
    assert!(!embedded.is_empty(), "nothing committed to the index");
    for rel in embedded {
        let mirror = fs::read_to_string(layout.mirror_dir.join(&rel))
            .unwrap_or_else(|err| panic!("{rel}: committed state without mirror file: {err}"));
        let (sha, _fingerprint) = db::embedding_state(&conn, &rel).unwrap().unwrap();
        assert_eq!(
            sha,
            sha256_hex(mirror.as_bytes()),
            "{rel}: committed vector state does not match the mirror"
        );
    }
}

#[test]
fn embed_index_converges_under_a_sync_and_edit_storm() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    // "dangerous" is dictionary-sanitized: the leak assertions have teeth.
    let parser_a = "// the dangerous_parser eats token streams into a grammar\n\
                    fn dangerous_parser() -> usize {\n    1\n}\n";
    let parser_b = "// the dangerous_parser eats token streams into a grammar\n\
                    fn dangerous_parser() -> usize {\n    2\n}\n";
    fs::write(repo.path().join("src/parser.rs"), parser_a).unwrap();
    fs::write(
        repo.path().join("src/net.rs"),
        "// socket helpers for the network layer\nfn connect_socket() -> usize {\n    1\n}\n",
    )
    .unwrap();
    index_workspace(repo.path()).unwrap();

    let requests = Arc::new(AtomicUsize::new(0));
    let base_url = spawn_slow_embed_server(Duration::from_millis(25), Arc::clone(&requests));
    enable_embeddings(repo.path(), &base_url);

    // Seed the index once so semantic_search always has vectors to read.
    embed_index(repo.path()).unwrap();

    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let root = repo.path().to_path_buf();
        let stop_ref = &stop;
        scope.spawn(move || {
            let mut flip = false;
            while !stop_ref.load(Ordering::Relaxed) {
                fs::write(
                    root.join("src/parser.rs"),
                    if flip { parser_a } else { parser_b },
                )
                .unwrap();
                let _ = index_workspace(&root);
                let _ = code_sanity::index::sync_single_file(&root, Path::new("src/parser.rs"));
                flip = !flip;
            }
        });

        // Embedder and reader survive the storm: every call must return Ok
        // (an Err or a hang here is a lock-ordering failure).
        for round in 0..5 {
            embed_index(repo.path())
                .unwrap_or_else(|err| panic!("embed_index round {round} failed: {err:#}"));
            let hits = semantic_search(repo.path(), "parser grammar", 2)
                .unwrap_or_else(|err| panic!("semantic_search round {round} failed: {err:#}"));
            for hit in hits {
                assert!(
                    !hit.preview.to_lowercase().contains("dangerous"),
                    "leak in search preview during storm: {}",
                    hit.preview
                );
            }
        }
        stop.store(true, Ordering::Relaxed);
    });

    // Quiescent: one run reconciles whatever the storm left stale, the next
    // is a no-op — the index converged.
    embed_index(repo.path()).unwrap();
    let report = embed_index(repo.path()).unwrap();
    assert_eq!(report.embedded, 0, "index did not converge: {report:?}");
    assert_eq!(report.stale, 0, "index did not converge: {report:?}");

    assert_committed_state_matches_mirror(repo.path());
    let layout = Layout::new(repo.path());
    let conn = db::connect(&layout).unwrap();
    for (_rel, _start, _end, text, _vector) in db::all_embedding_chunks(&conn).unwrap() {
        assert!(
            !text.to_lowercase().contains("dangerous"),
            "real term leaked into a committed chunk: {text}"
        );
    }
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn mid_flight_mirror_change_is_skipped_and_reconciled() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    let doc_a = "// token stream parser notes\nfn doc() -> usize {\n    1\n}\n";
    let doc_b = "// token stream parser notes\nfn doc() -> usize {\n    2\n}\n";
    fs::write(repo.path().join("src/doc.rs"), doc_a).unwrap();
    index_workspace(repo.path()).unwrap();

    let requests = Arc::new(AtomicUsize::new(0));
    let delay = Duration::from_millis(500);
    let base_url = spawn_slow_embed_server(delay, Arc::clone(&requests));
    enable_embeddings(repo.path(), &base_url);

    let total_files = {
        let layout = Layout::new(repo.path());
        let conn = db::connect(&layout).unwrap();
        db::tracked_files(&conn).unwrap().len()
    };

    let done = AtomicBool::new(false);
    let report = std::thread::scope(|scope| {
        let root = repo.path().to_path_buf();
        let done_ref = &done;
        let embedder = scope.spawn(move || {
            let report = embed_index(&root);
            done_ref.store(true, Ordering::SeqCst);
            report
        });

        // Wait until the LAST file's embedding request is in flight (the mock
        // counts before sleeping), i.e. the endpoint is holding the call open.
        let deadline = Instant::now() + Duration::from_secs(10);
        while requests.load(Ordering::SeqCst) < total_files {
            assert!(
                Instant::now() < deadline,
                "embedding requests never arrived"
            );
            std::thread::sleep(Duration::from_millis(5));
        }

        // While the call is in flight, rewrite the real file and re-render the
        // mirror. This must finish well inside the mock's delay: if embed_index
        // wrongly held the workspace lock across HTTP, this would block until
        // the response and the run would see nothing stale.
        let reindex_started = Instant::now();
        fs::write(repo.path().join("src/doc.rs"), doc_b).unwrap();
        index_workspace(repo.path()).unwrap();
        assert!(
            !done.load(Ordering::SeqCst),
            "embed_index finished before the mid-flight edit; widen the mock delay"
        );
        assert!(
            reindex_started.elapsed() < delay,
            "index_workspace blocked while an embedding call was in flight: \
             embed_index holds the workspace lock across HTTP"
        );

        embedder.join().unwrap().unwrap()
    });

    // The changed file's freshly computed vectors were refused at commit.
    assert_eq!(report.stale, 1, "{report:?}");
    let layout = Layout::new(repo.path());
    let conn = db::connect(&layout).unwrap();
    assert!(
        db::embedding_state(&conn, "src/doc.rs").unwrap().is_none(),
        "stale vectors must not be committed"
    );
    drop(conn);

    // The next quiescent run reconciles exactly that file.
    let report = embed_index(repo.path()).unwrap();
    assert_eq!(report.embedded, 1, "{report:?}");
    assert_eq!(report.stale, 0, "{report:?}");
    assert_committed_state_matches_mirror(repo.path());
    assert!(verify_workspace(repo.path()).is_ok());
}
