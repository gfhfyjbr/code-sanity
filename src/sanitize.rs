//! Deterministic lexical sanitizer.
//!
//! One matching primitive is shared by the sanitizer and the verify leak
//! backstop: every dictionary / denylist / alias-registry term is matched
//! case-insensitively and underscore-insensitively inside word runs
//! (`[A-Za-z0-9_]+`), in comments, string literals, and identifiers alike.
//! `AcmeClient` therefore also catches `ACME_CLIENT` and `acmeClientFactory`.
//!
//! The only sanctioned residues of a term in the mirror are word runs in the
//! repo-wide protected identifier set (public declarations and import-position
//! names, collected from the real files). That set is name-based: one symbol
//! gets one decision across the whole mirror, and `verify` can independently
//! recompute it to tell a sanctioned residue from a leak.

use crate::config::Config;
use crate::map::{PendingReplacement, RenderedSanitization, render_with_map, sha256_hex};
use anyhow::Result;
use chrono::Utc;
use std::collections::BTreeSet;
use std::path::Path;

/// Bump when matching/rendering semantics change. Part of the logic
/// fingerprint, so an upgrade re-renders every mirror file.
pub const SANITIZER_BEHAVIOR_VERSION: u32 = 2;

/// One term the sanitizer must remove, with its normalized matching form.
#[derive(Debug, Clone)]
pub struct Term {
    /// Original configured spelling, for reports.
    pub raw: String,
    /// ASCII-lowercased with `_`/`-` removed; the matching form.
    pub normalized: String,
    pub replacement: String,
    pub policy_source: &'static str,
}

/// Build the full term table from config: alias registry, dictionary, and
/// denylist (denylist terms get a deterministic salted alias so they are
/// removed even before a human approves a nicer name). Allowlisted terms are
/// excluded.
pub fn term_table(config: &Config) -> Vec<Term> {
    let allow: BTreeSet<String> = config
        .sanitizer
        .allowlist
        .iter()
        .map(|item| normalize_term(item))
        .collect();
    let mut seen = BTreeSet::new();
    let mut terms = Vec::new();
    let mut push = |raw: &str, replacement: String, policy_source: &'static str| {
        let normalized = normalize_term(raw);
        if normalized.is_empty() || allow.contains(&normalized) || !seen.insert(normalized.clone())
        {
            return;
        }
        terms.push(Term {
            raw: raw.to_string(),
            normalized,
            replacement,
            policy_source,
        });
    };

    for (term, alias) in &config.sanitizer.alias_registry {
        push(term, alias.clone(), "alias-registry");
    }
    for (term, replacement) in &config.sanitizer.dictionary {
        push(term, replacement.clone(), "static-dictionary");
    }
    for term in &config.sanitizer.denylist {
        push(term, derive_alias(&config.salt, term), "denylist-auto");
    }
    terms
}

pub(crate) fn normalize_term(term: &str) -> String {
    term.chars()
        .filter(|ch| *ch != '_' && *ch != '-')
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

/// Deterministic alias for a term with no human-chosen mapping yet.
pub fn derive_alias(salt: &str, original: &str) -> String {
    format!(
        "sym_{}",
        &sha256_hex(format!("{salt}:{original}").as_bytes())[..8]
    )
}

/// A term occurrence inside a word run.
#[derive(Debug, Clone)]
pub struct TermHit {
    pub start: usize,
    pub end: usize,
    pub term_index: usize,
}

fn is_word_byte(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

/// Byte ranges of maximal `[A-Za-z0-9_]+` runs. Word bytes are ASCII, so run
/// boundaries always sit on UTF-8 character boundaries.
pub fn word_runs(content: &str) -> Vec<(usize, usize)> {
    let bytes = content.as_bytes();
    let mut runs = Vec::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if is_word_byte(bytes[cursor]) {
            let start = cursor;
            while cursor < bytes.len() && is_word_byte(bytes[cursor]) {
                cursor += 1;
            }
            runs.push((start, cursor));
        } else {
            cursor += 1;
        }
    }
    runs
}

/// Find every term occurrence in one word run, case- and underscore-
/// insensitively. Hit offsets are absolute (based on `run_start`).
pub fn hits_in_run(run: &str, run_start: usize, terms: &[Term], out: &mut Vec<TermHit>) {
    let mut normalized = String::with_capacity(run.len());
    let mut offsets = Vec::with_capacity(run.len());
    for (idx, byte) in run.bytes().enumerate() {
        if byte == b'_' {
            continue;
        }
        normalized.push(byte.to_ascii_lowercase() as char);
        offsets.push(idx);
    }
    for (term_index, term) in terms.iter().enumerate() {
        let needle = term.normalized.as_str();
        if needle.is_empty() || needle.len() > normalized.len() {
            continue;
        }
        let mut from = 0usize;
        while let Some(found) = normalized[from..].find(needle) {
            let found = from + found;
            let start = offsets[found];
            let end = offsets[found + needle.len() - 1] + 1;
            out.push(TermHit {
                start: run_start + start,
                end: run_start + end,
                term_index,
            });
            from = found + needle.len();
        }
    }
}

/// Sanitize one word run exactly as `sanitize_content` would (same hit
/// selection order and case adaptation), returning the replaced text. The
/// patch bridge uses this to roundtrip-check reverse-mapped identifiers in
/// newly added lines.
pub fn sanitize_run_text(run: &str, terms: &[Term], protected: &BTreeSet<String>) -> String {
    if is_keyword(run) || run.starts_with("__") || protected.contains(run) {
        return run.to_string();
    }
    let mut hits = Vec::new();
    hits_in_run(run, 0, terms, &mut hits);
    hits.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then_with(|| (b.end - b.start).cmp(&(a.end - a.start)))
    });
    let mut out = String::with_capacity(run.len());
    let mut cursor = 0usize;
    for hit in &hits {
        if hit.start < cursor {
            continue;
        }
        out.push_str(&run[cursor..hit.start]);
        out.push_str(&adapt_replacement(
            &run[hit.start..hit.end],
            &terms[hit.term_index].replacement,
        ));
        cursor = hit.end;
    }
    out.push_str(&run[cursor..]);
    out
}

/// Collect the identifiers this file protects from sanitization: public
/// declarations, import-position names, and dunder names. Name-based on
/// purpose: protecting the name everywhere is the only way "one symbol, one
/// decision" and an independent verify backstop can both hold.
pub fn collect_protected_identifiers(content: &str) -> BTreeSet<String> {
    let mut protected = BTreeSet::new();

    // Line rule: a line led by a declaration/import marker keeps every word
    // run before any trailing comment. Covers `use a::b::c::d;`, `import x`,
    // `pub fn name(args)` signatures, `export const x`.
    let mut line_start = 0usize;
    for line in content.split_inclusive('\n') {
        let code_end = line
            .find("//")
            .or_else(|| line.find('#'))
            .unwrap_or(line.len());
        let code = &line[..code_end];
        let runs = word_runs(code);
        let lead: Vec<&str> = runs
            .iter()
            .take(2)
            .map(|(start, end)| &code[*start..*end])
            .collect();
        let marker = |token: &str| {
            matches!(
                token,
                "use" | "import" | "from" | "mod" | "package" | "require" | "pub" | "export"
            )
        };
        if lead.iter().any(|token| marker(token)) {
            for (start, end) in &runs {
                let run = &code[*start..*end];
                if !is_keyword(run) {
                    protected.insert(run.to_string());
                }
            }
        }
        line_start += line.len();
    }
    let _ = line_start;

    // Token rule: an identifier within four tokens after a visibility or
    // import marker is protected (struct fields after `pub`, later path
    // segments, `extern` items).
    for (start, end) in word_runs(content) {
        let run = &content[start..end];
        if is_keyword(run) {
            continue;
        }
        if run.starts_with("__") {
            protected.insert(run.to_string());
            continue;
        }
        let previous = previous_identifier_tokens(content, start, 4);
        if previous.iter().any(|token| {
            matches!(
                *token,
                "pub" | "export" | "import" | "from" | "use" | "mod" | "crate" | "extern"
            )
        }) {
            protected.insert(run.to_string());
        }
    }

    protected
}

/// A residual term occurrence found by the leak backstop.
#[derive(Debug, Clone)]
pub struct Leak {
    pub offset: usize,
    pub term: String,
    pub enclosing: String,
}

/// Scan text with the same primitive the sanitizer uses and report every term
/// occurrence whose enclosing word run is not a sanctioned residue (protected
/// identifier, keyword, or dunder). Used by `verify` as an independent
/// backstop over the mirror and the span-map replacement outputs.
pub fn find_leaks(content: &str, terms: &[Term], protected: &BTreeSet<String>) -> Vec<Leak> {
    let mut leaks = Vec::new();
    let mut hits = Vec::new();
    for (run_start, run_end) in word_runs(content) {
        let run = &content[run_start..run_end];
        if is_keyword(run) || run.starts_with("__") || protected.contains(run) {
            continue;
        }
        hits.clear();
        hits_in_run(run, run_start, terms, &mut hits);
        for hit in &hits {
            leaks.push(Leak {
                offset: hit.start,
                term: terms[hit.term_index].raw.clone(),
                enclosing: run.to_string(),
            });
        }
    }
    leaks
}

pub fn sanitize_content(
    rel_path: &Path,
    content: &str,
    config: &Config,
    protected: &BTreeSet<String>,
) -> Result<RenderedSanitization> {
    let language = detect_language(rel_path, content);
    let string_ranges = string_ranges(&language, content);
    let comment_ranges = comment_ranges(&language, content, &string_ranges);
    let terms = term_table(config);

    let mut candidates = Vec::new();
    let mut hits = Vec::new();
    for (run_start, run_end) in word_runs(content) {
        let run = &content[run_start..run_end];
        if is_keyword(run) || run.starts_with("__") || protected.contains(run) {
            continue;
        }
        hits.clear();
        hits_in_run(run, run_start, &terms, &mut hits);
        for hit in &hits {
            let term = &terms[hit.term_index];
            let slice = &content[hit.start..hit.end];
            let category = if range_contains(&comment_ranges, hit.start) {
                "comment"
            } else if range_contains(&string_ranges, hit.start) {
                "string_literal"
            } else {
                "identifier"
            };
            let (policy_source, confidence) =
                if term.policy_source == "static-dictionary" && category == "identifier" {
                    ("static-dictionary-private-identifier", 0.85)
                } else {
                    (term.policy_source, 1.0)
                };
            candidates.push(PendingReplacement {
                category: category.to_string(),
                sanitized_text: adapt_replacement(slice, &term.replacement),
                stable_key: stable_key(rel_path, slice, category, hit.start),
                original_text: slice.to_string(),
                confidence,
                policy_source: policy_source.to_string(),
                original_start: hit.start,
                original_end: hit.end,
            });
        }
    }

    let replacements = select_non_overlapping(candidates);
    render_with_map(
        &crate::config::normalize_rel_path(rel_path),
        content,
        &language,
        replacements,
        Utc::now().to_rfc3339(),
    )
}

pub fn detect_language(rel_path: &Path, _content: &str) -> String {
    match rel_path.extension().and_then(|ext| ext.to_str()) {
        Some("rs") => "rust",
        Some("py") => "python",
        Some("ts") | Some("tsx") => "typescript",
        Some("js") | Some("jsx") => "javascript",
        Some("md") | Some("markdown") => "markdown",
        Some("toml") => "toml",
        Some("json") => "json",
        Some("go") => "go",
        _ => "text",
    }
    .to_string()
}

fn select_non_overlapping(mut candidates: Vec<PendingReplacement>) -> Vec<PendingReplacement> {
    candidates.sort_by(|a, b| {
        a.original_start.cmp(&b.original_start).then_with(|| {
            (b.original_end - b.original_start).cmp(&(a.original_end - a.original_start))
        })
    });

    let mut selected: Vec<PendingReplacement> = Vec::new();
    for candidate in candidates {
        if selected
            .last()
            .is_none_or(|last| candidate.original_start >= last.original_end)
        {
            selected.push(candidate);
        }
    }
    selected
}

fn previous_identifier_tokens(content: &str, start: usize, limit: usize) -> Vec<&str> {
    content[..start]
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .rev()
        .filter(|token| !token.is_empty())
        .take(limit)
        .collect()
}

fn is_keyword(ident: &str) -> bool {
    matches!(
        ident,
        "as" | "async"
            | "await"
            | "break"
            | "const"
            | "continue"
            | "crate"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            | "class"
            | "def"
            | "import"
            | "from"
            | "export"
            | "function"
            | "var"
    )
}

pub(crate) fn comment_ranges(
    language: &str,
    content: &str,
    string_ranges: &[ByteRange],
) -> Vec<ByteRange> {
    if language == "markdown" {
        return vec![ByteRange {
            start: 0,
            end: content.len(),
        }];
    }

    let mut ranges = Vec::new();
    let bytes = content.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if range_contains(string_ranges, cursor) {
            cursor += 1;
            continue;
        }
        if matches!(language, "rust" | "typescript" | "javascript" | "go") {
            if bytes[cursor..].starts_with(b"//") {
                let end = find_byte(bytes, cursor, b'\n').unwrap_or(content.len());
                ranges.push(ByteRange {
                    start: cursor + 2,
                    end,
                });
                cursor = end;
                continue;
            }
            if bytes[cursor..].starts_with(b"/*") {
                let end = find_bytes(bytes, cursor + 2, b"*/").unwrap_or(content.len());
                ranges.push(ByteRange {
                    start: cursor + 2,
                    end,
                });
                cursor = (end + 2).min(content.len());
                continue;
            }
        }
        if language == "python" && bytes[cursor] == b'#' {
            let end = find_byte(bytes, cursor, b'\n').unwrap_or(content.len());
            ranges.push(ByteRange {
                start: cursor + 1,
                end,
            });
            cursor = end;
            continue;
        }
        cursor += 1;
    }
    ranges
}

#[derive(Debug, Clone)]
pub(crate) struct ByteRange {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// String literal ranges (for categorization only; sanitization no longer
/// depends on zone detection). `'` is not a string delimiter in Rust or Go —
/// lifetimes (`&'a str`) and runes would otherwise open phantom strings.
pub(crate) fn string_ranges(language: &str, content: &str) -> Vec<ByteRange> {
    if matches!(language, "markdown" | "text") {
        return Vec::new();
    }

    let mut ranges = Vec::new();
    let bytes = content.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let quote = bytes[cursor];
        let is_quote = match language {
            "rust" => quote == b'"',
            "go" => quote == b'"' || quote == b'`',
            "typescript" | "javascript" => quote == b'"' || quote == b'\'' || quote == b'`',
            "json" => quote == b'"',
            _ => quote == b'"' || quote == b'\'',
        };
        if !is_quote {
            cursor += 1;
            continue;
        }

        if language == "python"
            && cursor + 2 < bytes.len()
            && bytes[cursor + 1] == quote
            && bytes[cursor + 2] == quote
        {
            let content_start = cursor + 3;
            let mut end = content_start;
            while end + 2 < bytes.len() {
                if bytes[end] == quote && bytes[end + 1] == quote && bytes[end + 2] == quote {
                    ranges.push(ByteRange {
                        start: content_start,
                        end,
                    });
                    cursor = end + 3;
                    break;
                }
                end += 1;
            }
            if end + 2 >= bytes.len() {
                ranges.push(ByteRange {
                    start: content_start,
                    end: content.len(),
                });
                break;
            }
            continue;
        }

        // Go raw strings have no escapes and may span lines.
        if language == "go" && quote == b'`' {
            let content_start = cursor + 1;
            let end = find_byte(bytes, content_start, b'`').unwrap_or(content.len());
            ranges.push(ByteRange {
                start: content_start,
                end,
            });
            cursor = (end + 1).min(content.len());
            continue;
        }

        let content_start = cursor + 1;
        cursor += 1;
        let mut escaped = false;
        while cursor < bytes.len() {
            if escaped {
                escaped = false;
                cursor += 1;
                continue;
            }
            if bytes[cursor] == b'\\' {
                escaped = true;
                cursor += 1;
                continue;
            }
            if bytes[cursor] == quote {
                ranges.push(ByteRange {
                    start: content_start,
                    end: cursor,
                });
                cursor += 1;
                break;
            }
            if bytes[cursor] == b'\n' && quote != b'`' {
                break;
            }
            cursor += 1;
        }
    }
    ranges
}

fn stable_key(rel_path: &Path, original_text: &str, category: &str, offset: usize) -> String {
    sha256_hex(
        format!(
            "{}:{category}:{offset}:{original_text}",
            crate::config::normalize_rel_path(rel_path)
        )
        .as_bytes(),
    )
}

/// Adapt a replacement to the casing of the matched slice: `ACME_CLIENT` gets
/// an upper-cased alias, `Acme` a capitalized one, `acme` the plain form.
/// Non-identifier characters in the replacement become `_` so identifiers
/// stay valid.
pub fn adapt_replacement(original: &str, replacement: &str) -> String {
    let base = to_identifier_word(replacement);
    let has_letters = original.chars().any(|ch| ch.is_ascii_alphabetic());
    let has_lower = original.chars().any(|ch| ch.is_ascii_lowercase());
    let has_upper = original.chars().any(|ch| ch.is_ascii_uppercase());
    if has_letters && !has_lower {
        return base.to_ascii_uppercase();
    }
    if has_letters && !has_upper {
        return base.to_ascii_lowercase();
    }
    if original
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_uppercase())
    {
        let mut chars = base.chars();
        return match chars.next() {
            Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
            None => String::new(),
        };
    }
    base
}

fn to_identifier_word(replacement: &str) -> String {
    replacement
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

pub(crate) fn range_contains(ranges: &[ByteRange], point: usize) -> bool {
    ranges
        .iter()
        .any(|range| point >= range.start && point < range.end)
}

fn find_byte(bytes: &[u8], from: usize, needle: u8) -> Option<usize> {
    bytes[from..]
        .iter()
        .position(|byte| *byte == needle)
        .map(|idx| from + idx)
}

fn find_bytes(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > bytes.len().saturating_sub(from) {
        return None;
    }
    (from..=bytes.len() - needle.len()).find(|idx| &bytes[*idx..*idx + needle.len()] == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use proptest::prelude::*;

    fn sanitize(rel: &str, content: &str) -> RenderedSanitization {
        let config = Config::default();
        sanitize_content(Path::new(rel), content, &config, &BTreeSet::new()).unwrap()
    }

    #[test]
    fn sanitizes_comments_and_private_identifiers() {
        let rendered = sanitize(
            "src/lib.rs",
            "// dangerous comment\nfn dangerous_parser() {}\n",
        );
        assert!(rendered.sanitized.contains("neutral comment"));
        assert!(rendered.sanitized.contains("fn neutral_parser()"));
        assert!(!rendered.sanitized.contains("dangerous"));
    }

    #[test]
    fn sanitizes_all_string_literals() {
        let rendered = sanitize("src/lib.rs", "let s = \"dangerous\";\n");
        assert_eq!(rendered.sanitized, "let s = \"neutral\";\n");
        assert_eq!(rendered.span_map.replacements[0].category, "string_literal");
    }

    #[test]
    fn lifetimes_do_not_open_phantom_strings() {
        let rendered = sanitize(
            "src/lib.rs",
            "fn f<'a>(acme_x: &'a str) -> &'a str {\n    acme_x\n}\n",
        );
        assert!(rendered.sanitized.contains("client_x"));
        assert!(!rendered.sanitized.contains("acme_x"));
    }

    #[test]
    fn registry_matches_case_and_subword_variants() {
        let mut config = Config::default();
        config
            .sanitizer
            .alias_registry
            .insert("AcmeClient".to_string(), "GadgetSvc".to_string());
        let content = "fn a() { AcmeClient::new(); }\nconst ACME_CLIENT: u8 = 1;\nlet f = acmeClientFactory;\n";
        let rendered =
            sanitize_content(Path::new("src/lib.rs"), content, &config, &BTreeSet::new()).unwrap();
        assert!(rendered.sanitized.contains("GadgetSvc::new()"));
        assert!(rendered.sanitized.contains("const GADGETSVC: u8"));
        assert!(rendered.sanitized.contains("GadgetSvcFactory"));
        assert!(!rendered.sanitized.to_lowercase().contains("acme"));
    }

    #[test]
    fn denylist_terms_get_derived_aliases_everywhere() {
        let mut config = Config::default();
        config.sanitizer.denylist = vec!["shadowfax".to_string()];
        let content = "// shadowfax rollout\nlet shadowfax_kill = 1;\nlet s = \"shadowfax\";\n";
        let rendered =
            sanitize_content(Path::new("src/lib.rs"), content, &config, &BTreeSet::new()).unwrap();
        assert!(!rendered.sanitized.to_lowercase().contains("shadowfax"));
        assert!(rendered.sanitized.contains("sym_"));
    }

    #[test]
    fn protected_identifiers_are_kept_repo_wide() {
        let config = Config::default();
        let mut protected = BTreeSet::new();
        protected.insert("dangerous_parser".to_string());
        let rendered = sanitize_content(
            Path::new("src/lib.rs"),
            "fn call() { dangerous_parser(); }\n",
            &config,
            &protected,
        )
        .unwrap();
        assert!(rendered.sanitized.contains("dangerous_parser()"));
    }

    #[test]
    fn public_declarations_and_imports_are_collected_as_protected() {
        let protected = collect_protected_identifiers(
            "pub fn dangerous_parser() {}\nuse dangerous_lib::helper::thing;\nfn private_one() {}\n",
        );
        assert!(protected.contains("dangerous_parser"));
        assert!(protected.contains("dangerous_lib"));
        assert!(protected.contains("thing"));
        assert!(!protected.contains("private_one"));
    }

    #[test]
    fn find_leaks_reports_unsanctioned_terms_only() {
        let config = Config::default();
        let terms = term_table(&config);
        let mut protected = BTreeSet::new();
        protected.insert("dangerous_parser".to_string());
        let leaks = find_leaks("dangerous_parser and dangerous_thing", &terms, &protected);
        assert_eq!(leaks.len(), 1);
        assert_eq!(leaks[0].enclosing, "dangerous_thing");
    }

    proptest! {
        #[test]
        fn rendered_replacements_are_monotonic(prefix in ".*", suffix in ".*") {
            let config = Config::default();
            let input = format!("// {prefix} dangerous {suffix}\n");
            let rendered = sanitize_content(
                Path::new("src/lib.rs"),
                &input,
                &config,
                &BTreeSet::new(),
            ).unwrap();
            let mut last_original = 0usize;
            let mut last_sanitized = 0usize;
            for span in rendered.span_map.spans {
                prop_assert!(span.original_start >= last_original);
                prop_assert!(span.sanitized_start >= last_sanitized);
                last_original = span.original_end;
                last_sanitized = span.sanitized_end;
            }
        }
    }
}
