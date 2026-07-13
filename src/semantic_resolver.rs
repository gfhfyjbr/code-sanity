use super::*;

#[derive(Debug)]
pub(super) struct SemanticAnalysis {
    pub nodes: Vec<SemanticNode>,
    pub symbols: Vec<SemanticSymbol>,
    pub occurrences: Vec<SemanticOccurrence>,
}

#[derive(Debug, Clone)]
struct ScopeFrame {
    tree_id: usize,
    node_id: String,
    kind: String,
    label: Option<String>,
    owner_type_hint: Option<String>,
}

#[derive(Debug, Clone)]
struct CallableInfo {
    min_arity: usize,
    max_arity: Option<usize>,
    signature: String,
    parameter_types: Vec<String>,
}

#[derive(Debug, Clone)]
struct ObjectiveCMethodIdentity {
    selector: String,
    selector_indexes: BTreeMap<usize, usize>,
    arity: usize,
}

#[derive(Debug, Clone)]
struct DeclarationInfo {
    kind: String,
    owner_tree_id: usize,
    owner_node_id: String,
    exclude_owner_scope: bool,
    declared_type: Option<String>,
    initializer: Option<String>,
    callable: Option<CallableInfo>,
    selector: Option<String>,
    selector_index: Option<usize>,
    /// Declarations in mutually exclusive preprocessor branches describe the
    /// same source-level binding even though both branches are present in the
    /// syntax tree. The surrounding conditional identity lets us coalesce
    /// those alternatives without conflating independent lexical bindings.
    conditional_group: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReferenceContext {
    ExternalOnly,
    Value,
    Type,
    TypeOrValue,
    Qualifier,
    Call {
        arity: usize,
        arguments: Vec<CallArgument>,
    },
    Member {
        receiver: String,
        call_arity: Option<usize>,
        call_arguments: Vec<CallArgument>,
    },
    ObjectiveCMethod {
        receiver: String,
        selector: String,
        selector_index: usize,
        arity: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CallArgument {
    text: String,
    syntax_kind: String,
}

#[derive(Debug, Clone)]
struct RawIdentifier {
    node_id: String,
    text: String,
    range: TextRange,
    declaration: Option<DeclarationInfo>,
    lexical_scopes: Vec<ScopeFrame>,
    declaration_scopes: Vec<ScopeFrame>,
    context: ReferenceContext,
    qualifier: Vec<String>,
    owner_type: Option<String>,
    enclosing_type: Option<String>,
}

#[derive(Debug, Clone)]
struct BindingCandidate {
    symbol_id: String,
    kind: String,
    start_byte: usize,
    scopes: Vec<String>,
    scope_labels: Vec<String>,
    declared_type: Option<String>,
    initializer: Option<String>,
    owner_type: Option<String>,
    callable: Option<CallableInfo>,
    selector: Option<String>,
    selector_index: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Resolution {
    Bound(String),
    Ambiguous,
    External,
}

pub(super) fn analyze(
    rel: &str,
    source: &str,
    language: LanguageId,
    tree: &Tree,
) -> Result<SemanticAnalysis> {
    let mut nodes = Vec::new();
    let mut raw_identifiers = Vec::new();
    collect_nodes(
        tree.root_node(),
        source,
        rel,
        language,
        Vec::new(),
        &mut Vec::new(),
        &mut nodes,
        &mut raw_identifiers,
    )?;

    let declared_type_names = raw_identifiers
        .iter()
        .filter_map(|raw| {
            raw.declaration.as_ref().and_then(|declaration| {
                matches!(
                    declaration.kind.as_str(),
                    "class" | "struct" | "union" | "enum" | "type" | "trait"
                )
                .then(|| raw.text.clone())
            })
        })
        .collect::<BTreeSet<_>>();
    for raw in &mut raw_identifiers {
        raw.enclosing_type = raw
            .lexical_scopes
            .iter()
            .rev()
            .filter_map(|scope| scope.owner_type_hint.as_ref())
            .find(|name| declared_type_names.contains(*name))
            .cloned();
        if raw.owner_type.is_none() {
            raw.owner_type = raw
                .qualifier
                .last()
                .filter(|name| declared_type_names.contains(*name))
                .cloned();
        }
        if raw.owner_type.is_some()
            && raw
                .declaration
                .as_ref()
                .is_some_and(|declaration| declaration.kind == "function")
        {
            raw.declaration.as_mut().expect("checked declaration").kind = "method".to_string();
        }
    }

    // Keep the historical ordinal component for stable IDs in fresh indexes;
    // semantic_store additionally preserves existing IDs by node identity when
    // a resolver-version reindex upgrades an initialized workspace.
    let mut declaration_ordinals = BTreeMap::<(String, String), usize>::new();
    let mut symbols = Vec::<SemanticSymbol>::new();
    let mut symbol_by_node = BTreeMap::<String, String>::new();
    let mut candidates_by_name = BTreeMap::<String, Vec<BindingCandidate>>::new();
    let mut grouped_symbols = BTreeMap::<String, String>::new();
    for raw in raw_identifiers
        .iter()
        .filter(|raw| raw.declaration.is_some())
    {
        let declaration = raw.declaration.as_ref().expect("filtered declaration");
        let qualified = qualified_name(raw);
        let group_key = declaration_group_key(raw, &qualified);
        let existing_group = group_key
            .as_ref()
            .and_then(|key| grouped_symbols.get(key))
            .cloned();
        let symbol_id = existing_group.unwrap_or_else(|| {
            let value = if let Some(key) = &group_key {
                // A normalized callable/type identity is independent of
                // declaration order, so overload reorderings retain IDs.
                stable_id(
                    "sym",
                    &[
                        rel,
                        language_name(language),
                        &declaration.kind,
                        &qualified,
                        key,
                    ],
                )
            } else {
                let ordinal = declaration_ordinals
                    .entry((declaration.kind.clone(), qualified.clone()))
                    .or_default();
                let value = stable_id(
                    "sym",
                    &[
                        rel,
                        language_name(language),
                        &declaration.kind,
                        &qualified,
                        &ordinal.to_string(),
                    ],
                );
                *ordinal += 1;
                value
            };
            if let Some(key) = &group_key {
                grouped_symbols.insert(key.clone(), value.clone());
            }
            value
        });
        symbol_by_node.insert(raw.node_id.clone(), symbol_id.clone());
        let scope_ids = raw
            .declaration_scopes
            .iter()
            .map(|scope| scope.node_id.clone())
            .collect::<Vec<_>>();
        let mut scope_labels = raw
            .declaration_scopes
            .iter()
            .filter_map(|scope| scope.label.clone())
            .collect::<Vec<_>>();
        append_qualified_components(&mut scope_labels, &raw.qualifier);
        candidates_by_name
            .entry(raw.text.clone())
            .or_default()
            .push(BindingCandidate {
                symbol_id: symbol_id.clone(),
                kind: declaration.kind.clone(),
                start_byte: raw.range.start_byte,
                scopes: scope_ids,
                scope_labels,
                declared_type: declaration.declared_type.clone(),
                initializer: declaration.initializer.clone(),
                owner_type: raw.owner_type.clone(),
                callable: declaration.callable.clone(),
                selector: declaration.selector.clone(),
                selector_index: declaration.selector_index,
            });
        if !symbols.iter().any(|symbol| symbol.symbol_id == symbol_id) {
            symbols.push(SemanticSymbol {
                symbol_id,
                node_id: raw.node_id.clone(),
                name: raw.text.clone(),
                kind: declaration.kind.clone(),
                qualified_name: qualified,
                scope_node_id: Some(declaration.owner_node_id.clone()),
                range: raw.range.clone(),
                origin: SourceOrigin::for_path(Path::new(rel)),
                locally_bound: false,
            });
        }
    }

    let mut occurrences = Vec::with_capacity(raw_identifiers.len());
    for raw in &raw_identifiers {
        let declaration_symbol = symbol_by_node.get(&raw.node_id).cloned();
        let resolution = declaration_symbol
            .clone()
            .map(Resolution::Bound)
            .unwrap_or_else(|| resolve_reference(raw, &candidates_by_name));
        let (symbol_id, role) = match resolution {
            Resolution::Bound(symbol_id) if declaration_symbol.is_some() => {
                (Some(symbol_id), OccurrenceRole::Declaration)
            }
            Resolution::Bound(symbol_id) => (Some(symbol_id), OccurrenceRole::Reference),
            Resolution::Ambiguous => (None, OccurrenceRole::Unresolved),
            Resolution::External => (None, OccurrenceRole::External),
        };
        occurrences.push(SemanticOccurrence {
            occurrence_id: stable_id("occ", &[rel, &raw.node_id, occurrence_role_name(role)]),
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

    Ok(SemanticAnalysis {
        nodes,
        symbols,
        occurrences,
    })
}

#[allow(clippy::too_many_arguments)]
fn collect_nodes(
    node: Node<'_>,
    source: &str,
    rel_path: &str,
    language: LanguageId,
    structural_path: Vec<usize>,
    scopes: &mut Vec<ScopeFrame>,
    nodes: &mut Vec<SemanticNode>,
    identifiers: &mut Vec<RawIdentifier>,
) -> Result<()> {
    let current_node_id = node_id(rel_path, node, &structural_path);
    let parent_node_id = node.parent().map(|parent| {
        let mut parent_path = structural_path.clone();
        parent_path.pop();
        node_id(rel_path, parent, &parent_path)
    });
    nodes.push(SemanticNode {
        node_id: current_node_id.clone(),
        parent_node_id: parent_node_id.clone(),
        kind: node.kind().to_string(),
        range: TextRange::from_node(node),
    });

    let pushed_scope = is_scope_node(node.kind());
    if pushed_scope {
        scopes.push(ScopeFrame {
            tree_id: node.id(),
            node_id: current_node_id.clone(),
            kind: node.kind().to_string(),
            label: scope_label(node, source),
            owner_type_hint: scope_owner_type_hint(node, source),
        });
    }

    if is_identifier_node(node.kind()) {
        let text = node
            .utf8_text(source.as_bytes())
            .context("read identifier node text")?;
        // Recovery productions can contain zero-width/missing identifier
        // nodes. They are useful as syntax diagnostics but are not names and
        // must never become rename occurrences.
        if text.is_empty() {
            return Ok(());
        }
        // Tokens such as `@selector`, `@protocol`, and `@encode` are
        // Objective-C syntax introducers, not user-owned identifiers. Some
        // recovery paths expose the word as an identifier node.
        if node.start_byte() > 0 && source.as_bytes()[node.start_byte() - 1] == b'@' {
            return Ok(());
        }
        let declaration = if matches!(
            language,
            LanguageId::Cpp | LanguageId::ObjectiveC | LanguageId::ObjectiveCpp
        ) && is_c_family_reserved_word(text)
        {
            None
        } else {
            declaration_info(node, source, rel_path, language, &structural_path)
        };
        let mut declaration_scopes = scopes.clone();
        if declaration.as_ref().is_some_and(|declaration| {
            declaration.exclude_owner_scope
                && scopes
                    .last()
                    .is_some_and(|scope| scope.tree_id == declaration.owner_tree_id)
        }) {
            declaration_scopes.pop();
        }
        if declaration
            .as_ref()
            .is_some_and(|declaration| declaration.exclude_owner_scope)
        {
            // Template parameters live in a synthetic lexical scope, but the
            // templated class/function itself is declared in its surrounding
            // namespace and must remain visible outside the template body.
            declaration_scopes.retain(|scope| scope.kind != "template_declaration");
        }
        if declaration
            .as_ref()
            .is_some_and(|declaration| declaration.kind == "module")
        {
            // Recovery grammars occasionally wrap `namespace name {` in a
            // bogus function/class. Namespace ownership may only inherit real
            // namespace scopes, never those recovery wrappers.
            declaration_scopes.retain(|scope| {
                matches!(
                    scope.kind.as_str(),
                    "translation_unit" | "namespace_definition"
                )
            });
        }
        let owner_type = declaration_scopes
            .iter()
            .rev()
            .find(|scope| is_type_scope(&scope.kind))
            .and_then(|scope| scope.label.clone());
        let constructor_spelling = is_constructor_or_destructor_spelling(node);
        let mut identifier_qualifier = qualifier(node, source);
        if constructor_spelling
            && identifier_qualifier
                .last()
                .is_some_and(|component| component == text)
        {
            identifier_qualifier.pop();
        }
        identifiers.push(RawIdentifier {
            node_id: current_node_id,
            text: text.to_string(),
            range: TextRange::from_node(node),
            declaration,
            lexical_scopes: scopes.clone(),
            declaration_scopes,
            context: reference_context(node, source),
            qualifier: identifier_qualifier,
            owner_type,
            enclosing_type: None,
        });
    }

    let mut cursor = node.walk();
    for (index, child) in node.named_children(&mut cursor).enumerate() {
        let mut child_path = structural_path.clone();
        child_path.push(index);
        collect_nodes(
            child,
            source,
            rel_path,
            language,
            child_path,
            scopes,
            nodes,
            identifiers,
        )?;
    }
    if pushed_scope {
        scopes.pop();
    }
    Ok(())
}

fn node_id(rel_path: &str, node: Node<'_>, structural_path: &[usize]) -> String {
    let path_key = structural_path
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(".");
    stable_id("node", &[rel_path, node.kind(), &path_key])
}

fn is_identifier_node(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "field_identifier"
            | "type_identifier"
            | "namespace_identifier"
            | "method_identifier"
    )
}

fn is_c_family_reserved_word(value: &str) -> bool {
    matches!(
        value,
        "alignas"
            | "alignof"
            | "and"
            | "and_eq"
            | "asm"
            | "atomic_cancel"
            | "atomic_commit"
            | "atomic_noexcept"
            | "auto"
            | "bitand"
            | "bitor"
            | "bool"
            | "break"
            | "case"
            | "catch"
            | "char"
            | "char8_t"
            | "char16_t"
            | "char32_t"
            | "class"
            | "compl"
            | "concept"
            | "const"
            | "consteval"
            | "constexpr"
            | "constinit"
            | "const_cast"
            | "continue"
            | "co_await"
            | "co_return"
            | "co_yield"
            | "decltype"
            | "default"
            | "delete"
            | "do"
            | "double"
            | "dynamic_cast"
            | "else"
            | "enum"
            | "explicit"
            | "export"
            | "extern"
            | "false"
            | "float"
            | "for"
            | "friend"
            | "goto"
            | "if"
            | "inline"
            | "int"
            | "long"
            | "mutable"
            | "namespace"
            | "new"
            | "noexcept"
            | "not"
            | "not_eq"
            | "nullptr"
            | "operator"
            | "or"
            | "or_eq"
            | "private"
            | "protected"
            | "public"
            | "reflexpr"
            | "register"
            | "reinterpret_cast"
            | "requires"
            | "return"
            | "short"
            | "signed"
            | "sizeof"
            | "static"
            | "static_assert"
            | "static_cast"
            | "struct"
            | "switch"
            | "synchronized"
            | "template"
            | "this"
            | "thread_local"
            | "throw"
            | "true"
            | "try"
            | "typedef"
            | "typeid"
            | "typename"
            | "union"
            | "unsigned"
            | "using"
            | "virtual"
            | "void"
            | "volatile"
            | "wchar_t"
            | "while"
            | "xor"
            | "xor_eq"
    )
}

fn is_scope_node(kind: &str) -> bool {
    matches!(
        kind,
        "translation_unit"
            | "template_declaration"
            | "namespace_definition"
            | "class_specifier"
            | "class_declaration"
            | "class_definition"
            | "struct_specifier"
            | "union_specifier"
            | "enum_specifier"
            | "class_interface"
            | "class_implementation"
            | "category_interface"
            | "category_implementation"
            | "protocol_declaration"
            | "function_item"
            | "function_definition"
            | "method_definition"
            | "lambda_expression"
            | "closure_expression"
            | "compound_statement"
            | "block"
            | "for_statement"
            | "for_range_loop"
            | "while_statement"
            | "do_statement"
            | "if_statement"
            | "switch_statement"
            | "catch_clause"
            | "match_expression"
    )
}

fn is_type_scope(kind: &str) -> bool {
    matches!(
        kind,
        "class_specifier"
            | "class_declaration"
            | "class_definition"
            | "struct_specifier"
            | "union_specifier"
            | "class_interface"
            | "class_implementation"
            | "category_interface"
            | "category_implementation"
            | "protocol_declaration"
    )
}

fn scope_label(node: Node<'_>, source: &str) -> Option<String> {
    match node.kind() {
        "translation_unit"
        | "template_declaration"
        | "compound_statement"
        | "block"
        | "for_statement"
        | "for_range_loop"
        | "while_statement"
        | "do_statement"
        | "if_statement"
        | "switch_statement"
        | "catch_clause"
        | "match_expression" => None,
        "namespace_definition"
        | "class_specifier"
        | "class_declaration"
        | "class_definition"
        | "struct_specifier"
        | "union_specifier"
        | "enum_specifier" => node
            .child_by_field_name("name")
            .and_then(|name| node_text(name, source)),
        "class_interface"
        | "class_implementation"
        | "category_interface"
        | "category_implementation"
        | "protocol_declaration" => {
            first_direct_identifier(node).and_then(|name| node_text(name, source))
        }
        "function_item" | "function_definition" => callable_name_node(node)
            .and_then(|name| {
                let mut components = qualifier(name, source);
                components.push(node_text(name, source)?);
                Some(components.join("::"))
            })
            .or_else(|| declaration_name(node, source))
            .map(|name| {
                let signature = callable_info(node, source)
                    .map(|callable| callable.signature)
                    .unwrap_or_else(|| "()".to_string());
                format!("{name}{signature}")
            }),
        "method_definition" => Some(objective_c_selector(node, source)),
        "lambda_expression" | "closure_expression" => Some(format!(
            "<lambda@{}:{}>",
            node.start_position().row + 1,
            node.start_position().column + 1
        )),
        _ => None,
    }
}

fn scope_owner_type_hint(node: Node<'_>, source: &str) -> Option<String> {
    if is_type_scope(node.kind()) {
        return scope_label(node, source);
    }
    if node.kind() == "method_definition" {
        return ancestor_of_kind(
            node,
            &[
                "class_interface",
                "class_implementation",
                "category_interface",
                "category_implementation",
            ],
        )
        .and_then(first_direct_identifier)
        .and_then(|name| node_text(name, source));
    }
    if node.kind() == "function_definition" {
        return callable_name_node(node).and_then(|name| qualifier(name, source).last().cloned());
    }
    None
}

fn declaration_info(
    node: Node<'_>,
    source: &str,
    rel_path: &str,
    language: LanguageId,
    structural_path: &[usize],
) -> Option<DeclarationInfo> {
    if matches!(
        language,
        LanguageId::Cpp | LanguageId::ObjectiveC | LanguageId::ObjectiveCpp
    ) {
        return c_family_declaration(node, source, rel_path, structural_path);
    }
    generic_declaration(node).map(|(kind, owner)| DeclarationInfo {
        kind: kind.to_string(),
        owner_tree_id: owner.id(),
        owner_node_id: owner_node_id(rel_path, owner, structural_path, node),
        exclude_owner_scope: declaration_opens_scope(kind, owner),
        declared_type: declared_type(owner, source),
        initializer: initializer_text(node, owner, source),
        callable: callable_info(owner, source),
        selector: None,
        selector_index: None,
        conditional_group: preprocessor_conditional_group(node),
    })
}

fn c_family_declaration(
    node: Node<'_>,
    source: &str,
    rel_path: &str,
    structural_path: &[usize],
) -> Option<DeclarationInfo> {
    let parent = node.parent()?;

    if is_namespace_declaration_name(node, source) {
        let owner = ancestor_of_kind(node, &["namespace_definition"]).unwrap_or(parent);
        return Some(declaration(
            "module",
            owner,
            source,
            rel_path,
            structural_path,
            node,
        ));
    }

    if parent.kind() == "structured_binding_declarator" && is_direct_child(parent, node) {
        let owner = ancestor_of_kind(parent, &["declaration", "for_range_loop"])?;
        return Some(declaration(
            "variable",
            owner,
            source,
            rel_path,
            structural_path,
            node,
        ));
    }
    if matches!(
        parent.kind(),
        "type_parameter_declaration"
            | "optional_type_parameter_declaration"
            | "variadic_type_parameter_declaration"
            | "template_template_parameter_declaration"
    ) && is_direct_child(parent, node)
    {
        return Some(declaration(
            "type_parameter",
            parent,
            source,
            rel_path,
            structural_path,
            node,
        ));
    }
    if matches!(
        parent.kind(),
        "alias_declaration" | "namespace_alias_definition"
    ) && parent
        .child_by_field_name("name")
        .is_some_and(|name| name.id() == node.id())
    {
        let kind = if parent.kind() == "namespace_alias_definition" {
            "module"
        } else {
            "type"
        };
        return Some(declaration(
            kind,
            parent,
            source,
            rel_path,
            structural_path,
            node,
        ));
    }

    if matches!(parent.kind(), "method_definition" | "method_declaration")
        && is_direct_child(parent, node)
    {
        return Some(declaration(
            "method",
            parent,
            source,
            rel_path,
            structural_path,
            node,
        ));
    }
    if parent.kind() == "method_parameter" && is_parameter_name(node) {
        if let Some(method) = ancestor_of_kind(parent, &["method_definition"]) {
            return Some(DeclarationInfo {
                kind: "parameter".to_string(),
                owner_tree_id: method.id(),
                owner_node_id: owner_node_id(rel_path, method, structural_path, node),
                exclude_owner_scope: false,
                declared_type: declared_type(parent, source),
                initializer: None,
                callable: None,
                selector: None,
                selector_index: None,
                conditional_group: preprocessor_conditional_group(node),
            });
        }
        return None;
    }
    if parent.kind() == "struct_declarator" && is_direct_child(parent, node) {
        if let Some(owner) = parent.parent().filter(|owner| {
            owner.kind() == "struct_declaration"
                && ancestor_of_kind(*owner, &["property_declaration"]).is_some()
        }) {
            return Some(declaration(
                "field",
                owner,
                source,
                rel_path,
                structural_path,
                node,
            ));
        }
    }
    if matches!(
        parent.kind(),
        "class_interface"
            | "class_implementation"
            | "category_interface"
            | "category_implementation"
            | "protocol_declaration"
    ) && first_direct_identifier(parent).is_some_and(|name| name.id() == node.id())
    {
        return Some(declaration(
            "class",
            parent,
            source,
            rel_path,
            structural_path,
            node,
        ));
    }
    if parent.kind() == "enumerator"
        && parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id())
    {
        return Some(declaration(
            "const",
            parent,
            source,
            rel_path,
            structural_path,
            node,
        ));
    }
    if matches!(parent.kind(), "preproc_def" | "preproc_function_def")
        && parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id())
    {
        return Some(declaration(
            "macro",
            parent,
            source,
            rel_path,
            structural_path,
            node,
        ));
    }

    let mut current = node;
    let mut saw_function = false;
    let mut indirection_before_function = false;
    let mut function_entity = false;
    while let Some(wrapper) = current.parent() {
        if !declarator_wrapper_contains(wrapper, current) {
            break;
        }
        if wrapper.kind() == "function_declarator" && !saw_function {
            function_entity = !indirection_before_function;
            saw_function = true;
        } else if matches!(
            wrapper.kind(),
            "pointer_declarator" | "reference_declarator"
        ) && !saw_function
        {
            indirection_before_function = true;
        }
        current = wrapper;
    }
    let owner = current.parent()?;
    if matches!(
        owner.kind(),
        "function_definition"
            | "parameter_declaration"
            | "optional_parameter_declaration"
            | "declaration"
            | "field_declaration"
            | "type_definition"
            | "alias_declaration"
            | "for_range_loop"
            | "catch_clause"
            | "condition_clause"
            | "struct_declaration"
    ) && !is_field_child(owner, "declarator", current)
    {
        return None;
    }
    if saw_function
        && matches!(owner.kind(), "function_definition" | "declaration")
        && owner.child_by_field_name("type").is_none()
    {
        // A constructor/destructor spelling belongs to the class identity;
        // it cannot be renamed independently from that class.
        return None;
    }
    let kind = match owner.kind() {
        "function_definition" => "function",
        "parameter_declaration" | "optional_parameter_declaration" => {
            // Named prototype parameters are projectable API spellings too.
            // They remain distinct syntax symbols until clangd links them to
            // a definition parameter during approval.
            if ancestor_of_kind(owner, &["template_parameter_list"]).is_some() {
                "template_parameter"
            } else {
                "parameter"
            }
        }
        "declaration" => {
            if function_entity && ancestor_of_kind(owner, &["compound_statement"]).is_none() {
                "function"
            } else {
                "variable"
            }
        }
        "field_declaration" => {
            if function_entity {
                "method"
            } else {
                "field"
            }
        }
        "type_definition" | "alias_declaration" => "type",
        "for_range_loop" | "catch_clause" | "condition_clause" => "variable",
        "struct_declaration" if ancestor_of_kind(owner, &["property_declaration"]).is_some() => {
            "field"
        }
        _ => {
            let name_matches = owner
                .child_by_field_name("name")
                .is_some_and(|name| name.id() == current.id());
            if !name_matches {
                return None;
            }
            match owner.kind() {
                "namespace_definition" => "module",
                "class_specifier" | "class_declaration" | "class_definition" => "class",
                "struct_specifier" => "struct",
                "union_specifier" => "union",
                "enum_specifier" => "enum",
                _ => return None,
            }
        }
    };
    Some(declaration(
        kind,
        owner,
        source,
        rel_path,
        structural_path,
        node,
    ))
}

fn is_namespace_declaration_name(node: Node<'_>, source: &str) -> bool {
    let prefix = &source.as_bytes()[..node.start_byte()];
    let mut end = prefix.len();
    while end > 0 && prefix[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && (prefix[start - 1] == b'_' || prefix[start - 1].is_ascii_alphanumeric()) {
        start -= 1;
    }
    if prefix.get(start..end) != Some(b"namespace") {
        return false;
    }
    let before_namespace = &prefix[..start];
    let mut previous_end = before_namespace.len();
    while previous_end > 0 && before_namespace[previous_end - 1].is_ascii_whitespace() {
        previous_end -= 1;
    }
    let mut previous_start = previous_end;
    while previous_start > 0
        && (before_namespace[previous_start - 1] == b'_'
            || before_namespace[previous_start - 1].is_ascii_alphanumeric())
    {
        previous_start -= 1;
    }
    before_namespace.get(previous_start..previous_end) != Some(b"using")
}

fn declaration(
    kind: &str,
    owner: Node<'_>,
    source: &str,
    rel_path: &str,
    structural_path: &[usize],
    original: Node<'_>,
) -> DeclarationInfo {
    let objective_c_identity = objective_c_method_identity(owner, source);
    let callable = objective_c_identity
        .as_ref()
        .map(|identity| CallableInfo {
            min_arity: identity.arity,
            max_arity: Some(identity.arity),
            signature: identity.selector.clone(),
            parameter_types: Vec::new(),
        })
        .or_else(|| callable_info(owner, source));
    DeclarationInfo {
        kind: kind.to_string(),
        owner_tree_id: owner.id(),
        owner_node_id: owner_node_id(rel_path, owner, structural_path, original),
        exclude_owner_scope: declaration_opens_scope(kind, owner),
        declared_type: declared_type(owner, source),
        initializer: initializer_text(original, owner, source),
        callable,
        selector: objective_c_identity
            .as_ref()
            .map(|identity| identity.selector.clone()),
        selector_index: objective_c_identity
            .and_then(|identity| identity.selector_indexes.get(&original.id()).copied()),
        conditional_group: preprocessor_conditional_group(original),
    }
}

fn preprocessor_conditional_group(mut node: Node<'_>) -> Option<String> {
    while let Some(parent) = node.parent() {
        if matches!(
            parent.kind(),
            "preproc_if" | "preproc_ifdef" | "preproc_ifndef"
        ) {
            return Some(format!("{}:{}", parent.start_byte(), parent.end_byte()));
        }
        node = parent;
    }
    None
}

fn declaration_opens_scope(kind: &str, owner: Node<'_>) -> bool {
    is_scope_node(owner.kind())
        && matches!(
            kind,
            "function" | "method" | "class" | "struct" | "union" | "enum" | "trait" | "module"
        )
}

fn is_field_child(parent: Node<'_>, field: &str, child: Node<'_>) -> bool {
    parent
        .children_by_field_name(field, &mut parent.walk())
        .any(|candidate| candidate.id() == child.id())
}

fn owner_node_id(
    rel_path: &str,
    owner: Node<'_>,
    identifier_path: &[usize],
    identifier: Node<'_>,
) -> String {
    let mut path = identifier_path.to_vec();
    let mut current = identifier;
    while current.id() != owner.id() {
        path.pop();
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent;
    }
    node_id(rel_path, owner, &path)
}

fn declarator_wrapper_contains(wrapper: Node<'_>, child: Node<'_>) -> bool {
    let kind = wrapper.kind();
    let field_match = wrapper
        .child_by_field_name("declarator")
        .is_some_and(|declarator| declarator.id() == child.id());
    match kind {
        "pointer_declarator"
        | "reference_declarator"
        | "array_declarator"
        | "function_declarator"
        | "parenthesized_declarator"
        | "attributed_declarator"
        | "struct_declarator"
        | "init_declarator" => {
            field_match
                || wrapper
                    .named_child(0)
                    .is_some_and(|candidate| candidate.id() == child.id())
        }
        "qualified_identifier" | "scoped_identifier" => wrapper
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == child.id()),
        "template_function" => wrapper
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == child.id()),
        "destructor_name" => is_direct_child(wrapper, child),
        _ => false,
    }
}

fn generic_declaration(node: Node<'_>) -> Option<(&'static str, Node<'_>)> {
    let parent = node.parent()?;
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
        let kind = match parent.kind() {
            "function_declarator" => "function",
            "init_declarator" | "declaration" => "variable",
            "parameter_declaration" | "optional_parameter_declaration" => "parameter",
            "type_definition" => "type",
            _ => return None,
        };
        return Some((kind, parent));
    }
    if pattern_matches {
        let kind = match parent.kind() {
            "let_declaration" => "variable",
            "parameter" | "closure_parameters" => "parameter",
            _ => return None,
        };
        return Some((kind, parent));
    }
    if !name_matches {
        return None;
    }
    let kind = match parent.kind() {
        "function_item" | "function_declaration" | "function_definition" => "function",
        "struct_item" | "struct_specifier" => "struct",
        "class_specifier" | "class_declaration" | "class_definition" => "class",
        "enum_item" | "enum_specifier" => "enum",
        "union_item" | "union_specifier" => "union",
        "trait_item" => "trait",
        "mod_item" | "namespace_definition" => "module",
        "type_item" | "type_definition" => "type",
        "const_item" => "const",
        "static_item" => "static",
        "field_declaration" => "field",
        "variable_declarator" | "var_spec" => "variable",
        "required_parameter" | "optional_parameter" | "typed_parameter" => "parameter",
        "macro_definition" => "macro",
        _ => return None,
    };
    Some((kind, parent))
}

fn resolve_reference(
    raw: &RawIdentifier,
    candidates_by_name: &BTreeMap<String, Vec<BindingCandidate>>,
) -> Resolution {
    if raw.context == ReferenceContext::ExternalOnly {
        return Resolution::External;
    }
    let Some(all_named) = candidates_by_name.get(&raw.text) else {
        return Resolution::External;
    };
    let reference_scopes = raw
        .lexical_scopes
        .iter()
        .map(|scope| scope.node_id.as_str())
        .collect::<Vec<_>>();

    let mut candidates = match &raw.context {
        ReferenceContext::Member {
            receiver,
            call_arity,
            call_arguments: _,
        } => member_candidates(raw, receiver, *call_arity, all_named, candidates_by_name),
        ReferenceContext::ObjectiveCMethod {
            receiver,
            selector: _,
            selector_index: _,
            arity,
        } => member_candidates(raw, receiver, Some(*arity), all_named, candidates_by_name),
        _ if !raw.qualifier.is_empty() => all_named.to_vec(),
        _ => all_named
            .iter()
            .filter(|candidate| is_scope_prefix(&candidate.scopes, &reference_scopes))
            .cloned()
            .collect::<Vec<_>>(),
    };
    if matches!(
        raw.context,
        ReferenceContext::Value | ReferenceContext::TypeOrValue | ReferenceContext::Call { .. }
    ) {
        if let Some(enclosing_type) = raw.enclosing_type.as_deref() {
            candidates.extend(
                all_named
                    .iter()
                    .filter(|candidate| {
                        candidate
                            .owner_type
                            .as_deref()
                            .map(base_type_name)
                            .is_some_and(|owner| owner == base_type_name(enclosing_type))
                    })
                    .cloned(),
            );
        }
    }
    if candidates.is_empty() {
        return if all_named.is_empty() {
            Resolution::External
        } else {
            // Same spelling exists, but not in a scope/type visible here. It
            // is unrelated ownership evidence and must not poison that symbol.
            Resolution::External
        };
    }
    candidates.retain(|candidate| context_accepts(&raw.context, &candidate.kind));
    if candidates.is_empty() {
        return Resolution::External;
    }
    if !raw.qualifier.is_empty() {
        candidates.retain(|candidate| labels_end_with(&candidate.scope_labels, &raw.qualifier));
        if candidates.is_empty() {
            return Resolution::External;
        }
    }
    if let ReferenceContext::ObjectiveCMethod {
        selector,
        selector_index,
        ..
    } = &raw.context
    {
        candidates.retain(|candidate| {
            candidate.selector.as_deref() == Some(selector.as_str())
                && candidate.selector_index == Some(*selector_index)
        });
        if candidates.is_empty() {
            return Resolution::External;
        }
    }

    let call_arity = match raw.context {
        ReferenceContext::Call { arity, .. } => Some(arity),
        ReferenceContext::Member { call_arity, .. } => call_arity,
        ReferenceContext::ObjectiveCMethod { arity, .. } => Some(arity),
        _ => None,
    };
    if let Some(arity) = call_arity {
        let exact = candidates
            .iter()
            .filter(|candidate| {
                candidate
                    .callable
                    .as_ref()
                    .is_some_and(|callable| callable_accepts_arity(callable, arity))
            })
            .cloned()
            .collect::<Vec<_>>();
        if !exact.is_empty() {
            candidates = exact;
        }
    }
    let call_arguments = match &raw.context {
        ReferenceContext::Call { arguments, .. }
        | ReferenceContext::Member {
            call_arguments: arguments,
            ..
        } => Some(arguments.as_slice()),
        _ => None,
    };
    if candidates.len() > 1 {
        if let Some(arguments) = call_arguments {
            let scored = candidates
                .iter()
                .filter_map(|candidate| {
                    overload_score(raw, candidate, arguments, candidates_by_name)
                        .map(|score| (candidate.symbol_id.clone(), score))
                })
                .collect::<Vec<_>>();
            if let Some(best_score) = scored.iter().map(|(_, score)| *score).max() {
                let best_ids = scored
                    .iter()
                    .filter(|(_, score)| *score == best_score)
                    .map(|(symbol_id, _)| symbol_id.clone())
                    .collect::<BTreeSet<_>>();
                if best_score > 0 && best_ids.len() == 1 {
                    candidates.retain(|candidate| best_ids.contains(&candidate.symbol_id));
                }
            }
        }
    }

    let deepest = candidates
        .iter()
        .map(|candidate| candidate.scopes.len())
        .max()
        .unwrap_or_default();
    candidates.retain(|candidate| candidate.scopes.len() == deepest);

    if matches!(raw.context, ReferenceContext::Value) {
        let preceding = candidates
            .iter()
            .filter(|candidate| candidate.start_byte <= raw.range.start_byte)
            .map(|candidate| candidate.start_byte)
            .max();
        if let Some(preceding) = preceding {
            candidates.retain(|candidate| candidate.start_byte == preceding);
        }
    }
    candidates.sort_by(|left, right| left.symbol_id.cmp(&right.symbol_id));
    candidates.dedup_by(|left, right| left.symbol_id == right.symbol_id);
    match candidates.as_slice() {
        [candidate] => Resolution::Bound(candidate.symbol_id.clone()),
        [] => Resolution::External,
        _ => Resolution::Ambiguous,
    }
}

fn member_candidates(
    raw: &RawIdentifier,
    receiver: &str,
    call_arity: Option<usize>,
    all_named: &[BindingCandidate],
    candidates_by_name: &BTreeMap<String, Vec<BindingCandidate>>,
) -> Vec<BindingCandidate> {
    let receiver_type = if matches!(receiver, "this" | "self" | "super") {
        raw.enclosing_type.clone()
    } else {
        visible_value_candidate(raw, receiver, candidates_by_name)
            .and_then(|candidate| {
                candidate.declared_type.clone().or_else(|| {
                    candidate.initializer.as_deref().and_then(|initializer| {
                        infer_expression_type(raw, initializer, candidates_by_name)
                    })
                })
            })
            .or_else(|| infer_expression_type(raw, receiver, candidates_by_name))
    };
    let Some(receiver_type) = receiver_type.map(|value| effective_receiver_type(&value)) else {
        return Vec::new();
    };
    all_named
        .iter()
        .filter(|candidate| {
            candidate
                .owner_type
                .as_deref()
                .map(base_type_name)
                .is_some_and(|owner| owner == receiver_type)
                && call_arity.is_none_or(|arity| {
                    candidate
                        .callable
                        .as_ref()
                        .is_none_or(|callable| callable_accepts_arity(callable, arity))
                })
        })
        .cloned()
        .collect()
}

fn overload_score(
    raw: &RawIdentifier,
    candidate: &BindingCandidate,
    arguments: &[CallArgument],
    candidates_by_name: &BTreeMap<String, Vec<BindingCandidate>>,
) -> Option<i32> {
    let callable = candidate.callable.as_ref()?;
    if callable.parameter_types.len() < arguments.len() {
        return None;
    }
    let mut score = 0i32;
    let mut evidence = 0usize;
    for (argument, parameter) in arguments.iter().zip(&callable.parameter_types) {
        let Some(argument_type) = infer_call_argument_type(raw, argument, candidates_by_name)
        else {
            continue;
        };
        score += type_compatibility_score(&argument_type, parameter)?;
        evidence += 1;
    }
    (evidence > 0).then_some(score)
}

fn infer_call_argument_type(
    raw: &RawIdentifier,
    argument: &CallArgument,
    candidates_by_name: &BTreeMap<String, Vec<BindingCandidate>>,
) -> Option<String> {
    match argument.syntax_kind.as_str() {
        "number_literal" => {
            let lower = argument.text.to_ascii_lowercase();
            if lower.contains('.') || lower.contains('e') {
                Some(if lower.ends_with('f') {
                    "float-literal".to_string()
                } else {
                    "double-literal".to_string()
                })
            } else {
                Some("int-literal".to_string())
            }
        }
        "string_literal" | "concatenated_string" => Some("string-literal".to_string()),
        "char_literal" => Some("char".to_string()),
        "true" | "false" => Some("bool".to_string()),
        "null" | "nullptr" => Some("nullptr".to_string()),
        "identifier" => {
            visible_value_candidate(raw, &argument.text, candidates_by_name).and_then(|candidate| {
                candidate.declared_type.or_else(|| {
                    candidate.initializer.as_deref().and_then(|initializer| {
                        infer_expression_type(raw, initializer, candidates_by_name)
                    })
                })
            })
        }
        "call_expression" | "field_expression" => {
            infer_expression_type(raw, &argument.text, candidates_by_name)
        }
        _ => None,
    }
}

fn type_compatibility_score(argument: &str, parameter: &str) -> Option<i32> {
    let argument = normalize_type_for_matching(argument);
    let parameter = normalize_type_for_matching(parameter);
    if argument == parameter {
        return Some(100);
    }
    match argument.as_str() {
        "int-literal" => {
            if is_integral_type(&parameter) {
                Some(if parameter == "int" { 90 } else { 70 })
            } else if is_floating_type(&parameter) {
                Some(30)
            } else {
                None
            }
        }
        "float-literal" => {
            is_floating_type(&parameter).then_some(if parameter == "float" { 90 } else { 70 })
        }
        "double-literal" => {
            is_floating_type(&parameter).then_some(if parameter == "double" { 90 } else { 70 })
        }
        "string-literal" => (parameter.contains("char*")
            || parameter.ends_with("string")
            || parameter.ends_with("string_view")
            || parameter.ends_with("NSString*"))
        .then_some(80),
        "nullptr" => parameter.contains('*').then_some(70),
        _ if is_integral_type(&argument) && is_integral_type(&parameter) => Some(40),
        _ if is_floating_type(&argument) && is_floating_type(&parameter) => Some(40),
        _ => {
            let argument_base = base_type_name(&argument);
            let parameter_base = base_type_name(&parameter);
            (argument_base == parameter_base
                && argument.matches('*').count() == parameter.matches('*').count())
            .then_some(80)
        }
    }
}

fn normalize_type_for_matching(value: &str) -> String {
    value
        .replace("const", "")
        .replace("volatile", "")
        .replace("&&", "")
        .replace('&', "")
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn is_integral_type(value: &str) -> bool {
    matches!(
        value,
        "bool"
            | "char"
            | "signedchar"
            | "unsignedchar"
            | "short"
            | "unsignedshort"
            | "int"
            | "unsigned"
            | "unsignedint"
            | "long"
            | "unsignedlong"
            | "longlong"
            | "unsignedlonglong"
            | "size_t"
            | "NSInteger"
            | "NSUInteger"
    )
}

fn is_floating_type(value: &str) -> bool {
    matches!(value, "float" | "double" | "longdouble" | "CGFloat")
}

fn infer_expression_type(
    raw: &RawIdentifier,
    expression: &str,
    candidates_by_name: &BTreeMap<String, Vec<BindingCandidate>>,
) -> Option<String> {
    let expression = expression
        .trim()
        .trim_matches(|character| matches!(character, '(' | ')' | '{' | '}' | '&' | '*'));
    if expression.is_empty() {
        return None;
    }
    if expression
        .chars()
        .all(|character| character == '_' || character.is_ascii_alphanumeric())
    {
        return visible_value_candidate(raw, expression, candidates_by_name)
            .and_then(|candidate| candidate.declared_type);
    }

    let callee = expression
        .split_once('(')
        .map_or(expression, |(callee, _)| callee)
        .trim();
    if let Some((receiver, member)) = split_last_member_access(callee) {
        let receiver_type =
            infer_expression_type(raw, receiver, candidates_by_name).or_else(|| {
                visible_value_candidate(raw, receiver, candidates_by_name)
                    .and_then(|candidate| candidate.declared_type)
            })?;
        return candidates_by_name.get(member).and_then(|candidates| {
            candidates
                .iter()
                .filter(|candidate| {
                    candidate
                        .owner_type
                        .as_deref()
                        .map(base_type_name)
                        .is_some_and(|owner| owner == effective_receiver_type(&receiver_type))
                })
                .filter_map(|candidate| candidate.declared_type.clone())
                .next()
        });
    }

    let components = callee
        .split("::")
        .filter(|component| !component.is_empty())
        .map(base_type_name)
        .collect::<Vec<_>>();
    let name = components.last()?.as_str();
    let qualifier = &components[..components.len().saturating_sub(1)];
    let candidates = candidates_by_name.get(name)?;
    let mut matches = candidates
        .iter()
        .filter(|candidate| {
            qualifier.is_empty() || labels_end_with(&candidate.scope_labels, qualifier)
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|candidate| usize::from(candidate.declared_type.is_none()));
    if let Some(declared_type) = matches
        .iter()
        .find_map(|candidate| candidate.declared_type.clone())
    {
        return Some(declared_type);
    }
    matches
        .iter()
        .find(|candidate| matches!(candidate.kind.as_str(), "class" | "struct" | "type"))
        .map(|_| base_type_name(name))
}

fn split_last_member_access(value: &str) -> Option<(&str, &str)> {
    let dot = value.rfind('.').map(|index| (index, 1));
    let arrow = value.rfind("->").map(|index| (index, 2));
    let (index, width) = match (dot, arrow) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (left @ Some(_), None) | (None, left @ Some(_)) => left,
        (None, None) => None,
    }?;
    let receiver = value[..index].trim();
    let member = value[index + width..].trim();
    (!receiver.is_empty() && !member.is_empty()).then_some((receiver, member))
}

fn visible_value_candidate(
    raw: &RawIdentifier,
    name: &str,
    candidates_by_name: &BTreeMap<String, Vec<BindingCandidate>>,
) -> Option<BindingCandidate> {
    let reference_scopes = raw
        .lexical_scopes
        .iter()
        .map(|scope| scope.node_id.as_str())
        .collect::<Vec<_>>();
    let mut candidates = candidates_by_name
        .get(name)?
        .iter()
        .filter(|candidate| {
            is_scope_prefix(&candidate.scopes, &reference_scopes)
                && matches!(candidate.kind.as_str(), "variable" | "parameter" | "field")
                && candidate.start_byte <= raw.range.start_byte
        })
        .cloned()
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .scopes
            .len()
            .cmp(&left.scopes.len())
            .then_with(|| right.start_byte.cmp(&left.start_byte))
    });
    let first = candidates.first()?.clone();
    let ambiguous = candidates.iter().skip(1).any(|candidate| {
        candidate.scopes.len() == first.scopes.len()
            && candidate.start_byte == first.start_byte
            && candidate.symbol_id != first.symbol_id
    });
    (!ambiguous).then_some(first)
}

fn context_accepts(context: &ReferenceContext, kind: &str) -> bool {
    match context {
        ReferenceContext::ExternalOnly => false,
        ReferenceContext::Type => matches!(
            kind,
            "class" | "struct" | "union" | "enum" | "type" | "trait" | "type_parameter"
        ),
        ReferenceContext::TypeOrValue => true,
        ReferenceContext::Qualifier => matches!(
            kind,
            "module" | "class" | "struct" | "union" | "enum" | "type" | "trait"
        ),
        ReferenceContext::Call { .. } => {
            matches!(
                kind,
                "function"
                    | "method"
                    | "macro"
                    | "variable"
                    | "field"
                    | "class"
                    | "struct"
                    | "type"
            )
        }
        ReferenceContext::Member { call_arity, .. } => {
            if call_arity.is_some() {
                matches!(kind, "function" | "method" | "field")
            } else {
                matches!(kind, "field" | "variable" | "method")
            }
        }
        ReferenceContext::ObjectiveCMethod { .. } => kind == "method",
        ReferenceContext::Value => !matches!(
            kind,
            "class" | "struct" | "union" | "enum" | "type" | "trait" | "type_parameter" | "module"
        ),
    }
}

fn is_scope_prefix(candidate: &[String], reference: &[&str]) -> bool {
    candidate.len() <= reference.len()
        && candidate
            .iter()
            .zip(reference)
            .all(|(candidate, reference)| candidate == reference)
}

fn labels_end_with(labels: &[String], qualifier: &[String]) -> bool {
    qualifier.len() <= labels.len()
        && labels[labels.len() - qualifier.len()..]
            .iter()
            .zip(qualifier)
            .all(|(left, right)| base_type_name(left) == base_type_name(right))
}

fn qualified_name(raw: &RawIdentifier) -> String {
    let mut labels = raw
        .declaration_scopes
        .iter()
        .filter_map(|scope| scope.label.clone())
        .collect::<Vec<_>>();
    append_qualified_components(&mut labels, &raw.qualifier);
    labels.push(raw.text.clone());
    labels.join("::")
}

fn declaration_group_key(raw: &RawIdentifier, qualified_name: &str) -> Option<String> {
    let declaration = raw.declaration.as_ref()?;
    match declaration.kind.as_str() {
        "class" | "struct" | "union" | "enum" | "type" | "trait" | "module" => {
            Some(format!("{}\0{qualified_name}", declaration.kind))
        }
        "function" | "method" => declaration.callable.as_ref().map(|callable| {
            format!(
                "{}\0{qualified_name}\0{}\0{}\0{}",
                declaration.kind,
                callable.signature,
                declaration.selector.as_deref().unwrap_or_default(),
                declaration.selector_index.unwrap_or_default(),
            )
        }),
        _ => declaration.conditional_group.as_ref().map(|conditional| {
            format!(
                "conditional\0{}\0{qualified_name}\0{conditional}",
                declaration.kind
            )
        }),
    }
}

fn append_qualified_components(labels: &mut Vec<String>, qualifier: &[String]) {
    let overlap = (0..=labels.len().min(qualifier.len()))
        .rev()
        .find(|length| {
            labels[labels.len() - *length..]
                .iter()
                .zip(&qualifier[..*length])
                .all(|(left, right)| base_type_name(left) == base_type_name(right))
        })
        .unwrap_or_default();
    labels.extend(qualifier[overlap..].iter().cloned());
}

fn reference_context(node: Node<'_>, source: &str) -> ReferenceContext {
    if node.parent().is_some_and(|parent| {
        parent.kind() == "method_parameter"
            && is_parameter_name(node)
            && ancestor_of_kind(parent, &["method_definition"]).is_none()
    }) {
        return ReferenceContext::ExternalOnly;
    }
    if is_constructor_or_destructor_spelling(node) {
        return ReferenceContext::Type;
    }
    if node.kind() == "type_identifier" {
        return if is_vexing_parse_initializer_argument(node) {
            ReferenceContext::TypeOrValue
        } else {
            ReferenceContext::Type
        };
    }
    if node.kind() == "namespace_identifier" {
        return ReferenceContext::Qualifier;
    }
    if let Some(parent) = node.parent() {
        if parent.kind() == "message_expression"
            && parent
                .children_by_field_name("method", &mut parent.walk())
                .any(|method| method.id() == node.id())
        {
            let receiver = parent
                .child_by_field_name("receiver")
                .and_then(|receiver| node_text(receiver, source))
                .unwrap_or_default();
            let methods = parent
                .children_by_field_name("method", &mut parent.walk())
                .collect::<Vec<_>>();
            let selector_index = methods
                .iter()
                .position(|method| method.id() == node.id())
                .unwrap_or_default();
            let arity = message_argument_count(parent);
            let selector = methods
                .iter()
                .filter_map(|method| node_text(*method, source))
                .collect::<Vec<_>>()
                .join(":");
            let selector = if arity == 0 {
                selector
            } else {
                format!("{selector}:")
            };
            return ReferenceContext::ObjectiveCMethod {
                receiver,
                selector,
                selector_index,
                arity,
            };
        }
        if parent.kind() == "field_expression"
            && parent
                .child_by_field_name("field")
                .is_some_and(|field| field.id() == node.id())
        {
            let receiver = parent
                .child_by_field_name("argument")
                .and_then(|argument| node_text(argument, source))
                .unwrap_or_default();
            // Only the terminal field in `receiver.member(...)` is the
            // callee. Treating identifiers inside the receiver as calls made
            // ordinary locals such as `stream` or `items` look like
            // functions and left them spuriously unresolved.
            let call_arity = enclosing_call(parent).and_then(function_call_arity);
            let call_arguments = enclosing_call(parent)
                .map(|call| function_call_arguments(call, source))
                .unwrap_or_default();
            return ReferenceContext::Member {
                receiver,
                call_arity,
                call_arguments,
            };
        }
        if parent.kind() == "field_expression"
            && parent
                .child_by_field_name("argument")
                .is_some_and(|argument| argument.id() == node.id())
        {
            return ReferenceContext::Value;
        }
    }
    if let Some(call) = enclosing_call(node) {
        return ReferenceContext::Call {
            arity: function_call_arity(call).unwrap_or_default(),
            arguments: function_call_arguments(call, source),
        };
    }
    ReferenceContext::Value
}

fn is_vexing_parse_initializer_argument(node: Node<'_>) -> bool {
    let Some(parameter) = ancestor_of_kind(
        node,
        &["parameter_declaration", "optional_parameter_declaration"],
    ) else {
        return false;
    };
    let Some(parameters) = parameter
        .parent()
        .filter(|parent| parent.kind() == "parameter_list")
    else {
        return false;
    };
    let Some(declarator) = parameters
        .parent()
        .filter(|parent| parent.kind() == "function_declarator")
    else {
        return false;
    };
    let Some(declaration) = ancestor_of_kind(declarator, &["declaration"]) else {
        return false;
    };
    ancestor_of_kind(declaration, &["compound_statement"]).is_some()
}

fn enclosing_call(node: Node<'_>) -> Option<Node<'_>> {
    let mut current = node;
    while let Some(parent) = current.parent() {
        if parent.kind() == "call_expression" {
            let function = parent.child_by_field_name("function")?;
            return (node.start_byte() >= function.start_byte()
                && node.end_byte() <= function.end_byte())
            .then_some(parent);
        }
        if !matches!(
            parent.kind(),
            "qualified_identifier" | "scoped_identifier" | "template_function"
        ) {
            break;
        }
        current = parent;
    }
    None
}

fn function_call_arity(call: Node<'_>) -> Option<usize> {
    let arguments = call.child_by_field_name("arguments")?;
    let mut cursor = arguments.walk();
    Some(arguments.named_children(&mut cursor).count())
}

fn function_call_arguments(call: Node<'_>, source: &str) -> Vec<CallArgument> {
    let Some(arguments) = call.child_by_field_name("arguments") else {
        return Vec::new();
    };
    let mut cursor = arguments.walk();
    arguments
        .named_children(&mut cursor)
        .filter_map(|argument| {
            Some(CallArgument {
                text: node_text(argument, source)?,
                syntax_kind: argument.kind().to_string(),
            })
        })
        .collect()
}

fn message_argument_count(message: Node<'_>) -> usize {
    let receiver_id = message
        .child_by_field_name("receiver")
        .map(|node| node.id());
    let mut cursor = message.walk();
    let method_ids = message
        .children_by_field_name("method", &mut cursor)
        .map(|node| node.id())
        .collect::<BTreeSet<_>>();
    let mut cursor = message.walk();
    message
        .named_children(&mut cursor)
        .filter(|child| Some(child.id()) != receiver_id && !method_ids.contains(&child.id()))
        .count()
}

fn callable_info(node: Node<'_>, source: &str) -> Option<CallableInfo> {
    let declarator = if node.kind() == "function_declarator" {
        node
    } else {
        find_descendant(node, |candidate| candidate.kind() == "function_declarator")?
    };
    let parameters = declarator.child_by_field_name("parameters")?;
    let mut cursor = parameters.walk();
    let children = parameters.named_children(&mut cursor).collect::<Vec<_>>();
    let variadic = children
        .iter()
        .any(|child| child.kind() == "variadic_parameter");
    let parameters = children
        .into_iter()
        .filter(|child| child.kind() != "variadic_parameter")
        .filter(|child| !is_void_only_parameter(*child, source))
        .collect::<Vec<_>>();
    let min_arity = parameters
        .iter()
        .filter(|parameter| {
            parameter.kind() != "optional_parameter_declaration"
                && parameter.child_by_field_name("default_value").is_none()
        })
        .count();
    let max_arity = (!variadic).then_some(parameters.len());
    let parameter_signature = parameters
        .iter()
        .map(|parameter| canonical_parameter(*parameter, source))
        .collect::<Vec<_>>()
        .join(",");
    let mut cursor = declarator.walk();
    let suffix = declarator
        .named_children(&mut cursor)
        .filter(|child| child.start_byte() >= parameters_end_byte(declarator))
        .filter(|child| child.kind() != "parameter_list")
        .filter_map(|child| node_text(child, source))
        .map(|text| normalize_signature_text(&text))
        .collect::<Vec<_>>()
        .join("");
    Some(CallableInfo {
        min_arity,
        max_arity,
        signature: format!("({parameter_signature}){suffix}"),
        parameter_types: parameters
            .iter()
            .map(|parameter| canonical_parameter(*parameter, source))
            .collect(),
    })
}

fn callable_accepts_arity(callable: &CallableInfo, arity: usize) -> bool {
    arity >= callable.min_arity && callable.max_arity.is_none_or(|maximum| arity <= maximum)
}

fn parameters_end_byte(declarator: Node<'_>) -> usize {
    declarator
        .child_by_field_name("parameters")
        .map_or(declarator.end_byte(), |parameters| parameters.end_byte())
}

fn is_void_only_parameter(parameter: Node<'_>, source: &str) -> bool {
    parameter.child_by_field_name("declarator").is_none()
        && parameter
            .child_by_field_name("type")
            .and_then(|kind| node_text(kind, source))
            .is_some_and(|kind| kind.trim() == "void")
}

fn canonical_parameter(parameter: Node<'_>, source: &str) -> String {
    let Ok(text) = parameter.utf8_text(source.as_bytes()) else {
        return String::new();
    };
    let mut removals = Vec::<(usize, usize)>::new();
    if let Some(default) = parameter.child_by_field_name("default_value") {
        removals.push((
            default.start_byte().saturating_sub(parameter.start_byte()),
            default.end_byte().saturating_sub(parameter.start_byte()),
        ));
    }
    if let Some(name) = parameter
        .child_by_field_name("declarator")
        .and_then(declarator_name_node)
    {
        removals.push((
            name.start_byte().saturating_sub(parameter.start_byte()),
            name.end_byte().saturating_sub(parameter.start_byte()),
        ));
    }
    removals.sort_unstable();
    let mut canonical = text.to_string();
    for (start, end) in removals.into_iter().rev() {
        if start <= end && end <= canonical.len() {
            canonical.replace_range(start..end, "");
        }
    }
    if let Some(default_start) = canonical.find('=') {
        canonical.truncate(default_start);
    }
    normalize_signature_text(&canonical)
}

fn declarator_name_node(node: Node<'_>) -> Option<Node<'_>> {
    if matches!(
        node.kind(),
        "identifier" | "field_identifier" | "type_identifier"
    ) {
        return Some(node);
    }
    for field in ["declarator", "name"] {
        if let Some(child) = node.child_by_field_name(field) {
            if let Some(name) = declarator_name_node(child) {
                return Some(name);
            }
        }
    }
    None
}

fn normalize_signature_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn callable_name_node(node: Node<'_>) -> Option<Node<'_>> {
    let declarator = if node.kind() == "function_declarator" {
        node
    } else {
        find_descendant(node, |candidate| candidate.kind() == "function_declarator")?
    };
    declarator
        .child_by_field_name("declarator")
        .and_then(declarator_name_node)
}

fn is_constructor_or_destructor_spelling(node: Node<'_>) -> bool {
    let mut current = node;
    let mut saw_function = false;
    loop {
        let Some(parent) = current.parent() else {
            return false;
        };
        if !declarator_wrapper_contains(parent, current) {
            return false;
        }
        saw_function |= parent.kind() == "function_declarator";
        current = parent;
        if saw_function {
            let Some(owner) = current.parent() else {
                return false;
            };
            if matches!(owner.kind(), "function_definition" | "declaration") {
                return owner.child_by_field_name("type").is_none();
            }
        }
    }
}

fn qualifier(node: Node<'_>, source: &str) -> Vec<String> {
    let mut current = node;
    let mut components = Vec::<String>::new();
    while let Some(parent) = current.parent() {
        if matches!(parent.kind(), "qualified_identifier" | "scoped_identifier") {
            if parent
                .child_by_field_name("name")
                .is_some_and(|name| current.start_byte() >= name.start_byte())
            {
                let mut prefix = parent
                    .child_by_field_name("scope")
                    .and_then(|scope| node_text(scope, source))
                    .unwrap_or_default()
                    .split("::")
                    .filter(|part| !part.is_empty())
                    .map(base_type_name)
                    .collect::<Vec<_>>();
                prefix.extend(components);
                components = prefix;
            }
            current = parent;
            continue;
        }
        if !matches!(
            parent.kind(),
            "template_function" | "template_type" | "destructor_name"
        ) {
            break;
        }
        current = parent;
    }
    components
}

fn declared_type(owner: Node<'_>, source: &str) -> Option<String> {
    owner
        .child_by_field_name("type")
        .and_then(|node| node_text(node, source))
        .or_else(|| {
            find_descendant(owner, |candidate| {
                matches!(candidate.kind(), "type_identifier" | "primitive_type")
            })
            .and_then(|node| node_text(node, source))
        })
        .map(|value| normalize_type_text(&value))
        .filter(|value| !matches!(base_type_name(value).as_str(), "auto" | "decltype"))
}

fn initializer_text(identifier: Node<'_>, owner: Node<'_>, source: &str) -> Option<String> {
    let mut current = identifier;
    loop {
        if matches!(
            current.kind(),
            "init_declarator" | "variable_declarator" | "let_declaration"
        ) {
            return current
                .child_by_field_name("value")
                .and_then(|value| node_text(value, source));
        }
        if current.id() == owner.id() {
            break;
        }
        current = current.parent()?;
    }
    owner
        .child_by_field_name("value")
        .and_then(|value| node_text(value, source))
}

fn base_type_name(value: &str) -> String {
    let before_template = value.split('<').next().unwrap_or(value);
    let unqualified = before_template
        .rsplit("::")
        .next()
        .unwrap_or(before_template);
    unqualified
        .split_whitespace()
        .rfind(|part| !matches!(*part, "const" | "volatile" | "struct" | "class" | "enum"))
        .unwrap_or(unqualified)
        .trim_matches(|character: char| !(character == '_' || character.is_ascii_alphanumeric()))
        .to_string()
}

fn effective_receiver_type(value: &str) -> String {
    let outer = base_type_name(value);
    if matches!(
        outer.as_str(),
        "unique_ptr" | "shared_ptr" | "weak_ptr" | "optional" | "reference_wrapper"
    ) {
        if let Some(arguments) = value
            .split_once('<')
            .and_then(|(_, rest)| rest.rsplit_once('>'))
            .map(|(arguments, _)| arguments)
        {
            if let Some(first) = arguments.split(',').next() {
                return base_type_name(first);
            }
        }
    }
    outer
}

fn normalize_type_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn declaration_name(node: Node<'_>, source: &str) -> Option<String> {
    let declarator = node
        .child_by_field_name("declarator")
        .or_else(|| find_descendant(node, |candidate| candidate.kind() == "function_declarator"))?;
    find_descendant(declarator, |candidate| {
        matches!(candidate.kind(), "identifier" | "field_identifier")
    })
    .and_then(|name| node_text(name, source))
}

fn objective_c_selector(node: Node<'_>, source: &str) -> String {
    objective_c_method_identity(node, source)
        .map(|identity| identity.selector)
        .unwrap_or_else(|| "<objc-method>".to_string())
}

fn objective_c_method_identity(node: Node<'_>, source: &str) -> Option<ObjectiveCMethodIdentity> {
    if !matches!(node.kind(), "method_definition" | "method_declaration") {
        return None;
    }
    let mut cursor = node.walk();
    let children = node.named_children(&mut cursor).collect::<Vec<_>>();
    let selector_nodes = children
        .iter()
        .copied()
        .filter(|child| is_identifier_node(child.kind()))
        .collect::<Vec<_>>();
    if selector_nodes.is_empty() {
        return None;
    }
    let arity = children
        .iter()
        .filter(|child| child.kind() == "method_parameter")
        .count();
    let selector = selector_nodes
        .iter()
        .filter_map(|selector| node_text(*selector, source))
        .collect::<Vec<_>>()
        .join(":");
    let selector = if arity == 0 {
        selector
    } else {
        format!("{selector}:")
    };
    let selector_indexes = selector_nodes
        .iter()
        .enumerate()
        .map(|(index, selector)| (selector.id(), index))
        .collect();
    Some(ObjectiveCMethodIdentity {
        selector,
        selector_indexes,
        arity,
    })
}

fn first_direct_identifier(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| is_identifier_node(child.kind()))
}

fn is_direct_child(parent: Node<'_>, child: Node<'_>) -> bool {
    let mut cursor = parent.walk();
    parent
        .named_children(&mut cursor)
        .any(|candidate| candidate.id() == child.id())
}

fn is_parameter_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if parent.kind() != "method_parameter" {
        return false;
    }
    let mut cursor = parent.walk();
    parent
        .named_children(&mut cursor)
        .filter(|child| is_identifier_node(child.kind()))
        .collect::<Vec<_>>()
        .last()
        .copied()
        .is_some_and(|candidate| candidate.id() == node.id())
}

fn ancestor_of_kind<'a>(mut node: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    while let Some(parent) = node.parent() {
        if kinds.contains(&parent.kind()) {
            return Some(parent);
        }
        node = parent;
    }
    None
}

fn find_descendant(
    node: Node<'_>,
    predicate: impl Fn(Node<'_>) -> bool + Copy,
) -> Option<Node<'_>> {
    if predicate(node) {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = find_descendant(child, predicate) {
            return Some(found);
        }
    }
    None
}

fn node_text(node: Node<'_>, source: &str) -> Option<String> {
    node.utf8_text(source.as_bytes())
        .ok()
        .map(ToOwned::to_owned)
}
