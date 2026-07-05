use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpanMap {
    pub rel_path: String,
    pub original_hash: String,
    pub sanitized_hash: String,
    pub original_size: usize,
    pub sanitized_size: usize,
    pub language: String,
    pub replacements: Vec<Replacement>,
    pub spans: Vec<SpanSegment>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Replacement {
    pub id: usize,
    pub category: String,
    pub original_text: String,
    pub sanitized_text: String,
    pub confidence: f64,
    pub policy_source: String,
    pub stable_key: String,
    pub original_start: usize,
    pub original_end: usize,
    pub sanitized_start: usize,
    pub sanitized_end: usize,
    pub original_line_start: usize,
    pub sanitized_line_start: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpanSegment {
    pub replacement_id: Option<usize>,
    pub original_start: usize,
    pub original_end: usize,
    pub sanitized_start: usize,
    pub sanitized_end: usize,
    pub original_line_start: usize,
    pub sanitized_line_start: usize,
}

#[derive(Debug, Clone)]
pub struct PendingReplacement {
    pub category: String,
    pub original_text: String,
    pub sanitized_text: String,
    pub confidence: f64,
    pub policy_source: String,
    pub stable_key: String,
    pub original_start: usize,
    pub original_end: usize,
}

#[derive(Debug, Clone)]
pub struct RenderedSanitization {
    pub sanitized: String,
    pub span_map: SpanMap,
}

impl SpanMap {
    pub fn conflicts_with_sanitized_edit(&self, start: usize, end: usize) -> bool {
        self.replacements.iter().any(|replacement| {
            if start == end {
                start > replacement.sanitized_start && start < replacement.sanitized_end
            } else {
                start < replacement.sanitized_end && end > replacement.sanitized_start
            }
        })
    }
}

pub fn render_with_map(
    rel_path: &str,
    original: &str,
    language: &str,
    mut replacements: Vec<PendingReplacement>,
    updated_at: String,
) -> Result<RenderedSanitization> {
    replacements.sort_by_key(|replacement| replacement.original_start);
    validate_replacements(original, &replacements)?;

    let mut sanitized = String::with_capacity(original.len());
    let mut rendered_replacements = Vec::new();
    let mut spans = Vec::new();
    let mut original_cursor = 0usize;
    let mut sanitized_cursor = 0usize;

    for (idx, replacement) in replacements.into_iter().enumerate() {
        if original_cursor < replacement.original_start {
            let original_start = original_cursor;
            let original_end = replacement.original_start;
            let piece = &original[original_start..original_end];
            sanitized.push_str(piece);
            let sanitized_start = sanitized_cursor;
            sanitized_cursor += piece.len();
            spans.push(SpanSegment {
                replacement_id: None,
                original_start,
                original_end,
                sanitized_start,
                sanitized_end: sanitized_cursor,
                original_line_start: line_number_at(original, original_start),
                sanitized_line_start: line_number_at(&sanitized, sanitized_start),
            });
        }

        let id = idx + 1;
        let sanitized_start = sanitized_cursor;
        sanitized.push_str(&replacement.sanitized_text);
        sanitized_cursor += replacement.sanitized_text.len();
        let sanitized_end = sanitized_cursor;
        let original_line_start = line_number_at(original, replacement.original_start);
        let sanitized_line_start = line_number_at(&sanitized, sanitized_start);

        rendered_replacements.push(Replacement {
            id,
            category: replacement.category,
            original_text: replacement.original_text,
            sanitized_text: replacement.sanitized_text,
            confidence: replacement.confidence,
            policy_source: replacement.policy_source,
            stable_key: replacement.stable_key,
            original_start: replacement.original_start,
            original_end: replacement.original_end,
            sanitized_start,
            sanitized_end,
            original_line_start,
            sanitized_line_start,
        });
        spans.push(SpanSegment {
            replacement_id: Some(id),
            original_start: replacement.original_start,
            original_end: replacement.original_end,
            sanitized_start,
            sanitized_end,
            original_line_start,
            sanitized_line_start,
        });
        original_cursor = replacement.original_end;
    }

    if original_cursor < original.len() {
        let original_start = original_cursor;
        let original_end = original.len();
        let piece = &original[original_start..original_end];
        sanitized.push_str(piece);
        let sanitized_start = sanitized_cursor;
        sanitized_cursor += piece.len();
        spans.push(SpanSegment {
            replacement_id: None,
            original_start,
            original_end,
            sanitized_start,
            sanitized_end: sanitized_cursor,
            original_line_start: line_number_at(original, original_start),
            sanitized_line_start: line_number_at(&sanitized, sanitized_start),
        });
    }

    let span_map = SpanMap {
        rel_path: rel_path.to_string(),
        original_hash: sha256_hex(original.as_bytes()),
        sanitized_hash: sha256_hex(sanitized.as_bytes()),
        original_size: original.len(),
        sanitized_size: sanitized.len(),
        language: language.to_string(),
        replacements: rendered_replacements,
        spans,
        updated_at,
    };

    Ok(RenderedSanitization {
        sanitized,
        span_map,
    })
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub fn line_number_at(text: &str, byte_offset: usize) -> usize {
    1 + text[..byte_offset.min(text.len())]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
}

fn validate_replacements(original: &str, replacements: &[PendingReplacement]) -> Result<()> {
    let mut previous_end = 0usize;
    for replacement in replacements {
        if replacement.original_start > replacement.original_end {
            bail!("replacement start is after end");
        }
        if replacement.original_end > original.len() {
            bail!("replacement is outside file bounds");
        }
        if !original.is_char_boundary(replacement.original_start)
            || !original.is_char_boundary(replacement.original_end)
        {
            bail!("replacement does not align to UTF-8 boundaries");
        }
        if replacement.original_start < previous_end {
            bail!("replacement spans overlap");
        }
        let actual = &original[replacement.original_start..replacement.original_end];
        if actual != replacement.original_text {
            bail!(
                "replacement text mismatch: expected {:?}, got {:?}",
                replacement.original_text,
                actual
            );
        }
        if replacement.sanitized_text.contains('\n') && !replacement.original_text.contains('\n') {
            bail!("sanitizer may not introduce new lines");
        }
        previous_end = replacement.original_end;
    }
    Ok(())
}

pub fn common_changed_range(old: &str, new: &str) -> (usize, usize) {
    let mut prefix = 0usize;
    let mut old_chars = old.char_indices();
    let mut new_chars = new.char_indices();
    while let (Some((old_idx, old_ch)), Some((new_idx, new_ch))) =
        (old_chars.next(), new_chars.next())
    {
        if old_idx != new_idx || old_ch != new_ch {
            break;
        }
        prefix = old_idx + old_ch.len_utf8();
    }

    let old_tail = &old[prefix..];
    let new_tail = &new[prefix..];
    let mut suffix = 0usize;
    let mut old_rev = old_tail.char_indices().rev();
    let mut new_rev = new_tail.char_indices().rev();
    while let (Some((old_idx, old_ch)), Some((new_idx, new_ch))) = (old_rev.next(), new_rev.next())
    {
        if old_ch != new_ch {
            break;
        }
        let old_suffix = old_tail.len() - old_idx;
        let new_suffix = new_tail.len() - new_idx;
        if old_suffix != new_suffix {
            break;
        }
        suffix = old_suffix;
    }

    let old_end = old.len().saturating_sub(suffix);
    (prefix, old_end)
}

pub fn load_span_map(path: &std::path::Path) -> Result<SpanMap> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_keeps_utf8_offsets() {
        let text = "привет dangerous мир\n";
        let start = text.find("dangerous").unwrap();
        let rendered = render_with_map(
            "src/lib.rs",
            text,
            "rust",
            vec![PendingReplacement {
                category: "comment".to_string(),
                original_text: "dangerous".to_string(),
                sanitized_text: "neutral".to_string(),
                confidence: 1.0,
                policy_source: "test".to_string(),
                stable_key: "k".to_string(),
                original_start: start,
                original_end: start + "dangerous".len(),
            }],
            "now".to_string(),
        )
        .unwrap();
        assert_eq!(rendered.sanitized, "привет neutral мир\n");
        assert_eq!(rendered.span_map.replacements[0].original_start, start);
        assert_eq!(rendered.span_map.replacements[0].sanitized_start, start);
    }

    #[test]
    fn detects_conflict_inside_replacement() {
        let rendered = render_with_map(
            "src/lib.rs",
            "dangerous_name",
            "rust",
            vec![PendingReplacement {
                category: "identifier".to_string(),
                original_text: "dangerous".to_string(),
                sanitized_text: "neutral".to_string(),
                confidence: 1.0,
                policy_source: "test".to_string(),
                stable_key: "k".to_string(),
                original_start: 0,
                original_end: 9,
            }],
            "now".to_string(),
        )
        .unwrap();
        assert!(rendered.span_map.conflicts_with_sanitized_edit(1, 2));
        assert!(!rendered.span_map.conflicts_with_sanitized_edit(7, 7));
    }
}
