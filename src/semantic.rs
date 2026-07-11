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
        LanguageId::ObjectiveCpp => Box::new(TreeSitterBackend::cpp(LanguageId::ObjectiveCpp)),
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
        let mut parser = Parser::new();
        parser
            .set_language(&self.grammar)
            .with_context(|| format!("load {:?} tree-sitter grammar", self.language))?;
        let tree = parser
            .parse(source, None)
            .ok_or_else(|| anyhow::anyhow!("tree-sitter parser returned no tree"))?;
        build_document(rel_path, source, self.language, self.capabilities(), &tree)
    }
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
    let mut nodes = Vec::new();
    let mut raw_identifiers = Vec::new();
    collect_nodes(
        tree.root_node(),
        source,
        &rel,
        Vec::new(),
        &mut nodes,
        &mut raw_identifiers,
    )?;

    let mut declaration_ordinals = BTreeMap::<(String, String), usize>::new();
    let mut symbols = Vec::new();
    for raw in raw_identifiers
        .iter()
        .filter(|node| node.declaration_kind.is_some())
    {
        let kind = raw.declaration_kind.as_deref().unwrap_or("symbol");
        let qualified = qualified_name(raw, &raw_identifiers);
        let ordinal = declaration_ordinals
            .entry((kind.to_string(), qualified.clone()))
            .or_default();
        let symbol_id = stable_id(
            "sym",
            &[
                &rel,
                language_name(language),
                kind,
                &qualified,
                &ordinal.to_string(),
            ],
        );
        *ordinal += 1;
        symbols.push(SemanticSymbol {
            symbol_id,
            node_id: raw.node_id.clone(),
            name: raw.text.clone(),
            kind: kind.to_string(),
            qualified_name: qualified,
            scope_node_id: raw.parent_node_id.clone(),
            range: raw.range.clone(),
            origin,
            locally_bound: false,
        });
    }

    let symbol_by_node = symbols
        .iter()
        .map(|symbol| (symbol.node_id.clone(), symbol.symbol_id.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut symbols_by_name = BTreeMap::<String, Vec<String>>::new();
    for symbol in &symbols {
        symbols_by_name
            .entry(symbol.name.clone())
            .or_default()
            .push(symbol.symbol_id.clone());
    }

    let mut occurrences = Vec::with_capacity(raw_identifiers.len());
    for raw in &raw_identifiers {
        let declaration_symbol = symbol_by_node.get(&raw.node_id).cloned();
        let unique_local = symbols_by_name
            .get(&raw.text)
            .filter(|matches| matches.len() == 1)
            .and_then(|matches| matches.first())
            .cloned();
        let symbol_id = declaration_symbol.clone().or(unique_local);
        let role = if declaration_symbol.is_some() {
            OccurrenceRole::Declaration
        } else if symbol_id.is_some() {
            OccurrenceRole::Reference
        } else {
            OccurrenceRole::Unresolved
        };
        occurrences.push(SemanticOccurrence {
            occurrence_id: stable_id("occ", &[&rel, &raw.node_id, occurrence_role_name(role)]),
            node_id: raw.node_id.clone(),
            symbol_id,
            name: raw.text.clone(),
            role,
            range: raw.range.clone(),
        });
    }
    for symbol in &mut symbols {
        symbol.locally_bound = occurrences.iter().any(|occurrence| {
            occurrence.role == OccurrenceRole::Reference
                && occurrence.symbol_id.as_deref() == Some(symbol.symbol_id.as_str())
        });
    }

    Ok(ParsedDocument {
        rel_path: rel,
        language,
        content_hash: sha256_hex(source.as_bytes()),
        origin,
        capabilities,
        parse_errors: count_error_nodes(tree.root_node()),
        nodes,
        symbols,
        occurrences,
    })
}

#[derive(Debug)]
struct RawIdentifier {
    node_id: String,
    parent_node_id: Option<String>,
    structural_path: Vec<usize>,
    text: String,
    range: TextRange,
    declaration_kind: Option<String>,
}

fn collect_nodes(
    node: Node<'_>,
    source: &str,
    rel_path: &str,
    structural_path: Vec<usize>,
    nodes: &mut Vec<SemanticNode>,
    identifiers: &mut Vec<RawIdentifier>,
) -> Result<()> {
    let path_key = structural_path
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(".");
    let node_id = stable_id("node", &[rel_path, node.kind(), &path_key]);
    let parent_node_id = node.parent().map(|parent| {
        let mut parent_path = structural_path.clone();
        parent_path.pop();
        let key = parent_path
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(".");
        stable_id("node", &[rel_path, parent.kind(), &key])
    });
    nodes.push(SemanticNode {
        node_id: node_id.clone(),
        parent_node_id: parent_node_id.clone(),
        kind: node.kind().to_string(),
        range: TextRange::from_node(node),
    });

    if is_identifier_node(node.kind()) {
        let text = node
            .utf8_text(source.as_bytes())
            .context("read identifier node text")?;
        let declaration_kind = declaration_kind(node).map(ToOwned::to_owned);
        identifiers.push(RawIdentifier {
            node_id,
            parent_node_id,
            structural_path: structural_path.clone(),
            text: text.to_string(),
            range: TextRange::from_node(node),
            declaration_kind,
        });
    }

    let mut cursor = node.walk();
    for (index, child) in node.named_children(&mut cursor).enumerate() {
        let mut child_path = structural_path.clone();
        child_path.push(index);
        collect_nodes(child, source, rel_path, child_path, nodes, identifiers)?;
    }
    Ok(())
}

fn is_identifier_node(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "field_identifier"
            | "type_identifier"
            | "namespace_identifier"
            | "scoped_identifier"
            | "method_identifier"
    )
}

fn declaration_kind(node: Node<'_>) -> Option<&'static str> {
    let parent = node.parent()?;
    if matches!(parent.kind(), "method_definition" | "method_declaration")
        && node.kind() == "identifier"
    {
        return Some("method");
    }
    let name_matches = parent
        .child_by_field_name("name")
        .is_some_and(|name| name.id() == node.id());
    let pattern_matches = parent
        .child_by_field_name("pattern")
        .is_some_and(|pattern| pattern.id() == node.id());
    let declarator_matches = parent
        .child_by_field_name("declarator")
        .is_some_and(|declarator| declarator.id() == node.id());
    if declarator_matches {
        return match parent.kind() {
            "function_declarator" => Some("function"),
            "init_declarator" | "declaration" => Some("variable"),
            "parameter_declaration" | "optional_parameter_declaration" => Some("parameter"),
            "type_definition" => Some("type"),
            _ => None,
        };
    }
    if pattern_matches {
        return match parent.kind() {
            "let_declaration" => Some("variable"),
            "parameter" | "closure_parameters" => Some("parameter"),
            _ => None,
        };
    }
    if !name_matches {
        return None;
    }
    match parent.kind() {
        "function_item"
        | "function_declarator"
        | "function_declaration"
        | "function_definition"
        | "method_declaration"
        | "method_definition" => Some("function"),
        "struct_item" | "struct_specifier" => Some("struct"),
        "class_specifier" | "class_declaration" | "class_definition" => Some("class"),
        "enum_item" | "enum_specifier" => Some("enum"),
        "union_item" | "union_specifier" => Some("union"),
        "trait_item" => Some("trait"),
        "mod_item" | "namespace_definition" => Some("module"),
        "type_item" | "type_definition" => Some("type"),
        "const_item" => Some("const"),
        "static_item" => Some("static"),
        "field_declaration" => Some("field"),
        "variable_declarator" | "var_spec" => Some("variable"),
        "required_parameter" | "optional_parameter" | "typed_parameter" => Some("parameter"),
        "macro_definition" => Some("macro"),
        _ => None,
    }
}

fn qualified_name(raw: &RawIdentifier, all: &[RawIdentifier]) -> String {
    let mut parents = all
        .iter()
        .filter(|candidate| {
            candidate.declaration_kind.is_some()
                && candidate.structural_path.len() < raw.structural_path.len()
                && raw.structural_path.starts_with(&candidate.structural_path)
        })
        .map(|candidate| candidate.text.as_str())
        .collect::<Vec<_>>();
    parents.push(raw.text.as_str());
    parents.join("::")
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
    fn ambiguous_shadowed_name_is_not_guessed() {
        let source = "fn one() { let value = 1; } fn two() { let value = 2; dbg!(value); }";
        let parsed = parse_document(Path::new("src/lib.rs"), source).unwrap();
        let unresolved = parsed
            .occurrences
            .iter()
            .filter(|occurrence| {
                occurrence.name == "value" && occurrence.role != OccurrenceRole::Declaration
            })
            .all(|occurrence| occurrence.role == OccurrenceRole::Unresolved);
        assert!(unresolved);
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
                occurrence.name == name && occurrence.role == OccurrenceRole::Unresolved
            }));
        }
    }

    #[test]
    fn cpp_overloads_remain_distinct_and_calls_fail_closed() {
        let source = "int parse(int x) { return x; } double parse(double x) { return x; } int run() { return parse(1); }";
        let parsed = parse_document(Path::new("src/main.cpp"), source).unwrap();
        let declarations = parsed
            .symbols
            .iter()
            .filter(|symbol| symbol.name == "parse")
            .collect::<Vec<_>>();
        assert_eq!(declarations.len(), 2);
        assert_ne!(declarations[0].symbol_id, declarations[1].symbol_id);
        assert!(parsed.occurrences.iter().any(|occurrence| {
            occurrence.name == "parse" && occurrence.role == OccurrenceRole::Unresolved
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
