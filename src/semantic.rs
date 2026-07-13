//! Syntax and semantic identity layer used by the v2 projection and MCP tools.
//!
//! Tree-sitter supplies lossless, byte-accurate structure. A parser result is
//! deliberately conservative: an occurrence is bound locally only when its
//! declaration is unambiguous. Language-server enrichment may add stronger
//! bindings later; missing semantics never fall back to a text rename.

use crate::map::sha256_hex;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, Stdio};
use tree_sitter::{Language, Node, Parser, Tree};

#[path = "semantic_resolver.rs"]
mod resolver;

/// Bump whenever parser-independent binding rules change in a way that
/// requires unchanged documents to be re-resolved. The value is persisted in
/// each document's capabilities JSON so old workspaces upgrade lazily on the
/// next ordinary index run without a schema migration.
pub const SEMANTIC_RESOLVER_VERSION: u32 = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LanguageId {
    Rust,
    Cpp,
    ObjectiveC,
    ObjectiveCpp,
    JavaScript,
    TypeScript,
    Python,
    Go,
    Unknown,
}

impl LanguageId {
    pub fn detect(path: &Path) -> Self {
        match path.extension().and_then(|value| value.to_str()) {
            Some("rs") => Self::Rust,
            Some("c" | "cc" | "cpp" | "cxx" | "h" | "hh" | "hpp" | "hxx") => Self::Cpp,
            Some("m") => Self::ObjectiveC,
            Some("mm") => Self::ObjectiveCpp,
            Some("js" | "jsx" | "mjs" | "cjs") => Self::JavaScript,
            Some("ts" | "tsx" | "mts" | "cts") => Self::TypeScript,
            Some("py") => Self::Python,
            Some("go") => Self::Go,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendCapabilities {
    #[serde(default)]
    pub resolver_version: u32,
    pub parse: bool,
    pub symbols: bool,
    pub references: bool,
    pub rename: bool,
    pub edit: bool,
    pub verify: bool,
    pub semantic_provider: Option<String>,
    pub read_only_reason: Option<String>,
}

impl BackendCapabilities {
    fn syntax_only(provider: Option<String>) -> Self {
        let has_semantic_provider = provider.is_some();
        Self {
            resolver_version: SEMANTIC_RESOLVER_VERSION,
            parse: true,
            symbols: true,
            references: has_semantic_provider,
            rename: has_semantic_provider,
            edit: true,
            verify: true,
            semantic_provider: provider,
            read_only_reason: None,
        }
    }

    fn read_only(reason: impl Into<String>) -> Self {
        Self {
            resolver_version: SEMANTIC_RESOLVER_VERSION,
            parse: false,
            symbols: false,
            references: false,
            rename: false,
            edit: false,
            verify: false,
            semantic_provider: None,
            read_only_reason: Some(reason.into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceOrigin {
    Owned,
    Generated,
    Vendor,
    Dependency,
}

impl SourceOrigin {
    pub fn for_path(path: &Path) -> Self {
        let normalized = path
            .to_string_lossy()
            .replace('\\', "/")
            .to_ascii_lowercase();
        let components = normalized.split('/').collect::<BTreeSet<_>>();
        if components.contains("vendor")
            || components.contains("vendors")
            || components.contains("third_party")
            || components.contains("third-party")
        {
            Self::Vendor
        } else if components.contains("target")
            || components.contains("node_modules")
            || components.contains("deps")
            || components.contains("_deps")
        {
            Self::Dependency
        } else if components.contains("generated")
            || components.contains("gen")
            || normalized.ends_with(".generated.rs")
            || normalized.ends_with(".g.rs")
        {
            Self::Generated
        } else {
            Self::Owned
        }
    }

    pub fn is_owned(self) -> bool {
        self == Self::Owned
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OccurrenceRole {
    Declaration,
    Reference,
    Unresolved,
    /// No repository-owned declaration is visible for this syntax context.
    /// External/unowned spellings remain unprojected but do not make an
    /// unrelated same-name owned symbol ambiguous.
    External,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextRange {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
}

impl TextRange {
    fn from_node(node: Node<'_>) -> Self {
        let start = node.start_position();
        let end = node.end_position();
        Self {
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            start_line: start.row + 1,
            start_column: start.column + 1,
            end_line: end.row + 1,
            end_column: end.column + 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticNode {
    pub node_id: String,
    pub parent_node_id: Option<String>,
    pub kind: String,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticSymbol {
    pub symbol_id: String,
    pub node_id: String,
    pub name: String,
    pub kind: String,
    pub qualified_name: String,
    pub scope_node_id: Option<String>,
    pub range: TextRange,
    pub origin: SourceOrigin,
    pub locally_bound: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticOccurrence {
    pub occurrence_id: String,
    pub node_id: String,
    pub symbol_id: Option<String>,
    pub name: String,
    pub role: OccurrenceRole,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedDocument {
    pub rel_path: String,
    pub language: LanguageId,
    pub content_hash: String,
    pub origin: SourceOrigin,
    pub capabilities: BackendCapabilities,
    pub parse_errors: usize,
    pub nodes: Vec<SemanticNode>,
    pub symbols: Vec<SemanticSymbol>,
    pub occurrences: Vec<SemanticOccurrence>,
}

pub trait LanguageBackend: Send + Sync {
    fn language(&self) -> LanguageId;
    fn capabilities(&self) -> BackendCapabilities;
    fn parse(&self, rel_path: &Path, source: &str) -> Result<ParsedDocument>;
}

pub fn capabilities_for_path(path: &Path) -> BackendCapabilities {
    backend_for_path(path).capabilities()
}

pub fn parse_document(rel_path: &Path, source: &str) -> Result<ParsedDocument> {
    backend_for_path(rel_path).parse(rel_path, source)
}

fn backend_for_path(path: &Path) -> Box<dyn LanguageBackend> {
    match LanguageId::detect(path) {
        LanguageId::Rust => Box::new(TreeSitterBackend::rust()),
        LanguageId::Cpp => Box::new(TreeSitterBackend::cpp(LanguageId::Cpp)),
        LanguageId::ObjectiveC => Box::new(TreeSitterBackend::objc(LanguageId::ObjectiveC)),
        LanguageId::ObjectiveCpp => Box::new(ObjectiveCppBackend::new()),
        LanguageId::JavaScript => Box::new(TreeSitterBackend::javascript()),
        LanguageId::TypeScript => Box::new(TreeSitterBackend::typescript(
            path.extension().and_then(|extension| extension.to_str()) == Some("tsx"),
        )),
        LanguageId::Python => Box::new(TreeSitterBackend::python()),
        LanguageId::Go => Box::new(TreeSitterBackend::go()),
        language => Box::new(ReadOnlyBackend { language }),
    }
}

struct ReadOnlyBackend {
    language: LanguageId,
}

impl LanguageBackend for ReadOnlyBackend {
    fn language(&self) -> LanguageId {
        self.language
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::read_only("no syntax/semantic backend is installed for this language")
    }

    fn parse(&self, rel_path: &Path, source: &str) -> Result<ParsedDocument> {
        Ok(ParsedDocument {
            rel_path: normalize_path(rel_path),
            language: self.language,
            content_hash: sha256_hex(source.as_bytes()),
            origin: SourceOrigin::for_path(rel_path),
            capabilities: self.capabilities(),
            parse_errors: 0,
            nodes: Vec::new(),
            symbols: Vec::new(),
            occurrences: Vec::new(),
        })
    }
}

struct TreeSitterBackend {
    language: LanguageId,
    grammar: Language,
    semantic_provider: Option<String>,
}

/// Objective-C++ is a true language union. Neither the C++ nor Objective-C
/// tree-sitter grammar accepts the complete surface, so `.mm` files are
/// parsed by both and merged at identifier/node granularity. C++ remains the
/// stable primary node namespace; Objective-C contributes namespaced nodes
/// and native selector/property/class semantics.
struct ObjectiveCppBackend {
    semantic_provider: Option<String>,
}

impl ObjectiveCppBackend {
    fn new() -> Self {
        Self {
            semantic_provider: command_version("clangd"),
        }
    }
}

impl LanguageBackend for ObjectiveCppBackend {
    fn language(&self) -> LanguageId {
        LanguageId::ObjectiveCpp
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::syntax_only(self.semantic_provider.clone())
    }

    fn parse(&self, rel_path: &Path, source: &str) -> Result<ParsedDocument> {
        let cpp_tree = parse_tree(
            tree_sitter_cpp::LANGUAGE.into(),
            LanguageId::ObjectiveCpp,
            source,
        )?;
        let objc_tree = parse_tree(
            tree_sitter_objc::LANGUAGE.into(),
            LanguageId::ObjectiveCpp,
            source,
        )?;
        build_objective_cpp_document(rel_path, source, self.capabilities(), &cpp_tree, &objc_tree)
    }
}

impl TreeSitterBackend {
    fn rust() -> Self {
        Self {
            language: LanguageId::Rust,
            grammar: tree_sitter_rust::LANGUAGE.into(),
            semantic_provider: command_version("rust-analyzer"),
        }
    }

    fn cpp(language: LanguageId) -> Self {
        Self {
            language,
            grammar: tree_sitter_cpp::LANGUAGE.into(),
            semantic_provider: command_version("clangd"),
        }
    }

    fn objc(language: LanguageId) -> Self {
        Self {
            language,
            grammar: tree_sitter_objc::LANGUAGE.into(),
            semantic_provider: command_version("clangd"),
        }
    }

    fn javascript() -> Self {
        Self {
            language: LanguageId::JavaScript,
            grammar: tree_sitter_javascript::LANGUAGE.into(),
            semantic_provider: None,
        }
    }

    fn typescript(tsx: bool) -> Self {
        Self {
            language: LanguageId::TypeScript,
            grammar: if tsx {
                tree_sitter_typescript::LANGUAGE_TSX.into()
            } else {
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
            },
            semantic_provider: None,
        }
    }

    fn python() -> Self {
        Self {
            language: LanguageId::Python,
            grammar: tree_sitter_python::LANGUAGE.into(),
            semantic_provider: None,
        }
    }

    fn go() -> Self {
        Self {
            language: LanguageId::Go,
            grammar: tree_sitter_go::LANGUAGE.into(),
            semantic_provider: None,
        }
    }
}

impl LanguageBackend for TreeSitterBackend {
    fn language(&self) -> LanguageId {
        self.language
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::syntax_only(self.semantic_provider.clone())
    }

    fn parse(&self, rel_path: &Path, source: &str) -> Result<ParsedDocument> {
        let tree = parse_tree(self.grammar.clone(), self.language, source)?;
        build_document(rel_path, source, self.language, self.capabilities(), &tree)
    }
}

fn parse_tree(grammar: Language, language: LanguageId, source: &str) -> Result<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&grammar)
        .with_context(|| format!("load {language:?} tree-sitter grammar"))?;
    parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter parser returned no tree"))
}

fn build_document(
    rel_path: &Path,
    source: &str,
    language: LanguageId,
    capabilities: BackendCapabilities,
    tree: &Tree,
) -> Result<ParsedDocument> {
    let rel = normalize_path(rel_path);
    let origin = SourceOrigin::for_path(rel_path);
    let analysis = resolver::analyze(&rel, source, language, tree)?;

    Ok(ParsedDocument {
        rel_path: rel,
        language,
        content_hash: sha256_hex(source.as_bytes()),
        origin,
        capabilities,
        parse_errors: count_error_nodes(tree.root_node()),
        nodes: analysis.nodes,
        symbols: analysis.symbols,
        occurrences: analysis.occurrences,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum HybridGrammar {
    Cpp,
    ObjectiveC,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ObjectiveCMethodBody {
    method_start: usize,
    body_start: usize,
    body_end: usize,
}

fn objective_cpp_cpp_projection(source: &str, objc_tree: &Tree) -> String {
    let mut projected = source.as_bytes().to_vec();
    let root = objc_tree.root_node();
    let mut method_bodies = Vec::<ObjectiveCMethodBody>::new();
    collect_objective_cpp_projection_regions(root, &mut projected, &mut method_bodies);
    // Error recovery in the Objective-C grammar can lose a whole method when
    // an interface contains C++-qualified property types. Recover method
    // bodies lexically as well; the scanner is string/comment aware and only
    // accepts a leading Objective-C `- (` / `+ (` method form.
    method_bodies.extend(lexical_objective_c_method_bodies(source));
    method_bodies.sort_unstable_by_key(|body| (body.body_start, body.body_end));
    method_bodies.dedup_by_key(|body| (body.body_start, body.body_end));
    for body in &method_bodies {
        projected[body.body_start..body.body_end]
            .copy_from_slice(&source.as_bytes()[body.body_start..body.body_end]);
        inject_same_line_token(
            &mut projected,
            body.method_start,
            body.body_start,
            b"void f() ",
        );
    }
    mask_objective_c_messages(root, &mut projected);
    normalize_objective_c_fast_enumeration(root, source, &mut projected);
    normalize_objective_c_pointer_declarations(root, source, &mut projected);
    mask_objective_c_statement_keywords(source, &mut projected);
    // Objective-C string literals differ from C++ only by the leading `@`.
    // Removing that byte (without shifting anything) lets body-local C++
    // declarations around the literal remain parseable.
    for index in 0..projected.len().saturating_sub(1) {
        if projected[index] == b'@' && projected[index + 1] == b'"' {
            projected[index] = b' ';
        }
    }
    String::from_utf8(projected).expect("projection only substitutes ASCII bytes")
}

fn collect_objective_cpp_projection_regions<'a>(
    node: Node<'a>,
    projected: &mut [u8],
    method_bodies: &mut Vec<ObjectiveCMethodBody>,
) {
    if matches!(
        node.kind(),
        "class_interface"
            | "category_interface"
            | "protocol_declaration"
            | "class_implementation"
            | "category_implementation"
    ) {
        mask_preserving_newlines(projected, node.start_byte(), node.end_byte());
        if matches!(
            node.kind(),
            "class_implementation" | "category_implementation"
        ) {
            collect_method_bodies(node, method_bodies);
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_objective_cpp_projection_regions(child, projected, method_bodies);
    }
}

fn collect_method_bodies(node: Node<'_>, bodies: &mut Vec<ObjectiveCMethodBody>) {
    if node.kind() == "method_definition" {
        let mut cursor = node.walk();
        if let Some(body) = node
            .named_children(&mut cursor)
            .find(|child| child.kind() == "compound_statement")
        {
            bodies.push(ObjectiveCMethodBody {
                method_start: node.start_byte(),
                body_start: body.start_byte(),
                body_end: body.end_byte(),
            });
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_method_bodies(child, bodies);
    }
}

fn lexical_objective_c_method_bodies(source: &str) -> Vec<ObjectiveCMethodBody> {
    let bytes = source.as_bytes();
    let mut bodies = Vec::new();
    let mut line_start = 0usize;
    while line_start < bytes.len() {
        let line_end = bytes[line_start..]
            .iter()
            .position(|byte| *byte == b'\n' || *byte == b'\r')
            .map_or(bytes.len(), |offset| line_start + offset);
        let mut marker = line_start;
        while marker < line_end && matches!(bytes[marker], b' ' | b'\t') {
            marker += 1;
        }
        let is_method = matches!(bytes.get(marker), Some(b'-' | b'+')) && {
            let mut after = marker + 1;
            while after < line_end && matches!(bytes[after], b' ' | b'\t') {
                after += 1;
            }
            bytes.get(after) == Some(&b'(')
        };
        if is_method {
            if let Some(body_start) = find_code_opening_brace(bytes, marker + 1) {
                if let Some(body_end) = find_matching_code_brace(bytes, body_start) {
                    bodies.push(ObjectiveCMethodBody {
                        method_start: marker,
                        body_start,
                        body_end,
                    });
                    line_start = body_end;
                    continue;
                }
            }
        }
        line_start = if line_end < bytes.len() {
            line_end + 1
        } else {
            bytes.len()
        };
    }
    bodies
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LexicalState {
    Code,
    String,
    Character,
    LineComment,
    BlockComment,
}

fn find_code_opening_brace(bytes: &[u8], start: usize) -> Option<usize> {
    let mut state = LexicalState::Code;
    let mut escaped = false;
    let mut index = start;
    while index < bytes.len() {
        let byte = bytes[index];
        match state {
            LexicalState::Code => match (byte, bytes.get(index + 1).copied()) {
                (b'/', Some(b'/')) => {
                    state = LexicalState::LineComment;
                    index += 1;
                }
                (b'/', Some(b'*')) => {
                    state = LexicalState::BlockComment;
                    index += 1;
                }
                (b'"', _) => state = LexicalState::String,
                (b'\'', _) => state = LexicalState::Character,
                (b';', _) => return None,
                (b'{', _) => return Some(index),
                _ => {}
            },
            LexicalState::String => {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    state = LexicalState::Code;
                }
            }
            LexicalState::Character => {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'\'' {
                    state = LexicalState::Code;
                }
            }
            LexicalState::LineComment => {
                if byte == b'\n' || byte == b'\r' {
                    state = LexicalState::Code;
                }
            }
            LexicalState::BlockComment => {
                if byte == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    state = LexicalState::Code;
                    index += 1;
                }
            }
        }
        index += 1;
    }
    None
}

fn find_matching_code_brace(bytes: &[u8], opening: usize) -> Option<usize> {
    let mut state = LexicalState::Code;
    let mut escaped = false;
    let mut depth = 0usize;
    let mut index = opening;
    while index < bytes.len() {
        let byte = bytes[index];
        match state {
            LexicalState::Code => match (byte, bytes.get(index + 1).copied()) {
                (b'/', Some(b'/')) => {
                    state = LexicalState::LineComment;
                    index += 1;
                }
                (b'/', Some(b'*')) => {
                    state = LexicalState::BlockComment;
                    index += 1;
                }
                (b'"', _) => state = LexicalState::String,
                (b'\'', _) => state = LexicalState::Character,
                (b'{', _) => depth += 1,
                (b'}', _) => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        return Some(index + 1);
                    }
                }
                _ => {}
            },
            LexicalState::String => {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    state = LexicalState::Code;
                }
            }
            LexicalState::Character => {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'\'' {
                    state = LexicalState::Code;
                }
            }
            LexicalState::LineComment => {
                if byte == b'\n' || byte == b'\r' {
                    state = LexicalState::Code;
                }
            }
            LexicalState::BlockComment => {
                if byte == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    state = LexicalState::Code;
                    index += 1;
                }
            }
        }
        index += 1;
    }
    None
}

fn mask_objective_c_statement_keywords(source: &str, projected: &mut [u8]) {
    let bytes = source.as_bytes();
    let mut index = 0usize;
    let mut state = LexicalState::Code;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        match state {
            LexicalState::Code => match (byte, bytes.get(index + 1).copied()) {
                (b'/', Some(b'/')) => {
                    state = LexicalState::LineComment;
                    index += 1;
                }
                (b'/', Some(b'*')) => {
                    state = LexicalState::BlockComment;
                    index += 1;
                }
                (b'"', _) => state = LexicalState::String,
                (b'\'', _) => state = LexicalState::Character,
                (b'@', _) => {
                    for keyword in ["autoreleasepool", "try", "catch", "finally", "synchronized"] {
                        let end = index + 1 + keyword.len();
                        if bytes.get(index + 1..end) == Some(keyword.as_bytes())
                            && bytes
                                .get(end)
                                .is_none_or(|next| !(*next == b'_' || next.is_ascii_alphanumeric()))
                        {
                            mask_preserving_newlines(projected, index, end);
                            index = end.saturating_sub(1);
                            break;
                        }
                    }
                    if bytes.get(index + 1..index + 6) == Some(b"throw") {
                        projected[index] = b' ';
                    }
                }
                _ => {}
            },
            LexicalState::String => {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    state = LexicalState::Code;
                }
            }
            LexicalState::Character => {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'\'' {
                    state = LexicalState::Code;
                }
            }
            LexicalState::LineComment => {
                if byte == b'\n' || byte == b'\r' {
                    state = LexicalState::Code;
                }
            }
            LexicalState::BlockComment => {
                if byte == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    state = LexicalState::Code;
                    index += 1;
                }
            }
        }
        index += 1;
    }
}

fn mask_objective_c_messages(node: Node<'_>, projected: &mut [u8]) {
    if matches!(
        node.kind(),
        "message_expression"
            | "block_literal"
            | "array_literal"
            | "dictionary_literal"
            | "boxed_expression"
    ) {
        mask_preserving_newlines(projected, node.start_byte(), node.end_byte());
        if let Some(byte) = projected[node.start_byte()..node.end_byte()]
            .iter_mut()
            .find(|byte| **byte != b'\n' && **byte != b'\r')
        {
            *byte = b'0';
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        mask_objective_c_messages(child, projected);
    }
}

fn normalize_objective_c_fast_enumeration(node: Node<'_>, source: &str, projected: &mut [u8]) {
    if node.kind() == "for_statement" {
        let mut cursor = node.walk();
        let children = node.named_children(&mut cursor).collect::<Vec<_>>();
        if let [kind, declarator, collection, _body] = children.as_slice() {
            let separator = source
                .get(declarator.end_byte()..collection.start_byte())
                .unwrap_or_default();
            if let Some(relative) = separator.find("in") {
                let start = declarator.end_byte() + relative;
                let token_boundary = separator
                    .get(relative + 2..)
                    .and_then(|tail| tail.chars().next())
                    .is_none_or(char::is_whitespace);
                if token_boundary && kind.end_byte().saturating_sub(kind.start_byte()) >= 4 {
                    mask_preserving_newlines(projected, kind.start_byte(), kind.end_byte());
                    projected[kind.start_byte()..kind.start_byte() + 4].copy_from_slice(b"auto");
                    projected[start] = b':';
                    projected[start + 1] = b' ';
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        normalize_objective_c_fast_enumeration(child, source, projected);
    }
}

fn normalize_objective_c_pointer_declarations(node: Node<'_>, source: &str, projected: &mut [u8]) {
    if node.kind() == "declaration" {
        if let Some(kind) = node.child_by_field_name("type") {
            let declarator_tail = source
                .get(kind.end_byte()..node.end_byte())
                .unwrap_or_default();
            let unqualified_user_type = matches!(kind.kind(), "type_identifier" | "identifier")
                && !source
                    .get(kind.start_byte()..kind.end_byte())
                    .unwrap_or_default()
                    .contains("::");
            if unqualified_user_type
                && declarator_tail
                    .split(['=', ';'])
                    .next()
                    .is_some_and(|declarator| declarator.contains('*'))
                && kind.end_byte().saturating_sub(kind.start_byte()) >= 4
            {
                mask_preserving_newlines(projected, kind.start_byte(), kind.end_byte());
                projected[kind.start_byte()..kind.start_byte() + 4].copy_from_slice(b"auto");
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        normalize_objective_c_pointer_declarations(child, source, projected);
    }
}

fn mask_preserving_newlines(bytes: &mut [u8], start: usize, end: usize) {
    for byte in &mut bytes[start..end] {
        if *byte != b'\n' && *byte != b'\r' {
            *byte = b' ';
        }
    }
}

fn inject_same_line_token(bytes: &mut [u8], start: usize, end: usize, token: &[u8]) {
    let mut line_start = start;
    while line_start < end {
        let line_end = bytes[line_start..end]
            .iter()
            .position(|byte| *byte == b'\n' || *byte == b'\r')
            .map_or(end, |offset| line_start + offset);
        if line_end.saturating_sub(line_start) >= token.len() {
            bytes[line_start..line_start + token.len()].copy_from_slice(token);
            return;
        }
        line_start = line_end.saturating_add(1);
    }
}

fn merge_cpp_projection_analysis(
    rel: &str,
    source: &str,
    mut primary: resolver::SemanticAnalysis,
    secondary: resolver::SemanticAnalysis,
) -> resolver::SemanticAnalysis {
    let (nodes, _, secondary_node_ids) =
        merge_secondary_nodes(rel, &primary.nodes, &secondary.nodes, "cpp-body");
    primary.nodes = nodes;

    let mut declaration_range_to_id = primary
        .symbols
        .iter()
        .map(|symbol| {
            (
                (
                    symbol.range.start_byte,
                    symbol.range.end_byte,
                    symbol.name.clone(),
                    symbol.kind.clone(),
                ),
                symbol.symbol_id.clone(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut used_ids = primary
        .symbols
        .iter()
        .map(|symbol| symbol.symbol_id.clone())
        .collect::<BTreeSet<_>>();
    let mut symbol_remap = BTreeMap::<String, String>::new();
    for symbol in &secondary.symbols {
        if !range_is_original_identifier(source, &symbol.range, &symbol.name) {
            continue;
        }
        let range_key = (
            symbol.range.start_byte,
            symbol.range.end_byte,
            symbol.name.clone(),
            symbol.kind.clone(),
        );
        if let Some(existing) = declaration_range_to_id.get(&range_key) {
            symbol_remap.insert(symbol.symbol_id.clone(), existing.clone());
            continue;
        }
        let mut output = symbol.clone();
        if used_ids.contains(&output.symbol_id) {
            output.symbol_id = stable_id("sym-hybrid", &[rel, "cpp-body", &symbol.symbol_id]);
        }
        output.node_id = secondary_node_ids
            .get(&output.node_id)
            .cloned()
            .unwrap_or(output.node_id);
        output.scope_node_id = output
            .scope_node_id
            .as_ref()
            .and_then(|node_id| secondary_node_ids.get(node_id))
            .cloned();
        used_ids.insert(output.symbol_id.clone());
        declaration_range_to_id.insert(range_key, output.symbol_id.clone());
        symbol_remap.insert(symbol.symbol_id.clone(), output.symbol_id.clone());
        primary.symbols.push(output);
    }

    let mut selected = primary
        .occurrences
        .drain(..)
        .map(|occurrence| (occurrence_key(&occurrence), occurrence))
        .collect::<BTreeMap<_, _>>();
    for mut occurrence in secondary.occurrences {
        if !range_is_original_identifier(source, &occurrence.range, &occurrence.name) {
            continue;
        }
        occurrence.node_id = secondary_node_ids
            .get(&occurrence.node_id)
            .cloned()
            .unwrap_or(occurrence.node_id);
        occurrence.symbol_id = occurrence
            .symbol_id
            .as_ref()
            .and_then(|symbol_id| symbol_remap.get(symbol_id))
            .cloned();
        occurrence.occurrence_id = stable_id(
            "occ",
            &[
                rel,
                &occurrence.node_id,
                occurrence_role_name(occurrence.role),
            ],
        );
        let key = occurrence_key(&occurrence);
        let replace = selected.get(&key).is_none_or(|existing| {
            (occurrence.role == OccurrenceRole::Declaration
                && existing.role != OccurrenceRole::Declaration)
                || (occurrence.role == OccurrenceRole::Reference
                    && occurrence.symbol_id.is_some()
                    && existing.role != OccurrenceRole::Declaration)
        });
        if replace {
            selected.insert(key, occurrence);
        }
    }
    primary.occurrences = selected.into_values().collect();
    let declared_ids = primary
        .occurrences
        .iter()
        .filter(|occurrence| occurrence.role == OccurrenceRole::Declaration)
        .filter_map(|occurrence| occurrence.symbol_id.clone())
        .collect::<BTreeSet<_>>();
    primary
        .symbols
        .retain(|symbol| declared_ids.contains(&symbol.symbol_id));
    for occurrence in &mut primary.occurrences {
        if occurrence
            .symbol_id
            .as_ref()
            .is_some_and(|symbol_id| !declared_ids.contains(symbol_id))
        {
            occurrence.symbol_id = None;
            if occurrence.role == OccurrenceRole::Reference {
                occurrence.role = OccurrenceRole::Unresolved;
            }
        }
    }
    for symbol in &mut primary.symbols {
        symbol.locally_bound = primary.occurrences.iter().any(|occurrence| {
            occurrence.role == OccurrenceRole::Reference
                && occurrence.symbol_id.as_deref() == Some(symbol.symbol_id.as_str())
        });
    }
    primary
}

fn range_is_original_identifier(source: &str, range: &TextRange, name: &str) -> bool {
    source.get(range.start_byte..range.end_byte) == Some(name)
}

fn build_objective_cpp_document(
    rel_path: &Path,
    source: &str,
    capabilities: BackendCapabilities,
    cpp_tree: &Tree,
    objc_tree: &Tree,
) -> Result<ParsedDocument> {
    let rel = normalize_path(rel_path);
    let cpp = resolver::analyze(&rel, source, LanguageId::ObjectiveCpp, cpp_tree)?;
    let cpp_projection = objective_cpp_cpp_projection(source, objc_tree);
    let cpp_projection_tree = parse_tree(
        tree_sitter_cpp::LANGUAGE.into(),
        LanguageId::ObjectiveCpp,
        &cpp_projection,
    )?;
    let cpp_projection_analysis = resolver::analyze(
        &rel,
        &cpp_projection,
        LanguageId::ObjectiveCpp,
        &cpp_projection_tree,
    )?;
    let cpp = merge_cpp_projection_analysis(&rel, source, cpp, cpp_projection_analysis);
    let objc = resolver::analyze(&rel, source, LanguageId::ObjectiveCpp, objc_tree)?;
    let mut analysis = merge_objective_cpp_analysis(&rel, source, objc_tree, cpp, objc);
    normalize_objective_cpp_local_names(&mut analysis, objc_tree, source);
    Ok(ParsedDocument {
        rel_path: rel,
        language: LanguageId::ObjectiveCpp,
        content_hash: sha256_hex(source.as_bytes()),
        origin: SourceOrigin::for_path(rel_path),
        capabilities,
        // Each grammar rejects syntax native to the other. The smaller error
        // count is a stable, conservative edit-regression baseline; semantic
        // selection below is still local rather than winner-takes-all.
        parse_errors: count_error_nodes(cpp_tree.root_node())
            .min(count_error_nodes(objc_tree.root_node()))
            .min(count_error_nodes(cpp_projection_tree.root_node())),
        nodes: analysis.nodes,
        symbols: analysis.symbols,
        occurrences: analysis.occurrences,
    })
}

fn normalize_objective_cpp_local_names(
    analysis: &mut resolver::SemanticAnalysis,
    objc_tree: &Tree,
    source: &str,
) {
    for symbol in &mut analysis.symbols {
        if let Some((class_name, selector)) =
            enclosing_objective_c_method(objc_tree, source, &symbol.range)
        {
            symbol.qualified_name = format!("{class_name}::[{selector}]::{}", symbol.name);
        }
    }
}

fn enclosing_objective_c_method(
    tree: &Tree,
    source: &str,
    range: &TextRange,
) -> Option<(String, String)> {
    let mut node = tree
        .root_node()
        .descendant_for_byte_range(range.start_byte, range.end_byte)?;
    loop {
        if node.kind() == "method_definition" {
            let mut cursor = node.walk();
            let children = node.named_children(&mut cursor).collect::<Vec<_>>();
            let fragments = children
                .iter()
                .filter(|child| child.kind() == "identifier")
                .filter_map(|child| child.utf8_text(source.as_bytes()).ok())
                .collect::<Vec<_>>();
            let arity = children
                .iter()
                .filter(|child| child.kind() == "method_parameter")
                .count();
            let mut selector = fragments.join(":");
            if arity > 0 {
                selector.push(':');
            }
            let mut owner = node;
            let class_name = loop {
                owner = owner.parent()?;
                if matches!(
                    owner.kind(),
                    "class_implementation" | "category_implementation"
                ) {
                    let mut cursor = owner.walk();
                    break owner
                        .named_children(&mut cursor)
                        .find(|child| child.kind() == "identifier")?
                        .utf8_text(source.as_bytes())
                        .ok()?
                        .to_string();
                }
            };
            return Some((class_name, selector));
        }
        node = node.parent()?;
    }
}

fn merge_objective_cpp_analysis(
    rel: &str,
    source: &str,
    objc_tree: &Tree,
    cpp: resolver::SemanticAnalysis,
    objc: resolver::SemanticAnalysis,
) -> resolver::SemanticAnalysis {
    type OccurrenceKey = (usize, usize, String);
    type TaggedSymbol = (HybridGrammar, String);

    let objc_reliable_symbols = objc
        .occurrences
        .iter()
        .filter(|occurrence| {
            occurrence.role == OccurrenceRole::Declaration
                && objective_cpp_objc_range_is_reliable(objc_tree, source, &occurrence.range)
        })
        .filter_map(|occurrence| occurrence.symbol_id.clone())
        .collect::<BTreeSet<_>>();

    let mut selected = BTreeMap::<OccurrenceKey, (HybridGrammar, SemanticOccurrence)>::new();
    for occurrence in &cpp.occurrences {
        selected.insert(
            occurrence_key(occurrence),
            (HybridGrammar::Cpp, occurrence.clone()),
        );
    }
    for occurrence in &objc.occurrences {
        let key = occurrence_key(occurrence);
        let native_range = objective_c_native_range(objc_tree, source, &occurrence.range);
        let reliable_range = native_range
            || objective_cpp_objc_range_is_reliable(objc_tree, source, &occurrence.range);
        let reliable_binding = occurrence
            .symbol_id
            .as_ref()
            .is_some_and(|symbol_id| objc_reliable_symbols.contains(symbol_id));
        let replace = selected.get(&key).is_none_or(|(_, existing)| {
            let authoritative_cpp_external = existing.role == OccurrenceRole::External
                && is_cpp_qualified_name_component(source, &existing.range);
            (native_range
                && (occurrence.role == OccurrenceRole::Declaration
                    || existing.role != OccurrenceRole::Declaration)
                && (occurrence.role == OccurrenceRole::Declaration
                    || occurrence_resolution_rank(occurrence.role)
                        > occurrence_resolution_rank(existing.role)))
                || (reliable_range
                    && !authoritative_cpp_external
                    && occurrence_resolution_rank(occurrence.role)
                        > occurrence_resolution_rank(existing.role))
                || (reliable_binding
                    && !authoritative_cpp_external
                    && occurrence.role == OccurrenceRole::Reference
                    && matches!(
                        existing.role,
                        OccurrenceRole::External | OccurrenceRole::Unresolved
                    ))
        });
        if replace {
            selected.insert(key, (HybridGrammar::ObjectiveC, occurrence.clone()));
        }
    }

    let (mut nodes, cpp_node_ids, objc_node_ids) =
        merge_secondary_nodes(rel, &cpp.nodes, &objc.nodes, "objc");
    let cpp_symbols = cpp
        .symbols
        .iter()
        .map(|symbol| (symbol.symbol_id.clone(), symbol))
        .collect::<BTreeMap<_, _>>();
    let objc_symbols = objc
        .symbols
        .iter()
        .map(|symbol| (symbol.symbol_id.clone(), symbol))
        .collect::<BTreeMap<_, _>>();

    let mut symbol_remap = BTreeMap::<TaggedSymbol, String>::new();
    let mut declaration_range_remap = BTreeMap::<(usize, usize, String, String), String>::new();
    let mut declaration_range_loose = BTreeMap::<(usize, usize, String), Option<String>>::new();
    let mut used_output_ids = BTreeMap::<String, String>::new();
    let mut symbols = Vec::<SemanticSymbol>::new();
    for (grammar, occurrence) in selected.values() {
        if occurrence.role != OccurrenceRole::Declaration {
            continue;
        }
        let Some(source_symbol_id) = occurrence.symbol_id.as_ref() else {
            continue;
        };
        let tagged = (*grammar, source_symbol_id.clone());
        if symbol_remap.contains_key(&tagged) {
            continue;
        }
        let Some(source_symbol) =
            hybrid_source_symbol(*grammar, source_symbol_id, &cpp_symbols, &objc_symbols)
        else {
            continue;
        };
        let identity = format!(
            "{}\0{}\0{}\0{}",
            source_symbol.kind,
            source_symbol.qualified_name,
            occurrence.range.start_byte,
            occurrence.range.end_byte
        );

        let mut output_id = source_symbol.symbol_id.clone();
        if used_output_ids
            .get(&output_id)
            .is_some_and(|existing| existing != &identity)
        {
            output_id = stable_id(
                "sym-hybrid",
                &[
                    rel,
                    match grammar {
                        HybridGrammar::Cpp => "cpp",
                        HybridGrammar::ObjectiveC => "objc",
                    },
                    &source_symbol.symbol_id,
                ],
            );
        }
        let node_ids = hybrid_node_map(*grammar, &cpp_node_ids, &objc_node_ids);
        let mut output = (*source_symbol).clone();
        output.symbol_id = output_id.clone();
        output.node_id = node_ids
            .get(&occurrence.node_id)
            .cloned()
            .unwrap_or_else(|| occurrence.node_id.clone());
        output.scope_node_id = output
            .scope_node_id
            .as_ref()
            .and_then(|node_id| node_ids.get(node_id))
            .cloned();
        output.range = occurrence.range.clone();
        output.locally_bound = false;
        used_output_ids.insert(output_id.clone(), identity);
        declaration_range_remap.insert(
            (
                occurrence.range.start_byte,
                occurrence.range.end_byte,
                source_symbol.name.clone(),
                source_symbol.kind.clone(),
            ),
            output_id.clone(),
        );
        declaration_range_loose
            .entry((
                occurrence.range.start_byte,
                occurrence.range.end_byte,
                source_symbol.name.clone(),
            ))
            .and_modify(|existing| {
                if existing.as_deref() != Some(output_id.as_str()) {
                    *existing = None;
                }
            })
            .or_insert_with(|| Some(output_id.clone()));
        symbol_remap.insert(tagged, output_id);
        symbols.push(output);
    }

    let mut occurrences = Vec::<SemanticOccurrence>::with_capacity(selected.len());
    for (grammar, mut occurrence) in selected.into_values() {
        let node_ids = hybrid_node_map(grammar, &cpp_node_ids, &objc_node_ids);
        occurrence.node_id = node_ids
            .get(&occurrence.node_id)
            .cloned()
            .unwrap_or(occurrence.node_id);
        if let Some(source_symbol_id) = occurrence.symbol_id.take() {
            let tagged = (grammar, source_symbol_id.clone());
            occurrence.symbol_id = symbol_remap.get(&tagged).cloned().or_else(|| {
                hybrid_source_symbol(grammar, &source_symbol_id, &cpp_symbols, &objc_symbols)
                    .and_then(|symbol| {
                        declaration_range_remap
                            .get(&(
                                symbol.range.start_byte,
                                symbol.range.end_byte,
                                symbol.name.clone(),
                                symbol.kind.clone(),
                            ))
                            .cloned()
                            .or_else(|| {
                                declaration_range_loose
                                    .get(&(
                                        symbol.range.start_byte,
                                        symbol.range.end_byte,
                                        symbol.name.clone(),
                                    ))
                                    .cloned()
                                    .flatten()
                            })
                    })
            });
            if occurrence.symbol_id.is_none() && occurrence.role == OccurrenceRole::Reference {
                occurrence.role = if is_cpp_qualified_name_component(source, &occurrence.range) {
                    OccurrenceRole::External
                } else {
                    OccurrenceRole::Unresolved
                };
            }
        }
        if occurrence.role == OccurrenceRole::Unresolved
            && is_cpp_qualified_name_component(source, &occurrence.range)
        {
            occurrence.role = OccurrenceRole::External;
        }
        occurrence.occurrence_id = stable_id(
            "occ",
            &[
                rel,
                &occurrence.node_id,
                occurrence_role_name(occurrence.role),
            ],
        );
        occurrences.push(occurrence);
    }
    for symbol in &mut symbols {
        symbol.locally_bound = occurrences.iter().any(|occurrence| {
            occurrence.role == OccurrenceRole::Reference
                && occurrence.symbol_id.as_deref() == Some(symbol.symbol_id.as_str())
        });
    }
    nodes.sort_by_key(|node| {
        (
            node.range.start_byte,
            node.range.end_byte,
            node.kind.clone(),
        )
    });
    resolver::SemanticAnalysis {
        nodes,
        symbols,
        occurrences,
    }
}

fn is_cpp_qualified_name_component(source: &str, range: &TextRange) -> bool {
    source
        .get(..range.start_byte)
        .is_some_and(|prefix| prefix.trim_end().ends_with("::"))
}

fn occurrence_resolution_rank(role: OccurrenceRole) -> u8 {
    match role {
        OccurrenceRole::Declaration | OccurrenceRole::Reference => 2,
        OccurrenceRole::Unresolved => 1,
        OccurrenceRole::External => 0,
    }
}

fn objective_cpp_objc_range_is_reliable(tree: &Tree, source: &str, range: &TextRange) -> bool {
    if objective_c_native_range(tree, source, range) {
        return true;
    }
    let Some(mut node) = tree
        .root_node()
        .descendant_for_byte_range(range.start_byte, range.end_byte)
    else {
        return false;
    };
    loop {
        if node.is_error() || node.is_missing() {
            return false;
        }
        if matches!(
            node.kind(),
            "function_definition" | "lambda_expression" | "declaration"
        ) {
            return true;
        }
        if matches!(
            node.kind(),
            "message_expression"
                | "method_definition"
                | "method_declaration"
                | "class_interface"
                | "class_implementation"
        ) {
            return false;
        }
        let Some(parent) = node.parent() else {
            return false;
        };
        node = parent;
    }
}

fn occurrence_key(occurrence: &SemanticOccurrence) -> (usize, usize, String) {
    (
        occurrence.range.start_byte,
        occurrence.range.end_byte,
        occurrence.name.clone(),
    )
}

fn hybrid_source_symbol<'a>(
    grammar: HybridGrammar,
    symbol_id: &str,
    cpp: &'a BTreeMap<String, &SemanticSymbol>,
    objc: &'a BTreeMap<String, &SemanticSymbol>,
) -> Option<&'a SemanticSymbol> {
    match grammar {
        HybridGrammar::Cpp => cpp.get(symbol_id).copied(),
        HybridGrammar::ObjectiveC => objc.get(symbol_id).copied(),
    }
}

fn hybrid_node_map<'a>(
    grammar: HybridGrammar,
    cpp: &'a BTreeMap<String, String>,
    objc: &'a BTreeMap<String, String>,
) -> &'a BTreeMap<String, String> {
    match grammar {
        HybridGrammar::Cpp => cpp,
        HybridGrammar::ObjectiveC => objc,
    }
}

fn merge_secondary_nodes(
    rel: &str,
    primary: &[SemanticNode],
    secondary: &[SemanticNode],
    label: &str,
) -> (
    Vec<SemanticNode>,
    BTreeMap<String, String>,
    BTreeMap<String, String>,
) {
    let mut nodes = primary.to_vec();
    let primary_ids = primary
        .iter()
        .map(|node| (node.node_id.clone(), node.node_id.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut exact = primary
        .iter()
        .map(|node| {
            (
                (
                    node.range.start_byte,
                    node.range.end_byte,
                    node.kind.clone(),
                ),
                node.node_id.clone(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut secondary_ids = BTreeMap::<String, String>::new();
    for node in secondary {
        let key = (
            node.range.start_byte,
            node.range.end_byte,
            node.kind.clone(),
        );
        if let Some(existing) = exact.get(&key) {
            secondary_ids.insert(node.node_id.clone(), existing.clone());
            continue;
        }
        let output_id = stable_id("node-hybrid", &[rel, label, &node.node_id]);
        let parent_node_id = node
            .parent_node_id
            .as_ref()
            .and_then(|parent| secondary_ids.get(parent))
            .cloned();
        nodes.push(SemanticNode {
            node_id: output_id.clone(),
            parent_node_id,
            kind: node.kind.clone(),
            range: node.range.clone(),
        });
        exact.insert(key, output_id.clone());
        secondary_ids.insert(node.node_id.clone(), output_id);
    }
    (nodes, primary_ids, secondary_ids)
}

fn objective_c_native_range(tree: &Tree, source: &str, range: &TextRange) -> bool {
    let Some(mut node) = tree
        .root_node()
        .descendant_for_byte_range(range.start_byte, range.end_byte)
    else {
        return false;
    };
    loop {
        match node.kind() {
            "message_expression"
            | "block_literal"
            | "method_parameter"
            | "method_type"
            | "property_declaration"
            | "property_attributes_declaration"
            | "protocol_declaration"
            | "category_interface"
            | "category_implementation" => return true,
            "method_definition" | "method_declaration" => {
                let has_method_prefix = source
                    .get(node.start_byte()..node.end_byte())
                    .and_then(|text| text.trim_start().chars().next())
                    .is_some_and(|prefix| matches!(prefix, '-' | '+'));
                if !has_method_prefix {
                    let Some(parent) = node.parent() else {
                        return false;
                    };
                    node = parent;
                    continue;
                }
                let body_start = node
                    .named_children(&mut node.walk())
                    .find(|child| child.kind() == "compound_statement")
                    .map_or(node.end_byte(), |body| body.start_byte());
                if range.end_byte <= body_start {
                    return true;
                }
            }
            "class_interface" | "class_implementation" => {
                let body_start = node
                    .named_children(&mut node.walk())
                    .find(|child| child.kind() == "implementation_definition")
                    .map_or(node.end_byte(), |body| body.start_byte());
                if range.end_byte <= body_start {
                    return true;
                }
            }
            _ => {}
        }
        let Some(parent) = node.parent() else {
            return false;
        };
        node = parent;
    }
}

fn count_error_nodes(node: Node<'_>) -> usize {
    let own = usize::from(node.is_error() || node.is_missing());
    let mut cursor = node.walk();
    own + node
        .children(&mut cursor)
        .map(count_error_nodes)
        .sum::<usize>()
}

fn command_version(command: &str) -> Option<String> {
    static RUST_ANALYZER: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    static CLANGD: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    match command {
        "rust-analyzer" => RUST_ANALYZER
            .get_or_init(|| discover_command_version(command))
            .clone(),
        "clangd" => CLANGD
            .get_or_init(|| discover_command_version(command))
            .clone(),
        _ => discover_command_version(command),
    }
}

fn discover_command_version(command: &str) -> Option<String> {
    let output = Command::new(command)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let first_line = String::from_utf8(output.stdout).ok()?;
    let version = first_line.lines().next()?.trim();
    (!version.is_empty()).then(|| format!("{command}: {version}"))
}

fn stable_id(prefix: &str, parts: &[&str]) -> String {
    let mut material = prefix.to_string();
    for part in parts {
        material.push('\0');
        material.push_str(part);
    }
    format!("{prefix}_{}", &sha256_hex(material.as_bytes())[..24])
}

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn language_name(language: LanguageId) -> &'static str {
    match language {
        LanguageId::Rust => "rust",
        LanguageId::Cpp => "cpp",
        LanguageId::ObjectiveC => "objective-c",
        LanguageId::ObjectiveCpp => "objective-cpp",
        LanguageId::JavaScript => "javascript",
        LanguageId::TypeScript => "typescript",
        LanguageId::Python => "python",
        LanguageId::Go => "go",
        LanguageId::Unknown => "unknown",
    }
}

fn occurrence_role_name(role: OccurrenceRole) -> &'static str {
    match role {
        OccurrenceRole::Declaration => "declaration",
        OccurrenceRole::Reference => "reference",
        OccurrenceRole::Unresolved => "unresolved",
        OccurrenceRole::External => "external",
    }
}

pub fn require_capability(enabled: bool, operation: &str, language: LanguageId) -> Result<()> {
    if !enabled {
        bail!("{operation} is unavailable for {language:?}; no semantic fallback was attempted");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_symbols_ignore_comments_and_strings() {
        let source = r#"
fn hwid() -> &'static str {
    // hwid is prose
    "hwid"
}
fn caller() { let _ = hwid(); }
"#;
        let parsed = parse_document(Path::new("src/lib.rs"), source).unwrap();
        let hwid = parsed
            .symbols
            .iter()
            .find(|symbol| symbol.name == "hwid")
            .unwrap();
        let bound = parsed
            .occurrences
            .iter()
            .filter(|occurrence| occurrence.symbol_id.as_deref() == Some(&hwid.symbol_id))
            .count();
        assert_eq!(bound, 2);
    }

    #[test]
    fn repeated_locals_bind_only_inside_their_lexical_scope() {
        let source = "fn one() { let value = 1; } fn two() { let value = 2; dbg!(value); }";
        let parsed = parse_document(Path::new("src/lib.rs"), source).unwrap();
        let declarations = parsed
            .symbols
            .iter()
            .filter(|symbol| symbol.name == "value")
            .collect::<Vec<_>>();
        assert_eq!(declarations.len(), 2);
        let reference = parsed
            .occurrences
            .iter()
            .find(|occurrence| {
                occurrence.name == "value" && occurrence.role == OccurrenceRole::Reference
            })
            .expect("second function reference must resolve");
        assert_eq!(
            reference.symbol_id.as_deref(),
            Some(declarations[1].symbol_id.as_str())
        );
    }

    #[test]
    fn symbol_ids_survive_whitespace_only_reparse() {
        let first =
            parse_document(Path::new("src/lib.rs"), "fn device_id() { device_id(); }").unwrap();
        let second = parse_document(
            Path::new("src/lib.rs"),
            "\n\nfn device_id() {\n    device_id();\n}\n",
        )
        .unwrap();
        assert_eq!(first.symbols[0].symbol_id, second.symbols[0].symbol_id);
        let first_node = first
            .occurrences
            .iter()
            .find(|occurrence| occurrence.role == OccurrenceRole::Declaration)
            .unwrap();
        let second_node = second
            .occurrences
            .iter()
            .find(|occurrence| occurrence.role == OccurrenceRole::Declaration)
            .unwrap();
        assert_eq!(first_node.node_id, second_node.node_id);
    }

    #[test]
    fn rust_macro_declaration_is_indexed() {
        let parsed = parse_document(
            Path::new("src/lib.rs"),
            "macro_rules! hwid_macro { () => { 1 } } fn run() { let _ = hwid_macro!(); }",
        )
        .unwrap();
        assert!(
            parsed
                .symbols
                .iter()
                .any(|symbol| symbol.name == "hwid_macro" && symbol.kind == "macro")
        );
    }

    #[test]
    fn unsupported_languages_are_explicitly_read_only() {
        let parsed = parse_document(Path::new("src/app.rb"), "def f; end").unwrap();
        assert!(!parsed.capabilities.edit);
        assert!(parsed.capabilities.read_only_reason.is_some());
    }

    #[test]
    fn adapter_languages_have_ast_edits_but_no_fake_semantic_rename() {
        for (path, source) in [
            ("src/app.js", "function helper() { return 1; }"),
            ("src/app.ts", "function helper(): number { return 1; }"),
            ("src/app.py", "def helper():\n    return 1\n"),
            (
                "src/app.go",
                "package app\nfunc helper() int { return 1 }\n",
            ),
        ] {
            let parsed = parse_document(Path::new(path), source).unwrap();
            assert!(parsed.capabilities.parse, "{path}");
            assert!(parsed.capabilities.edit, "{path}");
            assert!(!parsed.capabilities.rename, "{path}");
            assert!(!parsed.nodes.is_empty(), "{path}");
            assert!(
                parsed.symbols.iter().any(|symbol| symbol.name == "helper"),
                "{path}: {:?}",
                parsed.symbols
            );
        }
    }

    #[test]
    fn cpp_declarators_produce_owned_symbols_and_bound_references() {
        let source =
            "static int get_hwid() { return 1; } int run() { int hwid = get_hwid(); return hwid; }";
        let parsed = parse_document(Path::new("src/main.mm"), source).unwrap();
        for name in ["get_hwid", "run", "hwid"] {
            assert!(
                parsed.symbols.iter().any(|symbol| symbol.name == name),
                "missing {name}: {:?}",
                parsed.symbols
            );
        }
        let get_hwid = parsed
            .symbols
            .iter()
            .find(|symbol| symbol.name == "get_hwid")
            .unwrap();
        assert_eq!(
            parsed
                .occurrences
                .iter()
                .filter(|occurrence| {
                    occurrence.symbol_id.as_deref() == Some(&get_hwid.symbol_id)
                })
                .count(),
            2
        );
    }

    #[test]
    fn external_cpp_api_uses_never_become_owned_declarations() {
        let source = "void run() { Trezor wallet; wallet.launcher(); }";
        let parsed = parse_document(Path::new("src/main.mm"), source).unwrap();
        assert!(!parsed.symbols.iter().any(|symbol| symbol.name == "Trezor"));
        assert!(
            !parsed
                .symbols
                .iter()
                .any(|symbol| symbol.name == "launcher")
        );
        for name in ["Trezor", "launcher"] {
            assert!(parsed.occurrences.iter().any(|occurrence| {
                occurrence.name == name && occurrence.role == OccurrenceRole::External
            }));
        }
    }

    #[test]
    fn cpp_overloads_use_literal_types_and_still_keep_distinct_identities() {
        let source = "int parse(int x) { return x; } double parse(double x) { return x; } int run() { return parse(1); }";
        let parsed = parse_document(Path::new("src/main.cpp"), source).unwrap();
        let declarations = parsed
            .symbols
            .iter()
            .filter(|symbol| symbol.name == "parse")
            .collect::<Vec<_>>();
        assert_eq!(declarations.len(), 2);
        assert_ne!(declarations[0].symbol_id, declarations[1].symbol_id);
        let call = parsed
            .occurrences
            .iter()
            .find(|occurrence| {
                occurrence.name == "parse" && occurrence.role == OccurrenceRole::Reference
            })
            .expect("integer literal must select the int overload");
        assert_eq!(
            call.symbol_id.as_deref(),
            Some(declarations[0].symbol_id.as_str())
        );
    }

    #[test]
    fn cpp_pointer_and_reference_parameters_are_local_declarations() {
        let source = r#"
struct Box { int value; };
int read(Box &box, Box *pointer) {
    int result = box.value;
    { int result = pointer->value; result += 1; }
    return result;
}
"#;
        let parsed = parse_document(Path::new("src/main.cpp"), source).unwrap();
        for name in ["box", "pointer"] {
            let symbol = parsed
                .symbols
                .iter()
                .find(|symbol| symbol.name == name && symbol.kind == "parameter")
                .unwrap_or_else(|| panic!("missing parameter {name}: {:?}", parsed.symbols));
            assert!(parsed.occurrences.iter().any(|occurrence| {
                occurrence.role == OccurrenceRole::Reference
                    && occurrence.symbol_id.as_deref() == Some(symbol.symbol_id.as_str())
            }));
        }
        let results = parsed
            .symbols
            .iter()
            .filter(|symbol| symbol.name == "result")
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 2);
        for result in results {
            assert!(parsed.occurrences.iter().any(|occurrence| {
                occurrence.role == OccurrenceRole::Reference
                    && occurrence.symbol_id.as_deref() == Some(result.symbol_id.as_str())
            }));
        }
    }

    #[test]
    fn cpp_member_binding_uses_receiver_type_not_global_spelling() {
        let source = r#"
struct Left { int value; };
struct Right { int value; };
int sum(Left &left, Right &right) { return left.value + right.value; }
"#;
        let parsed = parse_document(Path::new("src/main.cpp"), source).unwrap();
        let left = parsed
            .symbols
            .iter()
            .find(|symbol| symbol.qualified_name == "Left::value")
            .unwrap();
        let right = parsed
            .symbols
            .iter()
            .find(|symbol| symbol.qualified_name == "Right::value")
            .unwrap();
        let references = parsed
            .occurrences
            .iter()
            .filter(|occurrence| {
                occurrence.name == "value" && occurrence.role == OccurrenceRole::Reference
            })
            .collect::<Vec<_>>();
        assert_eq!(references.len(), 2);
        assert_eq!(
            references[0].symbol_id.as_deref(),
            Some(left.symbol_id.as_str())
        );
        assert_eq!(
            references[1].symbol_id.as_deref(),
            Some(right.symbol_id.as_str())
        );
    }

    #[test]
    fn cpp_member_call_does_not_bind_to_a_different_receiver_type() {
        let source = r#"
class Database;
class ServerState {
public:
    void delete_agent(int token) {}
};
ServerState g_state;
void route(Database& db) {
    g_state.delete_agent(1);
    db.delete_agent(1);
}
"#;
        let parsed = parse_document(Path::new("src/server.cpp"), source).unwrap();
        let target = parsed
            .symbols
            .iter()
            .find(|symbol| symbol.qualified_name == "ServerState::delete_agent")
            .expect("inline method must retain its owning class");
        let calls = parsed
            .occurrences
            .iter()
            .filter(|occurrence| {
                occurrence.name == "delete_agent" && occurrence.role != OccurrenceRole::Declaration
            })
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 2, "{:#?}", parsed.occurrences);
        assert_eq!(
            calls[0].symbol_id.as_deref(),
            Some(target.symbol_id.as_str())
        );
        assert_eq!(calls[1].role, OccurrenceRole::External);
        assert_eq!(calls[1].symbol_id, None);
    }

    #[test]
    fn cpp_chained_and_smart_pointer_receivers_propagate_member_types() {
        let source = r#"
struct Leaf { int value; };
struct Holder { Leaf leaf; };
int read(std::unique_ptr<Holder> &holder) { return holder->leaf.value; }
"#;
        let parsed = parse_document(Path::new("src/main.cpp"), source).unwrap();
        for qualified in ["Holder::leaf", "Leaf::value"] {
            let symbol = parsed
                .symbols
                .iter()
                .find(|symbol| symbol.qualified_name == qualified)
                .unwrap_or_else(|| panic!("missing {qualified}"));
            assert!(
                parsed.occurrences.iter().any(|occurrence| {
                    occurrence.role == OccurrenceRole::Reference
                        && occurrence.symbol_id.as_deref() == Some(symbol.symbol_id.as_str())
                }),
                "missing chained binding for {qualified}: {:?}",
                parsed.occurrences
            );
        }
    }

    #[test]
    fn cpp_qualified_calls_and_redeclarations_share_one_symbol() {
        let source = r#"
namespace outer { int helper(int value); int helper(int value) { return value; } }
int helper(int value) { return value + 1; }
int run() { return outer::helper(1); }
"#;
        let parsed = parse_document(Path::new("src/main.cpp"), source).unwrap();
        let outer = parsed
            .symbols
            .iter()
            .filter(|symbol| symbol.qualified_name == "outer::helper")
            .collect::<Vec<_>>();
        assert_eq!(outer.len(), 1, "prototype and definition must coalesce");
        let declarations = parsed
            .occurrences
            .iter()
            .filter(|occurrence| {
                occurrence.role == OccurrenceRole::Declaration
                    && occurrence.symbol_id.as_deref() == Some(outer[0].symbol_id.as_str())
            })
            .count();
        assert_eq!(declarations, 2);
        let call = parsed
            .occurrences
            .iter()
            .find(|occurrence| {
                occurrence.name == "helper" && occurrence.role == OccurrenceRole::Reference
            })
            .unwrap();
        assert_eq!(call.symbol_id.as_deref(), Some(outer[0].symbol_id.as_str()));
    }

    #[test]
    fn cpp_namespace_declarations_are_not_confused_with_using_directives() {
        let source = r#"
namespace outer { namespace inner { int value; } }
using namespace outer;
int run() { return outer::inner::value; }
"#;
        let parsed = parse_document(Path::new("src/main.mm"), source).unwrap();
        assert_eq!(
            parsed
                .symbols
                .iter()
                .filter(|symbol| symbol.name == "outer" && symbol.kind == "module")
                .count(),
            1
        );
        assert!(
            parsed
                .symbols
                .iter()
                .any(|symbol| symbol.qualified_name == "outer::inner")
        );
        assert!(
            !parsed
                .symbols
                .iter()
                .any(|symbol| matches!(symbol.name.as_str(), "using" | "namespace" | "if"))
        );
    }

    #[test]
    fn cpp_constructors_and_auto_factory_receivers_bind_to_owned_types() {
        let source = r#"
namespace api {
class Widget {
public:
    Widget(int value);
    ~Widget();
    int get() const;
    static Widget make();
};
Widget::Widget(int value) {}
Widget::~Widget() = default;
int Widget::get() const { return 1; }
Widget Widget::make() { return Widget(1); }
}
int run() { auto widget = api::Widget::make(); return widget.get(); }
"#;
        let parsed = parse_document(Path::new("src/main.mm"), source).unwrap();
        let widget = parsed
            .symbols
            .iter()
            .find(|symbol| symbol.qualified_name == "api::Widget")
            .unwrap();
        assert_eq!(
            parsed
                .symbols
                .iter()
                .filter(|symbol| symbol.name == "Widget")
                .count(),
            1,
            "constructors/destructors must not become duplicate symbols"
        );
        assert!(
            parsed
                .occurrences
                .iter()
                .filter(|occurrence| occurrence.name == "Widget")
                .all(
                    |occurrence| occurrence.symbol_id.as_deref() == Some(widget.symbol_id.as_str())
                )
        );
        let get = parsed
            .symbols
            .iter()
            .find(|symbol| symbol.qualified_name == "api::Widget::get")
            .unwrap();
        assert!(parsed.occurrences.iter().any(|occurrence| {
            occurrence.role == OccurrenceRole::Reference
                && occurrence.symbol_id.as_deref() == Some(get.symbol_id.as_str())
        }));
    }

    #[test]
    fn cpp_unknown_conversion_keeps_same_arity_overload_ambiguous() {
        let source = r#"
struct A {}; struct B {}; struct Convertible {};
int parse(A value) { return 1; }
int parse(B value) { return 2; }
int run(Convertible value) { return parse(value); }
"#;
        let parsed = parse_document(Path::new("src/main.cpp"), source).unwrap();
        assert!(parsed.occurrences.iter().any(|occurrence| {
            occurrence.name == "parse" && occurrence.role == OccurrenceRole::Unresolved
        }));
    }

    #[test]
    fn cpp_advanced_declarators_templates_and_structured_bindings_are_owned() {
        let source = r#"
int *returns_pointer(int value);
int (*function_pointer)(int value);
using Handler = void (*)(int);
template <typename Value, int Size>
struct Buffer { Value items[Size]; };
Buffer<int, 2> global_buffer;
void structured() {
    auto [left, right] = get_pair();
    use(left, right);
    for (auto &[key, value] : entries) { use(key, value); }
}
"#;
        let parsed = parse_document(Path::new("src/main.cpp"), source).unwrap();
        let expected = [
            ("returns_pointer", "function"),
            ("function_pointer", "variable"),
            ("Handler", "type"),
            ("Value", "type_parameter"),
            ("Size", "template_parameter"),
            ("left", "variable"),
            ("right", "variable"),
            ("key", "variable"),
            ("value", "variable"),
        ];
        for (name, kind) in expected {
            assert!(
                parsed
                    .symbols
                    .iter()
                    .any(|symbol| symbol.name == name && symbol.kind == kind),
                "missing {kind} {name}: {:?}",
                parsed.symbols
            );
        }
        for name in ["Value", "Size", "Buffer", "left", "right", "key", "value"] {
            assert!(
                parsed.occurrences.iter().any(|occurrence| {
                    occurrence.name == name && occurrence.role == OccurrenceRole::Reference
                }),
                "missing advanced reference {name}: {:?}",
                parsed.occurrences
            );
        }
    }

    #[test]
    fn cpp_member_call_receivers_remain_value_references() {
        let source = r#"
struct Buffer { void push(int value); };
void write() {
    Buffer output;
    if (output.push(1), true) { output.push(2); }
}
"#;
        let parsed = parse_document(Path::new("src/main.cpp"), source).unwrap();
        let output = parsed
            .symbols
            .iter()
            .find(|symbol| symbol.name == "output")
            .unwrap();
        let references = parsed
            .occurrences
            .iter()
            .filter(|occurrence| {
                occurrence.name == "output" && occurrence.role == OccurrenceRole::Reference
            })
            .collect::<Vec<_>>();
        assert_eq!(references.len(), 2, "{:#?}", parsed.occurrences);
        assert!(references.iter().all(|occurrence| {
            occurrence.symbol_id.as_deref() == Some(output.symbol_id.as_str())
        }));
    }

    #[test]
    fn cpp_preprocessor_branch_declarations_share_one_binding() {
        let source = r#"
int render() {
#ifdef __APPLE__
    auto output = make_apple_output();
#else
    auto output = make_portable_output();
#endif
    return output.size();
}
"#;
        let parsed = parse_document(Path::new("src/main.cpp"), source).unwrap();
        let outputs = parsed
            .symbols
            .iter()
            .filter(|symbol| symbol.name == "output")
            .collect::<Vec<_>>();
        assert_eq!(outputs.len(), 1, "{:#?}", parsed.symbols);
        assert_eq!(
            parsed
                .occurrences
                .iter()
                .filter(|occurrence| {
                    occurrence.name == "output"
                        && occurrence.role == OccurrenceRole::Declaration
                        && occurrence.symbol_id.as_deref() == Some(outputs[0].symbol_id.as_str())
                })
                .count(),
            2
        );
        assert!(parsed.occurrences.iter().any(|occurrence| {
            occurrence.name == "output"
                && occurrence.role == OccurrenceRole::Reference
                && occurrence.symbol_id.as_deref() == Some(outputs[0].symbol_id.as_str())
        }));
    }

    #[test]
    fn cpp_vexing_parse_arguments_can_bind_structured_values() {
        let source = r#"
struct Locked {
    Mutex mutex;
    void work();
};
void Locked::work() { Guard guard(mutex); }
void load(Entries& inputs) {
    for (const auto& [path, info] : inputs) {
        Stream stream(path, Mode::binary);
        consume(path.c_str(), info);
    }
}
"#;
        let parsed = parse_document(Path::new("src/main.cpp"), source).unwrap();
        let path = parsed
            .symbols
            .iter()
            .find(|symbol| symbol.name == "path")
            .expect("structured binding path");
        assert_eq!(
            parsed
                .occurrences
                .iter()
                .filter(|occurrence| occurrence.name == "path"
                    && occurrence.role == OccurrenceRole::Reference
                    && occurrence.symbol_id.as_deref() == Some(path.symbol_id.as_str()))
                .count(),
            2,
            "{:#?}",
            parsed.occurrences
        );
        let mutex = parsed
            .symbols
            .iter()
            .find(|symbol| symbol.qualified_name == "Locked::mutex")
            .expect("owned field");
        assert!(parsed.occurrences.iter().any(|occurrence| {
            occurrence.name == "mutex"
                && occurrence.role == OccurrenceRole::Reference
                && occurrence.symbol_id.as_deref() == Some(mutex.symbol_id.as_str())
        }));
    }

    #[test]
    fn objective_cpp_keeps_locals_with_message_initializers() {
        let source = r#"
namespace goidaware { enum class WalletType {}; }
static void scan_items() {
    NSArray *items = @[@"first", @"second"];
    for (NSString *item in items) { consume(item); }
}
@interface Worker : NSObject
@property(nonatomic, assign) goidaware::WalletType walletType;
@end
@implementation Worker
- (instancetype)init { self = [super init]; return self; }
- (void)run:(NSString *)callId result:(NSString *)json {
    NSFileManager *manager = [NSFileManager defaultManager];
    NSString *safeCallId = [callId stringByReplacingOccurrencesOfString:@"'" withString:@"\\'"];
    safeCallId = [safeCallId stringByReplacingOccurrencesOfString:@"\\" withString:@"\\\\"];
    [manager removeItemAtPath:safeCallId error:nil];
}
@end
"#;
        let parsed = parse_document(Path::new("src/main.mm"), source).unwrap();
        for name in ["item", "manager", "safeCallId"] {
            let symbol = parsed
                .symbols
                .iter()
                .find(|symbol| symbol.name == name)
                .unwrap_or_else(|| panic!("missing local {name}: {:#?}", parsed.symbols));
            assert!(
                parsed.occurrences.iter().any(|occurrence| {
                    occurrence.name == name
                        && occurrence.role == OccurrenceRole::Reference
                        && occurrence.symbol_id.as_deref() == Some(symbol.symbol_id.as_str())
                }),
                "unbound local {name}: {:#?}",
                parsed.occurrences
            );
            assert!(
                !parsed.occurrences.iter().any(|occurrence| {
                    occurrence.name == name && occurrence.role == OccurrenceRole::Unresolved
                }),
                "unresolved local {name}: {:#?}",
                parsed.occurrences
            );
        }
    }

    #[test]
    fn objective_cpp_projects_autoreleasepool_locals_and_preserves_cpp_qualifiers() {
        let source = r#"
#include <nlohmann/json.hpp>
using json = nlohmann::json;
void clean() {
    @autoreleasepool {
        NSFileManager *manager = [NSFileManager defaultManager];
        if ([manager fileExistsAtPath:@"/tmp/item"]) {
            json result;
            consume(result);
        }
    }
}
"#;
        let parsed = parse_document(Path::new("src/main.mm"), source).unwrap();
        for name in ["manager", "result"] {
            let symbol = parsed
                .symbols
                .iter()
                .find(|symbol| symbol.name == name)
                .unwrap_or_else(|| panic!("missing local {name}: {:#?}", parsed.symbols));
            assert!(parsed.occurrences.iter().any(|occurrence| {
                occurrence.name == name
                    && occurrence.role == OccurrenceRole::Reference
                    && occurrence.symbol_id.as_deref() == Some(symbol.symbol_id.as_str())
            }));
        }
        let qualified_target = parsed
            .occurrences
            .iter()
            .find(|occurrence| {
                occurrence.name == "json"
                    && source.get(
                        occurrence.range.start_byte.saturating_sub(10)..occurrence.range.end_byte,
                    ) == Some("nlohmann::json")
            })
            .expect("qualified target occurrence");
        assert_eq!(qualified_target.role, OccurrenceRole::External);
    }

    #[test]
    fn objective_c_selectors_bind_by_class_full_selector_and_fragment() {
        let source = r#"
@interface Worker : NSObject
@property(nonatomic) NSInteger count;
- (void)handleValue:(NSInteger)value other:(NSString *)other;
@end
@implementation Worker
- (void)handleValue:(NSInteger)value other:(NSString *)other {
    NSInteger result = value;
    self.count = result;
    [self handleValue:result other:other];
}
@end
"#;
        let parsed = parse_document(Path::new("src/worker.m"), source).unwrap();
        assert_eq!(
            parsed
                .symbols
                .iter()
                .filter(|symbol| symbol.name == "Worker" && symbol.kind == "class")
                .count(),
            1
        );
        for fragment in ["handleValue", "other"] {
            let symbol = parsed
                .symbols
                .iter()
                .find(|symbol| symbol.name == fragment && symbol.kind == "method")
                .unwrap_or_else(|| panic!("missing selector fragment {fragment}"));
            assert!(parsed.occurrences.iter().any(|occurrence| {
                occurrence.role == OccurrenceRole::Reference
                    && occurrence.symbol_id.as_deref() == Some(symbol.symbol_id.as_str())
            }));
        }
        let count = parsed
            .symbols
            .iter()
            .find(|symbol| symbol.qualified_name == "Worker::count")
            .expect("Objective-C property must be an owned field");
        assert!(parsed.occurrences.iter().any(|occurrence| {
            occurrence.role == OccurrenceRole::Reference
                && occurrence.symbol_id.as_deref() == Some(count.symbol_id.as_str())
        }));
    }

    #[test]
    fn objective_cpp_merges_cpp_bodies_and_objective_c_selectors() {
        let source = r#"
namespace detail {
struct Counter { int value; int read() const { return value; } };
}
@interface Worker : NSObject
@property(nonatomic) NSInteger count;
- (NSInteger)sumValue:(NSInteger)value other:(NSInteger)other;
@end
@implementation Worker
- (NSInteger)sumValue:(NSInteger)value other:(NSInteger)other {
    detail::Counter counter{(int)value};
    NSInteger result = counter.read() + other;
    self.count = result;
    return [self sumValue:result other:0];
}
@end
"#;
        let parsed = parse_document(Path::new("src/worker.mm"), source).unwrap();
        for qualified in [
            "detail::Counter",
            "detail::Counter::read",
            "Worker",
            "Worker::count",
        ] {
            assert!(
                parsed
                    .symbols
                    .iter()
                    .any(|symbol| symbol.qualified_name == qualified),
                "missing hybrid symbol {qualified}: {:?}",
                parsed.symbols
            );
        }
        let counter = parsed
            .symbols
            .iter()
            .find(|symbol| symbol.name == "counter")
            .expect("C++ local inside Objective-C method must be discovered");
        assert_eq!(counter.qualified_name, "Worker::[sumValue:other:]::counter");
        for name in ["counter", "read", "count", "sumValue"] {
            assert!(
                parsed.occurrences.iter().any(|occurrence| {
                    occurrence.name == name && occurrence.role == OccurrenceRole::Reference
                }),
                "missing hybrid reference {name}: {:?}",
                parsed.occurrences
            );
        }
    }

    #[test]
    fn unrelated_external_member_does_not_poison_owned_same_name_field() {
        let source = r#"
struct Local { int launch; };
void run(Foreign &foreign) { foreign.launch(); }
"#;
        let parsed = parse_document(Path::new("src/main.cpp"), source).unwrap();
        assert!(parsed.occurrences.iter().any(|occurrence| {
            occurrence.name == "launch" && occurrence.role == OccurrenceRole::External
        }));
        assert!(!parsed.occurrences.iter().any(|occurrence| {
            occurrence.name == "launch" && occurrence.role == OccurrenceRole::Unresolved
        }));
    }

    #[test]
    fn dependency_and_vendor_paths_are_not_owned() {
        assert_eq!(
            SourceOrigin::for_path(Path::new("build/_deps/lib/src/a.cc")),
            SourceOrigin::Dependency
        );
        assert_eq!(
            SourceOrigin::for_path(Path::new("vendor/lib/src/a.rs")),
            SourceOrigin::Vendor
        );
    }
}
