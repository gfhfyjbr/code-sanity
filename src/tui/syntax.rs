use crate::semantic::LanguageId;
use std::path::Path;
use std::sync::OnceLock;
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntaxKind {
    Plain,
    Comment,
    Keyword,
    String,
    Number,
    Constant,
    Type,
    Function,
    Property,
    Variable,
    Operator,
    Punctuation,
    Tag,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxSpan {
    pub start: usize,
    pub end: usize,
    pub kind: SyntaxKind,
}

#[derive(Debug, Clone, Copy)]
struct AssignedKind {
    kind: SyntaxKind,
    priority: u8,
}

impl Default for AssignedKind {
    fn default() -> Self {
        Self {
            kind: SyntaxKind::Plain,
            priority: 0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Profile {
    Rust,
    Cpp,
    ObjectiveC,
    JavaScript,
    TypeScript,
    Tsx,
    Python,
    Go,
}

static RUST_QUERY: OnceLock<Option<Query>> = OnceLock::new();
static CPP_QUERY: OnceLock<Option<Query>> = OnceLock::new();
static OBJECTIVE_C_QUERY: OnceLock<Option<Query>> = OnceLock::new();
static JAVASCRIPT_QUERY: OnceLock<Option<Query>> = OnceLock::new();
static TYPESCRIPT_QUERY: OnceLock<Option<Query>> = OnceLock::new();
static TSX_QUERY: OnceLock<Option<Query>> = OnceLock::new();
static PYTHON_QUERY: OnceLock<Option<Query>> = OnceLock::new();
static GO_QUERY: OnceLock<Option<Query>> = OnceLock::new();

pub fn highlight_lines(path: &Path, lines: &[&str]) -> Vec<Vec<SyntaxSpan>> {
    if lines.is_empty() {
        return Vec::new();
    }

    let source = lines.join("\n");
    let mut assignments = vec![AssignedKind::default(); source.len()];
    for profile in profiles_for(path) {
        apply_profile(*profile, &source, &mut assignments);
    }

    let mut offset = 0;
    lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let end = offset + line.len();
            let spans = spans_for_line(line, &assignments[offset..end]);
            offset = end + usize::from(index + 1 < lines.len());
            spans
        })
        .collect()
}

fn profiles_for(path: &Path) -> &'static [Profile] {
    match LanguageId::detect(path) {
        LanguageId::Rust => &[Profile::Rust],
        LanguageId::Cpp => &[Profile::Cpp],
        LanguageId::ObjectiveC => &[Profile::ObjectiveC],
        LanguageId::ObjectiveCpp => &[Profile::Cpp, Profile::ObjectiveC],
        LanguageId::JavaScript => &[Profile::JavaScript],
        LanguageId::TypeScript
            if path.extension().and_then(|extension| extension.to_str()) == Some("tsx") =>
        {
            &[Profile::Tsx]
        }
        LanguageId::TypeScript => &[Profile::TypeScript],
        LanguageId::Python => &[Profile::Python],
        LanguageId::Go => &[Profile::Go],
        LanguageId::Unknown => &[],
    }
}

fn apply_profile(profile: Profile, source: &str, assignments: &mut [AssignedKind]) {
    let language = profile.language();
    let Some(query) = profile.query(&language) else {
        return;
    };
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return;
    }
    let Some(tree) = parser.parse(source, None) else {
        return;
    };

    let mut cursor = QueryCursor::new();
    let mut captures = cursor.captures(query, tree.root_node(), source.as_bytes());
    while let Some((query_match, capture_index)) = captures.next() {
        let capture = &query_match.captures[*capture_index];
        let Some(assigned) = capture_kind(query.capture_names()[capture.index as usize]) else {
            continue;
        };
        let range = capture.node.byte_range();
        let Some(target) = assignments.get_mut(range) else {
            continue;
        };
        for current in target {
            if assigned.priority >= current.priority {
                *current = assigned;
            }
        }
    }
}

fn spans_for_line(line: &str, assignments: &[AssignedKind]) -> Vec<SyntaxSpan> {
    if line.is_empty() {
        return Vec::new();
    }

    let mut spans = Vec::new();
    let mut start = 0;
    let mut current = assignments[0].kind;
    for (index, _) in line.char_indices().skip(1) {
        let kind = assignments[index].kind;
        if kind != current {
            spans.push(SyntaxSpan {
                start,
                end: index,
                kind: current,
            });
            start = index;
            current = kind;
        }
    }
    spans.push(SyntaxSpan {
        start,
        end: line.len(),
        kind: current,
    });
    spans
}

fn capture_kind(name: &str) -> Option<AssignedKind> {
    let root = name.split('.').next().unwrap_or(name);
    let (kind, priority) = match root {
        "comment" => (SyntaxKind::Comment, 100),
        "escape" => (SyntaxKind::String, 95),
        "string" | "embedded" | "text" => (SyntaxKind::String, 90),
        "number" => (SyntaxKind::Number, 80),
        "constant" | "label" => (SyntaxKind::Constant, 80),
        "keyword" | "exception" | "include" | "preproc" | "storageclass" => {
            (SyntaxKind::Keyword, 70)
        }
        "type" | "namespace" => (SyntaxKind::Type, 60),
        "function" | "method" | "constructor" | "selector" => (SyntaxKind::Function, 60),
        "property" | "attribute" => (SyntaxKind::Property, 55),
        "tag" => (SyntaxKind::Tag, 55),
        "variable" | "parameter" => (SyntaxKind::Variable, 40),
        "operator" => (SyntaxKind::Operator, 30),
        "punctuation" => (SyntaxKind::Punctuation, 20),
        _ => return None,
    };
    Some(AssignedKind { kind, priority })
}

impl Profile {
    fn language(self) -> Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Self::ObjectiveC => tree_sitter_objc::LANGUAGE.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
        }
    }

    fn query(self, language: &Language) -> Option<&'static Query> {
        match self {
            Self::Rust => RUST_QUERY
                .get_or_init(|| Query::new(language, tree_sitter_rust::HIGHLIGHTS_QUERY).ok())
                .as_ref(),
            Self::Cpp => CPP_QUERY
                .get_or_init(|| {
                    Query::new(
                        language,
                        &[
                            tree_sitter_c::HIGHLIGHT_QUERY,
                            tree_sitter_cpp::HIGHLIGHT_QUERY,
                        ]
                        .join("\n"),
                    )
                    .ok()
                })
                .as_ref(),
            Self::ObjectiveC => OBJECTIVE_C_QUERY
                .get_or_init(|| {
                    Query::new(
                        language,
                        &[
                            tree_sitter_c::HIGHLIGHT_QUERY,
                            tree_sitter_objc::HIGHLIGHTS_QUERY,
                        ]
                        .join("\n"),
                    )
                    .ok()
                })
                .as_ref(),
            Self::JavaScript => JAVASCRIPT_QUERY
                .get_or_init(|| {
                    Query::new(
                        language,
                        &[
                            tree_sitter_javascript::HIGHLIGHT_QUERY,
                            tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                        ]
                        .join("\n"),
                    )
                    .ok()
                })
                .as_ref(),
            Self::TypeScript => TYPESCRIPT_QUERY
                .get_or_init(|| {
                    Query::new(
                        language,
                        &[
                            tree_sitter_javascript::HIGHLIGHT_QUERY,
                            tree_sitter_typescript::HIGHLIGHTS_QUERY,
                        ]
                        .join("\n"),
                    )
                    .ok()
                })
                .as_ref(),
            Self::Tsx => TSX_QUERY
                .get_or_init(|| {
                    Query::new(
                        language,
                        &[
                            tree_sitter_javascript::HIGHLIGHT_QUERY,
                            tree_sitter_typescript::HIGHLIGHTS_QUERY,
                            tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                        ]
                        .join("\n"),
                    )
                    .ok()
                })
                .as_ref(),
            Self::Python => PYTHON_QUERY
                .get_or_init(|| Query::new(language, tree_sitter_python::HIGHLIGHTS_QUERY).ok())
                .as_ref(),
            Self::Go => GO_QUERY
                .get_or_init(|| Query::new(language, tree_sitter_go::HIGHLIGHTS_QUERY).ok())
                .as_ref(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_queries_compile_for_every_profile() {
        for profile in [
            Profile::Rust,
            Profile::Cpp,
            Profile::ObjectiveC,
            Profile::JavaScript,
            Profile::TypeScript,
            Profile::Tsx,
            Profile::Python,
            Profile::Go,
        ] {
            let language = profile.language();
            assert!(
                profile.query(&language).is_some(),
                "highlight query failed for {profile:?}"
            );
        }
    }

    #[test]
    fn highlighting_depends_on_the_detected_language() {
        let code = r#"fn main() { let message = "hello"; } // note"#;
        let rust = highlight_lines(Path::new("main.rs"), &[code]);
        let plain = highlight_lines(Path::new("notes.txt"), &[code]);

        let rust_kinds = rust[0].iter().map(|span| span.kind).collect::<Vec<_>>();
        assert!(rust_kinds.contains(&SyntaxKind::Keyword));
        assert!(rust_kinds.contains(&SyntaxKind::Function));
        assert!(rust_kinds.contains(&SyntaxKind::String));
        assert!(rust_kinds.contains(&SyntaxKind::Comment));
        assert_eq!(
            plain,
            vec![vec![SyntaxSpan {
                start: 0,
                end: code.len(),
                kind: SyntaxKind::Plain,
            }]]
        );
    }

    #[test]
    fn every_supported_extension_emits_language_specific_tokens() {
        let cases = [
            ("main.rs", "fn main() {}", "fn", SyntaxKind::Keyword),
            (
                "main.c",
                "int main(void) { return 0; }",
                "int",
                SyntaxKind::Type,
            ),
            ("main.cpp", "class Widget {};", "class", SyntaxKind::Keyword),
            (
                "main.m",
                "@interface Device @end",
                "@interface",
                SyntaxKind::Keyword,
            ),
            (
                "main.mm",
                "@implementation Device @end",
                "@implementation",
                SyntaxKind::Keyword,
            ),
            ("main.js", "const value = 1;", "const", SyntaxKind::Keyword),
            (
                "main.ts",
                "interface User {}",
                "interface",
                SyntaxKind::Keyword,
            ),
            ("main.tsx", "const view = <div />;", "div", SyntaxKind::Tag),
            ("main.py", "def main(): pass", "def", SyntaxKind::Keyword),
            ("main.go", "func main() {}", "func", SyntaxKind::Keyword),
        ];

        for (path, code, token, expected) in cases {
            let highlighted = highlight_lines(Path::new(path), &[code]);
            let start = code.find(token).unwrap();
            let kind = highlighted[0]
                .iter()
                .find(|span| start >= span.start && start < span.end)
                .map(|span| span.kind);
            assert_eq!(kind, Some(expected), "unexpected highlight for {path}");
        }
    }

    #[test]
    fn objective_cpp_combines_cpp_and_objective_c_highlights() {
        let lines = [
            "@implementation Device",
            r#"- (void)run { std::string value = "ready"; }"#,
            "@end",
        ];
        let highlighted = highlight_lines(Path::new("device.mm"), &lines);
        let kinds = highlighted
            .iter()
            .flatten()
            .map(|span| span.kind)
            .collect::<Vec<_>>();

        assert!(kinds.contains(&SyntaxKind::Keyword));
        assert!(kinds.contains(&SyntaxKind::Type));
        assert!(kinds.contains(&SyntaxKind::String));
    }

    #[test]
    fn utf8_fallback_spans_keep_valid_boundaries_and_empty_lines() {
        let lines = ["ключ = значение", "", "готово"];
        let highlighted = highlight_lines(Path::new("notes.txt"), &lines);

        assert_eq!(highlighted.len(), lines.len());
        assert!(highlighted[1].is_empty());
        for (line, spans) in lines.iter().zip(highlighted) {
            for span in spans {
                assert!(line.is_char_boundary(span.start));
                assert!(line.is_char_boundary(span.end));
                assert_eq!(span.kind, SyntaxKind::Plain);
            }
        }
    }
}
