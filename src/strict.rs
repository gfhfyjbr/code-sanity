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
//! This is a guardrail, not a hard sandbox. Absolute paths, network, or an
//! escape out of the worktree can still reach the real repo; true isolation
//! needs an overlay/FUSE/container (optional, see docs/THREAT_MODEL.md).

use crate::config::{Config, Layout};
use crate::db;
use crate::map::{load_span_map, sha256_hex};
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Run `command` with output sanitized through the reverse map. When
/// `in_worktree` is set the command runs inside a fresh sanitized worktree.
/// Returns the child's exit code.
pub fn run(root: &Path, command: &[String], in_worktree: bool) -> Result<i32> {
    if command.is_empty() {
        bail!("no command given; usage: code-sanity sh -- <cmd> [args...]");
    }
    let pairs = build_reverse_pairs(root)?;
    let cwd = if in_worktree {
        materialize_worktree(root)?
    } else {
        root.to_path_buf()
    };

    let (program, args) = command.split_first().expect("command is non-empty");
    let output = Command::new(program)
        .args(args)
        .current_dir(&cwd)
        .output()
        .with_context(|| format!("run {program}"))?;

    let out = sanitize_output(&String::from_utf8_lossy(&output.stdout), &pairs);
    let err = sanitize_output(&String::from_utf8_lossy(&output.stderr), &pairs);
    std::io::stdout()
        .write_all(out.as_bytes())
        .context("write sanitized stdout")?;
    std::io::stderr()
        .write_all(err.as_bytes())
        .context("write sanitized stderr")?;
    Ok(output.status.code().unwrap_or(1))
}

/// Build the reverse replacement table (real original text -> sanitized alias)
/// from every tracked file's span map, plus the config dictionary and registry.
/// Sorted longest-original first so greedy replacement prefers longer matches.
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

    let mut pairs: Vec<(String, String)> = map
        .into_iter()
        .filter(|(original, sanitized)| original.len() >= 2 && original != sanitized)
        .collect();
    pairs.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
    Ok(pairs)
}

/// Greedy left-to-right substring replacement of real originals with aliases.
/// Substring (not whole-word) so that a term replaced inside an identifier or a
/// path in build output is hidden just as it is in the mirror.
pub fn sanitize_output(text: &str, pairs: &[(String, String)]) -> String {
    if pairs.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    while cursor < text.len() {
        let rest = &text[cursor..];
        let mut matched = false;
        for (original, sanitized) in pairs {
            if rest.starts_with(original.as_str()) {
                out.push_str(sanitized);
                cursor += original.len();
                matched = true;
                break;
            }
        }
        if !matched {
            let ch = rest.chars().next().expect("cursor within text");
            out.push(ch);
            cursor += ch.len_utf8();
        }
    }
    out
}

/// Copy the sanitized mirror into a stable worktree outside the repo tree, so the
/// real root is not a parent of the process cwd.
fn materialize_worktree(root: &Path) -> Result<PathBuf> {
    let layout = Layout::new(root);
    if !layout.mirror_dir.exists() {
        bail!("sanitized mirror is missing; run `code-sanity index` first");
    }
    let key = sha256_hex(root.to_string_lossy().as_bytes());
    let dest = std::env::temp_dir().join(format!("code-sanity-worktree-{}", &key[..16]));
    if dest.exists() {
        std::fs::remove_dir_all(&dest)
            .with_context(|| format!("clear worktree {}", dest.display()))?;
    }
    copy_dir(&layout.mirror_dir, &dest)?;
    Ok(dest)
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
    fn greedy_output_sanitizer_prefers_longest_and_hides_substrings() {
        let pairs = vec![
            ("dangerous".to_string(), "neutral".to_string()),
            ("evil".to_string(), "sample".to_string()),
        ];
        let mut sorted = pairs.clone();
        sorted.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        let out = sanitize_output("fn dangerous_parser() // evil", &sorted);
        assert_eq!(out, "fn neutral_parser() // sample");
    }
}
