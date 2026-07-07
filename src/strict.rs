//! Strict mode: run commands so the agent sees only sanitized names, including
//! in build/test output.
//!
//! Two runners share one reverse-map output sanitizer:
//! - `sh` runs a command in the real repo and sanitizes its stdout/stderr, so a
//!   build/test actually compiles and passes while real identifiers are hidden.
//! - `strict-run` first materializes a sanitized worktree (a copy of the mirror)
//!   outside the repo and runs the command there, so the process also reads only
//!   sanitized files.
//!
//! Output is streamed line by line (a long build shows progress immediately)
//! and rewritten with an Aho-Corasick automaton in leftmost-longest mode.
//!
//! This is a guardrail, not a hard sandbox. Absolute paths, network, or an
//! escape out of the worktree can still reach the real repo; true isolation
//! needs an overlay/FUSE/container (optional, see docs/THREAT_MODEL.md).

use crate::config::{Config, Layout};
use crate::db;
use crate::map::{load_span_map, sha256_hex};
use aho_corasick::{AhoCorasick, AhoCorasickKind, MatchKind};
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Compiled output sanitizer: real original text -> sanitized alias.
pub struct OutputSanitizer {
    automaton: Option<AhoCorasick>,
    replacements: Vec<String>,
}

impl OutputSanitizer {
    pub fn new(pairs: &[(String, String)]) -> Result<Self> {
        if pairs.is_empty() {
            return Ok(Self {
                automaton: None,
                replacements: Vec::new(),
            });
        }
        let automaton = AhoCorasick::builder()
            .kind(Some(AhoCorasickKind::DFA))
            .match_kind(MatchKind::LeftmostLongest)
            .build(pairs.iter().map(|(original, _)| original.as_str()))
            .context("build output sanitizer automaton")?;
        Ok(Self {
            automaton: Some(automaton),
            replacements: pairs.iter().map(|(_, alias)| alias.clone()).collect(),
        })
    }

    pub fn sanitize(&self, text: &str) -> String {
        match &self.automaton {
            None => text.to_string(),
            Some(automaton) => automaton.replace_all(text, &self.replacements),
        }
    }
}

/// Run `command` with output sanitized through the reverse map. When
/// `in_worktree` is set the command runs inside a fresh sanitized worktree.
/// stdout/stderr are streamed line by line as the child produces them.
/// Returns the child's exit code.
pub fn run(root: &Path, command: &[String], in_worktree: bool) -> Result<i32> {
    if command.is_empty() {
        bail!("no command given; usage: code-sanity sh -- <cmd> [args...]");
    }
    // Snapshot (reverse pairs + worktree copy) under a shared lock so a
    // concurrent index/apply cannot produce a torn view; released before the
    // child runs so long commands do not starve writers.
    let (sanitizer, worktree) = {
        let layout = crate::config::Layout::new(root);
        let _lock = crate::lock::WorkspaceLock::acquire_shared(&layout)?;
        let pairs = build_reverse_pairs(root)?;
        let sanitizer = std::sync::Arc::new(OutputSanitizer::new(&pairs)?);
        let worktree = if in_worktree {
            Some(materialize_worktree(root)?)
        } else {
            None
        };
        (sanitizer, worktree)
    };
    let cwd = worktree
        .as_ref()
        .map(|worktree| worktree.path.clone())
        .unwrap_or_else(|| root.to_path_buf());

    let (program, args) = command.split_first().expect("command is non-empty");
    let mut child = Command::new(program)
        .args(args)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("run {program}"))?;

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let out_sanitizer = sanitizer.clone();
    let out_thread = std::thread::spawn(move || {
        stream_sanitized(stdout, &mut std::io::stdout().lock(), &out_sanitizer)
    });
    let err_sanitizer = sanitizer.clone();
    let err_thread = std::thread::spawn(move || {
        stream_sanitized(stderr, &mut std::io::stderr().lock(), &err_sanitizer)
    });

    let status = child
        .wait()
        .with_context(|| format!("wait for {program}"))?;
    out_thread
        .join()
        .map_err(|_| anyhow::anyhow!("stdout stream thread panicked"))?
        .context("stream sanitized stdout")?;
    err_thread
        .join()
        .map_err(|_| anyhow::anyhow!("stderr stream thread panicked"))?
        .context("stream sanitized stderr")?;

    Ok(status.code().unwrap_or(1))
}

/// Copy one stream to a writer line by line, sanitizing each line before it
/// is written and flushing immediately so output streams in real time.
fn stream_sanitized(
    reader: impl std::io::Read,
    writer: &mut impl Write,
    sanitizer: &OutputSanitizer,
) -> Result<()> {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .context("read child output")?;
        if read == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&line);
        writer
            .write_all(sanitizer.sanitize(&text).as_bytes())
            .context("write sanitized output")?;
        writer.flush().context("flush sanitized output")?;
    }
    Ok(())
}

/// Build the reverse replacement table (real original text -> sanitized alias)
/// from every tracked file's span map, plus the config dictionary, registry,
/// and denylist. Sorted longest-original first (the automaton is leftmost-
/// longest, but a deterministic order keeps replacement choices stable).
pub fn build_reverse_pairs(root: &Path) -> Result<Vec<(String, String)>> {
    let layout = Layout::new(root);
    let config = Config::load_or_default(&layout)?;
    let conn = db::connect(&layout)?;
    db::init_schema(&conn)?;

    let mut map: BTreeMap<String, String> = BTreeMap::new();
    for file in db::tracked_files(&conn)? {
        if let Ok(span_map) = load_span_map(&layout.map_path(Path::new(&file))) {
            for replacement in &span_map.replacements {
                map.entry(replacement.original_text.clone())
                    .or_insert_with(|| replacement.sanitized_text.clone());
            }
        }
    }
    for (term, alias) in &config.sanitizer.dictionary {
        map.entry(term.clone()).or_insert_with(|| alias.clone());
    }
    for (term, alias) in &config.sanitizer.alias_registry {
        map.entry(term.clone()).or_insert_with(|| alias.clone());
    }
    for term in &config.sanitizer.denylist {
        map.entry(term.clone())
            .or_insert_with(|| crate::sanitize::derive_alias(&config.salt, term));
    }

    let mut pairs: Vec<(String, String)> = map
        .into_iter()
        .filter(|(original, sanitized)| original.len() >= 2 && original != sanitized)
        .collect();
    pairs.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
    Ok(pairs)
}

/// Substring replacement of real originals with aliases (kept for tests and
/// small one-shot texts; the streaming path uses the same automaton). Fails
/// closed: a sanitizer that cannot be built is an error, never unsanitized
/// pass-through.
pub fn sanitize_output(text: &str, pairs: &[(String, String)]) -> Result<String> {
    Ok(OutputSanitizer::new(pairs)?.sanitize(text))
}

/// A per-run sanitized worktree in a private (0700) unique directory, removed
/// on drop.
struct Worktree {
    path: PathBuf,
}

impl Drop for Worktree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Copy the sanitized mirror into a fresh worktree outside the repo tree, so
/// the real root is not a parent of the process cwd. Each run gets a unique
/// directory readable only by the owner, so parallel runs never share state
/// and other local users cannot read the (sanitized) sources.
fn materialize_worktree(root: &Path) -> Result<Worktree> {
    let layout = Layout::new(root);
    if !layout.mirror_dir.exists() {
        bail!("sanitized mirror is missing; run `code-sanity index` first");
    }
    let key = sha256_hex(root.to_string_lossy().as_bytes());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let dest = std::env::temp_dir().join(format!(
        "code-sanity-worktree-{}-{}-{nanos}",
        &key[..16],
        std::process::id()
    ));

    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&dest)
        .with_context(|| format!("create worktree {}", dest.display()))?;
    let worktree = Worktree { path: dest };
    copy_dir(&layout.mirror_dir, &worktree.path)?;
    Ok(worktree)
}

fn copy_dir(source: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    for entry in std::fs::read_dir(source).with_context(|| format!("read {}", source.display()))? {
        let entry = entry.context("read worktree source entry")?;
        let file_type = entry.file_type().context("stat worktree source entry")?;
        let target = dest.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), &target)
                .with_context(|| format!("copy to {}", target.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_sanitizer_prefers_longest_and_hides_substrings() {
        let pairs = vec![
            ("dangerous_parser".to_string(), "special".to_string()),
            ("dangerous".to_string(), "neutral".to_string()),
            ("evil".to_string(), "sample".to_string()),
        ];
        let out = sanitize_output("fn dangerous_parser() // evil dangerous", &pairs).unwrap();
        assert_eq!(out, "fn special() // sample neutral");
    }

    #[test]
    fn empty_pairs_pass_text_through() {
        assert_eq!(sanitize_output("hello", &[]).unwrap(), "hello");
    }
}
