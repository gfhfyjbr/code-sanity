use crate::config::Config;
use crate::map::{PendingReplacement, RenderedSanitization, render_with_map, sha256_hex};
use anyhow::{Result, bail};
use chrono::Utc;
use std::collections::HashSet;
use std::path::Path;

pub trait SanitizerProvider {
    fn sanitize(
        &self,
        rel_path: &Path,
        content: &str,
        config: &Config,
    ) -> Result<RenderedSanitization>;
}

#[derive(Debug, Default)]
pub struct StubSanitizerProvider;

#[derive(Debug, Default)]
pub struct LlmSanitizerProviderStub;

impl SanitizerProvider for StubSanitizerProvider {
    fn sanitize(
        &self,
        rel_path: &Path,
        content: &str,
        config: &Config,
    ) -> Result<RenderedSanitization> {
        let language = detect_language(rel_path, content);
        let mut candidates = Vec::new();
        let string_ranges = string_ranges(&language, content);
        let comment_ranges = comment_ranges(&language, content, &string_ranges);
        let mut blocked_ranges = comment_ranges.clone();
        blocked_ranges.extend(string_ranges.clone());

        for range in &comment_ranges {
            collect_dictionary_replacements(
                rel_path,
                content,
                range.start,
                range.end,
                "comment",
                config,
                &mut candidates,
            );
        }

        for range in &string_ranges {
            if is_fixture_or_test(rel_path) || is_in_test_context(content, range.start) {
                collect_dictionary_replacements(
                    rel_path,
                    content,
                    range.start,
                    range.end,
                    "string_literal",
                    config,
                    &mut candidates,
                );
            }
        }

        collect_identifier_replacements(
            rel_path,
            content,
            config,
            &blocked_ranges,
            &mut candidates,
        );

        let replacements = select_non_overlapping(candidates);
        render_with_map(
            &crate::config::normalize_rel_path(rel_path),
            content,
            &language,
            replacements,
            Utc::now().to_rfc3339(),
        )
    }
}

impl SanitizerProvider for LlmSanitizerProviderStub {
    fn sanitize(
        &self,
        _rel_path: &Path,
        _content: &str,
        _config: &Config,
    ) -> Result<RenderedSanitization> {
        bail!("LLM sanitizer provider is scaffolded only; use provider.kind = \"stub\"")
    }
}

#[derive(Debug, Clone)]
struct ByteRange {
    start: usize,
    end: usize,
}

pub fn sanitize_content(
    rel_path: &Path,
    content: &str,
    config: &Config,
) -> Result<RenderedSanitization> {
    match &config.sanitizer.provider {
        crate::config::ProviderConfig::Stub => {
            StubSanitizerProvider.sanitize(rel_path, content, config)
        }
        crate::config::ProviderConfig::LlmStub { .. } => {
            LlmSanitizerProviderStub.sanitize(rel_path, content, config)
        }
    }
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

fn collect_dictionary_replacements(
    rel_path: &Path,
    content: &str,
    start: usize,
    end: usize,
    category: &str,
    config: &Config,
    out: &mut Vec<PendingReplacement>,
) {
    let slice = &content[start..end];
    let allowlist = config
        .sanitizer
        .allowlist
        .iter()
        .map(|item| item.to_lowercase())
        .collect::<HashSet<_>>();

    for (term, replacement) in &config.sanitizer.dictionary {
        let term_lower = term.to_ascii_lowercase();
        if allowlist.contains(&term_lower) {
            continue;
        }
        let mut search_at = 0usize;
        while let Some(local_start) = find_ascii_case_insensitive(slice, &term_lower, search_at) {
            let local_end = local_start + term_lower.len();
            if is_start_boundary(slice, local_start) && is_end_boundary(slice, local_end) {
                let original_text = slice[local_start..local_end].to_string();
                out.push(PendingReplacement {
                    category: category.to_string(),
                    sanitized_text: match_case(&original_text, replacement),
                    stable_key: stable_key(rel_path, &original_text, category, start + local_start),
                    original_text,
                    confidence: 1.0,
                    policy_source: "static-dictionary".to_string(),
                    original_start: start + local_start,
                    original_end: start + local_end,
                });
            }
            search_at = local_end;
        }
    }
}

fn collect_identifier_replacements(
    rel_path: &Path,
    content: &str,
    config: &Config,
    blocked_ranges: &[ByteRange],
    out: &mut Vec<PendingReplacement>,
) {
    let protected_identifiers = public_declaration_identifiers(content);
    let bytes = content.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let Some((start, ch)) = next_char_at(content, cursor) else {
            break;
        };
        if !is_ident_start(ch) {
            cursor = start + ch.len_utf8();
            continue;
        }

        let mut end = start + ch.len_utf8();
        while end < bytes.len() {
            let Some((idx, next)) = next_char_at(content, end) else {
                break;
            };
            if !is_ident_continue(next) {
                end = idx;
                break;
            }
            end = idx + next.len_utf8();
        }

        cursor = end;
        if range_overlaps(blocked_ranges, start, end) {
            continue;
        }

        let ident = &content[start..end];
        if protected_identifiers.contains(ident) {
            continue;
        }
        if !should_sanitize_identifier(content, start, ident) {
            continue;
        }

        let lower_ident = ident.to_lowercase();
        for (term, replacement) in &config.sanitizer.dictionary {
            let term_lower = term.to_lowercase();
            if config
                .sanitizer
                .allowlist
                .iter()
                .any(|item| item.eq_ignore_ascii_case(&term_lower))
            {
                continue;
            }
            let mut search_at = 0usize;
            while let Some(local_start) = lower_ident[search_at..].find(&term_lower) {
                let local_start = search_at + local_start;
                let local_end = local_start + term_lower.len();
                let original_text = ident[local_start..local_end].to_string();
                out.push(PendingReplacement {
                    category: "identifier".to_string(),
                    sanitized_text: match_identifier_case(&original_text, replacement),
                    stable_key: stable_key(
                        rel_path,
                        &original_text,
                        "identifier",
                        start + local_start,
                    ),
                    original_text,
                    confidence: 0.85,
                    policy_source: "static-dictionary-private-identifier".to_string(),
                    original_start: start + local_start,
                    original_end: start + local_end,
                });
                search_at = local_end;
            }
        }
    }
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

fn should_sanitize_identifier(content: &str, start: usize, ident: &str) -> bool {
    if is_keyword(ident) || ident.starts_with("__") {
        return false;
    }
    let prefix = &content[..start];
    let previous = prefix
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .rev()
        .filter(|token| !token.is_empty())
        .take(3)
        .collect::<Vec<_>>();

    if previous.iter().any(|token| {
        matches!(
            *token,
            "pub" | "export" | "import" | "from" | "use" | "mod" | "crate" | "extern"
        )
    }) {
        return false;
    }

    true
}

fn public_declaration_identifiers(content: &str) -> HashSet<String> {
    let bytes = content.as_bytes();
    let mut protected = HashSet::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let Some((start, ch)) = next_char_at(content, cursor) else {
            break;
        };
        if !is_ident_start(ch) {
            cursor = start + ch.len_utf8();
            continue;
        }

        let mut end = start + ch.len_utf8();
        while end < bytes.len() {
            let Some((idx, next)) = next_char_at(content, end) else {
                break;
            };
            if !is_ident_continue(next) {
                end = idx;
                break;
            }
            end = idx + next.len_utf8();
        }

        let ident = &content[start..end];
        if is_public_declaration_identifier(content, start, ident) {
            protected.insert(ident.to_string());
        }
        cursor = end;
    }
    protected
}

fn is_public_declaration_identifier(content: &str, start: usize, ident: &str) -> bool {
    if is_keyword(ident) {
        return false;
    }
    let previous = previous_identifier_tokens(content, start, 6);
    let Some(first) = previous.first() else {
        return false;
    };
    let declaration_before_name = matches!(
        *first,
        "fn" | "struct"
            | "enum"
            | "trait"
            | "type"
            | "const"
            | "static"
            | "mod"
            | "function"
            | "class"
            | "interface"
    );
    declaration_before_name
        && previous
            .iter()
            .skip(1)
            .any(|token| matches!(*token, "pub" | "export"))
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

fn comment_ranges(language: &str, content: &str, string_ranges: &[ByteRange]) -> Vec<ByteRange> {
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

fn string_ranges(language: &str, content: &str) -> Vec<ByteRange> {
    if matches!(language, "markdown" | "text") {
        return Vec::new();
    }

    let mut ranges = Vec::new();
    let bytes = content.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let quote = bytes[cursor];
        let is_quote = quote == b'"'
            || quote == b'\''
            || (matches!(language, "typescript" | "javascript") && quote == b'`');
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

fn is_fixture_or_test(rel_path: &Path) -> bool {
    let lowered = rel_path.to_string_lossy().to_lowercase();
    lowered.contains("fixture")
        || lowered.contains("/tests/")
        || lowered.contains("\\tests\\")
        || lowered.contains("_test.")
        || lowered.contains(".test.")
        || lowered.contains(".spec.")
        || lowered.contains("/examples/")
}

fn is_in_test_context(content: &str, start: usize) -> bool {
    let window_start = start.saturating_sub(4096);
    let before = &content[window_start..start];
    let marker = before
        .rfind("#[cfg(test)]")
        .into_iter()
        .chain(before.rfind("mod tests"))
        .max();
    let Some(marker) = marker else {
        return false;
    };
    let after_marker = &before[marker..];
    let opens = after_marker.chars().filter(|ch| *ch == '{').count();
    let closes = after_marker.chars().filter(|ch| *ch == '}').count();
    opens > closes || after_marker.contains("#[test]")
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

fn match_case(original: &str, replacement: &str) -> String {
    if original.chars().all(|ch| !ch.is_ascii_lowercase()) {
        replacement.to_ascii_uppercase()
    } else if original
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_uppercase())
    {
        let mut chars = replacement.chars();
        match chars.next() {
            Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
            None => String::new(),
        }
    } else {
        replacement.to_string()
    }
}

fn match_identifier_case(original: &str, replacement: &str) -> String {
    if original.contains('_') {
        return replacement.replace('-', "_");
    }
    match_case(original, &to_identifier_word(replacement))
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

fn is_start_boundary(text: &str, idx: usize) -> bool {
    let before = text[..idx].chars().next_back();
    before.is_none_or(|ch| !(ch.is_ascii_alphanumeric() || ch == '_'))
}

fn is_end_boundary(text: &str, idx: usize) -> bool {
    let after = text[idx..].chars().next();
    after.is_none_or(|ch| !(ch.is_ascii_alphanumeric() || ch == '_'))
}

fn is_ident_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic()
}

fn is_ident_continue(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

fn next_char_at(text: &str, start: usize) -> Option<(usize, char)> {
    text[start..]
        .char_indices()
        .next()
        .map(|(idx, ch)| (start + idx, ch))
}

fn range_contains(ranges: &[ByteRange], point: usize) -> bool {
    ranges
        .iter()
        .any(|range| point >= range.start && point < range.end)
}

fn range_overlaps(ranges: &[ByteRange], start: usize, end: usize) -> bool {
    ranges
        .iter()
        .any(|range| start < range.end && end > range.start)
}

fn find_ascii_case_insensitive(haystack: &str, needle_lower: &str, from: usize) -> Option<usize> {
    let hay = haystack.as_bytes();
    let needle = needle_lower.as_bytes();
    if needle.is_empty()
        || needle.len() > hay.len()
        || from > hay.len().saturating_sub(needle.len())
    {
        return None;
    }
    for idx in from..=hay.len() - needle.len() {
        if !haystack.is_char_boundary(idx) || !haystack.is_char_boundary(idx + needle.len()) {
            continue;
        }
        if hay[idx..idx + needle.len()]
            .iter()
            .map(|byte| byte.to_ascii_lowercase())
            .eq(needle.iter().copied())
        {
            return Some(idx);
        }
    }
    None
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

    #[test]
    fn sanitizes_comments_and_private_identifiers() {
        let config = Config::default();
        let rendered = sanitize_content(
            Path::new("src/lib.rs"),
            "// dangerous comment\nfn dangerous_parser() {}\n",
            &config,
        )
        .unwrap();
        assert!(rendered.sanitized.contains("neutral comment"));
        assert!(rendered.sanitized.contains("fn neutral_parser()"));
        assert!(!rendered.sanitized.contains("dangerous"));
    }

    #[test]
    fn skips_strings_outside_fixtures() {
        let config = Config::default();
        let rendered =
            sanitize_content(Path::new("src/lib.rs"), "let s = \"dangerous\";\n", &config).unwrap();
        assert_eq!(rendered.sanitized, "let s = \"dangerous\";\n");
    }

    proptest! {
        #[test]
        fn rendered_replacements_are_monotonic(prefix in ".*", suffix in ".*") {
            let config = Config::default();
            let input = format!("// {prefix} dangerous {suffix}\n");
            let rendered = sanitize_content(Path::new("src/lib.rs"), &input, &config).unwrap();
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
