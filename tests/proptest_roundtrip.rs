//! Property test for the core bridge invariant: a random file plus a patch
//! outside replacement spans roundtrips byte-for-byte in BOTH directions:
//! sanitize(patched real) == patched mirror, and reverse-projecting the
//! patched mirror through the span map reproduces the patched real file.

use code_sanity::map::SpanMap;
use code_sanity::{index_workspace, verify_workspace, write_sanitized_content};
use proptest::prelude::*;
use std::fs;
use std::path::Path;

const TEMPLATES: &[&str] = &[
    "// dangerous comment with acme details",
    "fn dangerous_parser() -> usize {",
    "    let value = 1;",
    "}",
    "let s = \"acme runtime string\";",
    "fn helper_widget() -> usize { 2 }",
];

// Words safe to type into the mirror: neutral fillers plus aliases the agent
// legitimately sees ("neutral" reverse-maps to "dangerous" when the file
// carries that span pair).
const INSERT_WORDS: &[&str] = &["alpha", "beta", "gamma", "value", "neutral", "sample"];

/// Reverse-project a sanitized mirror through its span map: splice every
/// replacement's original text back over its sanitized span.
fn reverse_project(span_map: &SpanMap, mirror: &str) -> String {
    let mut replacements: Vec<_> = span_map.replacements.iter().collect();
    replacements.sort_by_key(|replacement| replacement.sanitized_start);
    let mut out = String::with_capacity(mirror.len());
    let mut cursor = 0usize;
    for replacement in replacements {
        out.push_str(&mirror[cursor..replacement.sanitized_start]);
        out.push_str(&replacement.original_text);
        cursor = replacement.sanitized_end;
    }
    out.push_str(&mirror[cursor..]);
    out
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]
    #[test]
    fn patch_outside_spans_roundtrips_bidirectionally(
        line_picks in prop::collection::vec(0usize..TEMPLATES.len(), 3..10),
        insert_pos in 0usize..64,
        word_picks in prop::collection::vec(0usize..INSERT_WORDS.len(), 1..5),
    ) {
        let repo = tempfile::tempdir().unwrap();
        fs::create_dir_all(repo.path().join("src")).unwrap();
        let content = line_picks
            .iter()
            .map(|pick| TEMPLATES[*pick])
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(repo.path().join("src/gen.rs"), &content).unwrap();
        index_workspace(repo.path()).unwrap();

        // Insert one new line at a random line boundary. The zzz_ prefix keeps
        // the changed region from bleeding into neighbouring lines (and thus
        // into replacement spans) via common prefix/suffix trimming.
        let mirror_path = repo.path().join(".code-sanity/mirror/src/gen.rs");
        let mirror = fs::read_to_string(&mirror_path).unwrap();
        let mut lines: Vec<&str> = mirror.lines().collect();
        let at = insert_pos % (lines.len() + 1);
        let inserted = format!(
            "let zzz_marker = \"{}\";",
            word_picks
                .iter()
                .map(|pick| INSERT_WORDS[*pick])
                .collect::<Vec<_>>()
                .join(" ")
        );
        lines.insert(at, &inserted);
        let edited = lines.join("\n") + "\n";

        write_sanitized_content(repo.path(), Path::new("src/gen.rs"), &edited).unwrap();
        prop_assert!(verify_workspace(repo.path()).is_ok());

        // Forward: the mirror is exactly what the agent asked for.
        let mirror_after = fs::read_to_string(&mirror_path).unwrap();
        prop_assert_eq!(&mirror_after, &edited);

        // Backward: reverse projection reproduces the real file byte-for-byte.
        let real_after = fs::read_to_string(repo.path().join("src/gen.rs")).unwrap();
        let map_raw =
            fs::read_to_string(repo.path().join(".code-sanity/maps/src/gen.rs.map.json")).unwrap();
        let span_map: SpanMap = serde_json::from_str(&map_raw).unwrap();
        prop_assert_eq!(reverse_project(&span_map, &mirror_after), real_after);
    }
}
