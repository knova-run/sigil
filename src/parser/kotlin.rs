//! Kotlin symbol and text extraction.
//!
//! Targets the `tree-sitter-kotlin-sg` grammar (ast-grep fork). Covers the
//! core top-level / class-body declarations: function, class, interface
//! (both surface as `class_declaration` in this grammar — disambiguated via
//! the leading anonymous keyword), object, property, package, import.
//!
//! Nested members (functions / properties inside a class or object body)
//! are emitted with a qualified `Outer.member` name, mirroring the Java
//! extractor's convention so downstream tooling can join consistently.

use tree_sitter::{Node, Tree};

use crate::parser::format::{ReferenceEntry, SymbolEntry, TextEntry};
use crate::parser::helpers::*;
use crate::parser::treesitter::MAX_DEPTH;

/// Kotlin keywords and noisy identifiers we don't want surfacing as tokens.
const KOTLIN_STOPWORDS: &[&str] = &[
    "fun",
    "val",
    "var",
    "class",
    "object",
    "interface",
    "data",
    "sealed",
    "open",
    "abstract",
    "override",
    "companion",
    "init",
    "constructor",
    "internal",
    "lateinit",
    "suspend",
    "operator",
    "infix",
    "inline",
    "tailrec",
    "external",
    "vararg",
    "crossinline",
    "noinline",
    "reified",
    "import",
    "package",
    // Common Kotlin builtins. Lowercase here because the filter calls
    // `tok.to_lowercase()` before the contains() check — PascalCase
    // entries would silently never match.
    "int",
    "long",
    "short",
    "byte",
    "float",
    "double",
    "boolean",
    "char",
    "string",
    "unit",
    "any",
    "nothing",
    "list",
    "map",
    "set",
    "array",
    "mutablelist",
    "mutablemap",
    "mutableset",
];

fn filter_kotlin_tokens(tokens: Option<String>) -> Option<String> {
    tokens.and_then(|t| {
        let filtered: Vec<&str> = t
            .split_whitespace()
            .filter(|tok| !KOTLIN_STOPWORDS.contains(&tok.to_lowercase().as_str()))
            .filter(|tok| !tok.chars().all(|c| c.is_uppercase() || c == '_'))
            .collect();
        if filtered.is_empty() {
            None
        } else {
            Some(filtered.join(" "))
        }
    })
}

/// Common Kotlin stdlib calls we filter from the call graph.
fn is_kotlin_builtin(name: &str) -> bool {
    matches!(
        name,
        "println"
            | "print"
            | "error"
            | "require"
            | "check"
            | "TODO"
            | "let"
            | "apply"
            | "also"
            | "run"
            | "with"
            | "takeIf"
            | "takeUnless"
            | "toString"
            | "equals"
            | "hashCode"
            | "listOf"
            | "mutableListOf"
            | "mapOf"
            | "mutableMapOf"
            | "setOf"
            | "mutableSetOf"
            | "arrayOf"
            | "emptyList"
            | "emptyMap"
            | "emptySet"
    )
}

fn is_kotlin_primitive(name: &str) -> bool {
    matches!(
        name,
        "Int"
            | "Long"
            | "Short"
            | "Byte"
            | "Float"
            | "Double"
            | "Boolean"
            | "Char"
            | "String"
            | "Unit"
            | "Any"
            | "Nothing"
    )
}

pub fn extract(
    tree: &Tree,
    source: &[u8],
    file_path: &str,
    symbols: &mut Vec<SymbolEntry>,
    texts: &mut Vec<TextEntry>,
    references: &mut Vec<ReferenceEntry>,
) {
    let root = tree.root_node();
    walk_node(root, source, file_path, None, symbols, texts, references, 0);
    resolve_kotlin_imports_tier2(symbols, references);
}

/// Tier-2 resolver. Kotlin imports bind either the last `.`-segment
/// (`import foo.Bar` → `Bar`) or the explicit `as` alias
/// (`import foo.Bar as B` → `B`). Selector calls (`navigation_expression`)
/// whose head matches an alias get upgraded to two confidence-0.8 edges:
/// the raw selector and the resolved `<import-path>/<rest>` form.
fn resolve_kotlin_imports_tier2(
    symbols: &[SymbolEntry],
    references: &mut Vec<ReferenceEntry>,
) {
    use std::collections::HashMap;
    let mut imports: HashMap<String, String> = HashMap::new();
    for s in symbols.iter().filter(|s| s.kind == "import") {
        let path = s.name.clone();
        let short = match &s.alias {
            Some(a) if !a.is_empty() => a.clone(),
            _ => match path.rsplit('.').next() {
                Some(seg) if !seg.is_empty() => seg.to_string(),
                _ => continue,
            },
        };
        imports.insert(short, path);
    }
    if imports.is_empty() {
        return;
    }
    let mut added: Vec<ReferenceEntry> = Vec::new();
    for r in references.iter_mut() {
        if r.kind != "call" {
            continue;
        }
        let Some((head, rest)) = r.name.split_once('.') else {
            continue;
        };
        let Some(path) = imports.get(head) else {
            continue;
        };
        r.confidence = Some(0.8);
        added.push(ReferenceEntry {
            file: r.file.clone(),
            name: format!("{path}/{rest}"),
            kind: "call".to_string(),
            line: r.line,
            caller: r.caller.clone(),
            project: r.project.clone(),
            confidence: Some(0.8),
        });
    }
    references.extend(added);
}

#[allow(clippy::too_many_arguments)]
fn walk_node(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
    texts: &mut Vec<TextEntry>,
    references: &mut Vec<ReferenceEntry>,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }

    let kind = node.kind();

    match kind {
        "package_header" => {
            extract_package(node, source, file_path, symbols);
            return;
        }
        "import_header" => {
            extract_import(node, source, file_path, symbols, references);
            return;
        }
        "class_declaration" => {
            let class_kind = if first_anonymous_keyword(node, source).as_deref() == Some("interface")
            {
                "interface"
            } else {
                "class"
            };
            extract_class_like(
                node, source, file_path, parent_ctx, class_kind, symbols, texts, references, depth,
            );
            return;
        }
        "object_declaration" => {
            extract_class_like(
                node, source, file_path, parent_ctx, "object", symbols, texts, references, depth,
            );
            return;
        }
        "companion_object" => {
            // companion object { ... } — nest its members under
            // `<parent>.Companion`. Tree shape is `companion_object` →
            // `class_body`.
            let nested_parent = match parent_ctx {
                Some(p) => format!("{p}.Companion"),
                None => "Companion".to_string(),
            };
            if let Some(body) = first_child_of_kind(node, "class_body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    walk_node(
                        child,
                        source,
                        file_path,
                        Some(&nested_parent),
                        symbols,
                        texts,
                        references,
                        depth + 1,
                    );
                }
            }
            return;
        }
        "function_declaration" => {
            extract_function(node, source, file_path, parent_ctx, symbols, references);
            // Walk into body for call references
            if let Some(body) = first_child_of_kind(node, "function_body") {
                let fn_name = function_name(node, source).unwrap_or_default();
                let full = qualify(parent_ctx, &fn_name);
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    walk_node(
                        child,
                        source,
                        file_path,
                        Some(&full),
                        symbols,
                        texts,
                        references,
                        depth + 1,
                    );
                }
            }
            return;
        }
        "property_declaration" => {
            extract_property(node, source, file_path, parent_ctx, symbols, references);
            return;
        }
        "line_comment" | "multiline_comment" => {
            extract_kotlin_comment(node, source, file_path, parent_ctx, texts);
            return;
        }
        "string_literal" => {
            extract_string(node, source, file_path, parent_ctx, texts);
            return;
        }
        "call_expression" => {
            extract_call_ref(node, source, file_path, parent_ctx, references);
            // Fall through to recurse into argument expressions.
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_node(
            child,
            source,
            file_path,
            parent_ctx,
            symbols,
            texts,
            references,
            depth + 1,
        );
    }
}

// ---------------------------------------------------------------------------
// Tree-walking helpers (the kotlin grammar exposes structure via positional
// children + anonymous keywords; named fields are sparse, so we work
// kind-by-kind.)
// ---------------------------------------------------------------------------

fn first_child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
    }
    None
}

/// Return the first anonymous (un-named) keyword child of a node, as text.
fn first_anonymous_keyword(node: Node, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            return Some(node_text(child, source));
        }
    }
    None
}

fn qualify(parent_ctx: Option<&str>, name: &str) -> String {
    match parent_ctx {
        Some(p) if !p.is_empty() => format!("{p}.{name}"),
        _ => name.to_string(),
    }
}

/// Visibility from a `modifiers` child of a declaration node.
fn extract_kotlin_visibility(node: Node, source: &[u8]) -> String {
    if let Some(mods) = first_child_of_kind(node, "modifiers") {
        let mut cursor = mods.walk();
        for child in mods.children(&mut cursor) {
            if child.kind() == "visibility_modifier" {
                let t = node_text(child, source);
                return match t.as_str() {
                    "public" => "public".to_string(),
                    "private" => "private".to_string(),
                    "protected" => "internal".to_string(),
                    "internal" => "internal".to_string(),
                    _ => "public".to_string(),
                };
            }
        }
    }
    // Kotlin default visibility is public.
    "public".to_string()
}

fn has_modifier_kind(node: Node, modifier_kind: &str) -> bool {
    if let Some(mods) = first_child_of_kind(node, "modifiers") {
        let mut cursor = mods.walk();
        for child in mods.children(&mut cursor) {
            if child.kind() == modifier_kind {
                return true;
            }
        }
    }
    false
}

fn has_modifier_token(node: Node, source: &[u8], token: &str) -> bool {
    if let Some(mods) = first_child_of_kind(node, "modifiers") {
        let mut cursor = mods.walk();
        for child in mods.children(&mut cursor) {
            let t = node_text(child, source);
            if t == token {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Declarations
// ---------------------------------------------------------------------------

fn extract_package(node: Node, source: &[u8], file_path: &str, symbols: &mut Vec<SymbolEntry>) {
    if let Some(ident) = first_child_of_kind(node, "identifier") {
        let name = node_text(ident, source);
        let line = node_line_range(node);
        push_symbol(
            symbols,
            file_path,
            name,
            "module",
            line,
            None,
            None,
            None,
            Some("public".to_string()),
        );
    }
}

fn extract_import(
    node: Node,
    source: &[u8],
    file_path: &str,
    symbols: &mut Vec<SymbolEntry>,
    references: &mut Vec<ReferenceEntry>,
) {
    let line = node_line_range(node);
    let ident = match first_child_of_kind(node, "identifier") {
        Some(n) => n,
        None => return,
    };
    let name = node_text(ident, source);
    let alias = first_child_of_kind(node, "import_alias").and_then(|alias_node| {
        first_child_of_kind(alias_node, "type_identifier").map(|t| node_text(t, source))
    });

    push_symbol(
        symbols,
        file_path,
        name.clone(),
        "import",
        line,
        None,
        None,
        alias,
        Some("private".to_string()),
    );
    references.push(ReferenceEntry {
        file: file_path.to_string(),
        name,
        kind: "import".to_string(),
        line,
        caller: None,
        project: String::new(),
    confidence: None,
    });
}

#[allow(clippy::too_many_arguments)]
fn extract_class_like(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    kind: &str,
    symbols: &mut Vec<SymbolEntry>,
    texts: &mut Vec<TextEntry>,
    references: &mut Vec<ReferenceEntry>,
    depth: usize,
) {
    let name = match first_child_of_kind(node, "type_identifier") {
        Some(n) => node_text(n, source),
        None => return,
    };

    let line = node_line_range(node);
    let visibility = extract_kotlin_visibility(node, source);
    let full_name = qualify(parent_ctx, &name);

    let tokens = first_child_of_kind(node, "class_body")
        .and_then(|body| filter_kotlin_tokens(extract_tokens(body, source)));

    // Heritage: Kotlin's `class Dog : Animal(), Runnable` exposes its
    // supertypes via `delegation_specifier` children inside an inner
    // `class_body`-sibling list. We walk all descendants for
    // `delegation_specifier` and extract the first user_type / identifier
    // within each. Discriminating extend (single concrete superclass)
    // from implement (interfaces) requires a symbol table; for now
    // every supertype lands as `extend` — same trade-off the Kotlin
    // grammar makes (no explicit `implements` keyword).
    let mut heritage: Vec<(String, String)> = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "delegation_specifier" {
            continue;
        }
        let target = kotlin_supertype_name(child, source);
        if !target.is_empty() {
            heritage.push(("extend".to_string(), target));
        }
    }

    symbols.push(SymbolEntry {
        file: file_path.to_string(),
        name: full_name.clone(),
        kind: kind.to_string(),
        line,
        parent: parent_ctx.map(String::from),
        tokens,
        alias: None,
        visibility: Some(visibility),
        sig: None,
        project: String::new(),
        heritage,
    });

    if let Some(body) = first_child_of_kind(node, "class_body") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            walk_node(
                child,
                source,
                file_path,
                Some(&full_name),
                symbols,
                texts,
                references,
                depth + 1,
            );
        }
    }
}

/// Pull the supertype name from a Kotlin `delegation_specifier` node.
/// Common shapes:
///   * `user_type` (Foo)
///   * `constructor_invocation` (Foo()) → first user_type child
///   * `explicit_delegation` (Foo by bar) → first user_type child
fn kotlin_supertype_name(node: Node, source: &[u8]) -> String {
    fn extract_user_type(n: Node, source: &[u8]) -> String {
        // Strip the generic-args / `?` suffix by taking the first type_identifier.
        let mut cursor = n.walk();
        for c in n.children(&mut cursor) {
            if c.kind() == "type_identifier" {
                return node_text(c, source);
            }
        }
        node_text(n, source)
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "user_type" => return extract_user_type(child, source),
            "constructor_invocation" | "explicit_delegation" => {
                let mut inner = child.walk();
                for c in child.children(&mut inner) {
                    if c.kind() == "user_type" {
                        return extract_user_type(c, source);
                    }
                }
            }
            _ => {}
        }
    }
    String::new()
}

fn function_name(node: Node, source: &[u8]) -> Option<String> {
    // For `function_declaration`, the function name is the first
    // `simple_identifier` child that follows any `modifiers` / `type_parameters`.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "simple_identifier" {
            return Some(node_text(child, source));
        }
    }
    None
}

fn extract_function(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
    _references: &mut Vec<ReferenceEntry>,
) {
    let name = match function_name(node, source) {
        Some(n) => n,
        None => return,
    };

    let line = node_line_range(node);
    let visibility = extract_kotlin_visibility(node, source);
    let full_name = qualify(parent_ctx, &name);

    // Kind: top-level = "function"; nested inside class/object = "method".
    let kind = if parent_ctx.is_some() { "method" } else { "function" };

    let tokens = first_child_of_kind(node, "function_body")
        .and_then(|body| filter_kotlin_tokens(extract_tokens(body, source)));

    push_symbol(
        symbols,
        file_path,
        full_name,
        kind,
        line,
        parent_ctx,
        tokens,
        None,
        Some(visibility),
    );
}

fn extract_property(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
    _references: &mut Vec<ReferenceEntry>,
) {
    let line = node_line_range(node);
    let visibility = extract_kotlin_visibility(node, source);

    // const val FOO = ... → constant; val/var → property (when inside a
    // class) or variable (at file scope, when mutable). The Kotlin
    // convention is `const val` for compile-time constants and ALL_CAPS
    // `val` for runtime singletons; we treat both as "constant" only when
    // the `const` modifier is present (preserves the language semantics).
    let is_const = has_modifier_kind(node, "property_modifier")
        && has_modifier_token(node, source, "const");

    let binding_is_val = first_child_of_kind(node, "binding_pattern_kind")
        .map(|b| node_text(b, source) == "val")
        .unwrap_or(false);

    let kind = if is_const {
        "constant"
    } else if parent_ctx.is_some() {
        "property"
    } else if binding_is_val {
        // Top-level `val NAME` looks like a Rust `static` — treat as constant
        // when ALL_CAPS, otherwise as a variable.
        let var_decl = first_child_of_kind(node, "variable_declaration");
        let pname = var_decl
            .and_then(|v| first_child_of_kind(v, "simple_identifier"))
            .map(|s| node_text(s, source))
            .unwrap_or_default();
        if !pname.is_empty()
            && pname
                .chars()
                .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
        {
            "constant"
        } else {
            "variable"
        }
    } else {
        "variable"
    };

    // Walk variable_declaration to get the name (and type). Kotlin allows
    // destructuring `val (a, b) = pair`, which exposes multiple
    // variable_declaration children; emit one symbol per name.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "variable_declaration" {
            continue;
        }
        let name_node = match first_child_of_kind(child, "simple_identifier") {
            Some(n) => n,
            None => continue,
        };
        let name = node_text(name_node, source);
        let full_name = qualify(parent_ctx, &name);

        // For constants, surface the RHS literal as sig so `code.context FOO`
        // returns the value inline (matches the Java/Rust convention).
        let sig = if kind == "constant" {
            rhs_literal_text(node, source).map(|s| truncate_sig(&s))
        } else {
            None
        };

        symbols.push(SymbolEntry {
            file: file_path.to_string(),
            name: full_name,
            kind: kind.to_string(),
            line,
            parent: parent_ctx.map(String::from),
            tokens: None,
            alias: None,
            visibility: Some(visibility.clone()),
            sig,
            project: String::new(),
        heritage: Vec::new(),
    });
    }
}

/// Best-effort right-hand-side literal extraction. Looks past
/// `modifiers`, `binding_pattern_kind`, `variable_declaration` for the
/// initializer expression.
fn rhs_literal_text(node: Node, source: &[u8]) -> Option<String> {
    let mut saw_var_decl = false;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "variable_declaration" {
            saw_var_decl = true;
            continue;
        }
        if saw_var_decl && child.is_named() {
            return Some(node_text(child, source));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Call references
// ---------------------------------------------------------------------------

fn extract_call_ref(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    references: &mut Vec<ReferenceEntry>,
) {
    // call_expression → first named child is the callee (simple_identifier
    // or navigation_expression). Skip the call_suffix.
    let mut cursor = node.walk();
    let callee = node
        .children(&mut cursor)
        .find(|c| c.is_named() && c.kind() != "call_suffix");
    let Some(callee) = callee else {
        return;
    };
    let (name, confidence) = match callee.kind() {
        "simple_identifier" => (node_text(callee, source), Some(0.95_f64)),
        "navigation_expression" => (node_text(callee, source), None),
        _ => return,
    };
    if name.is_empty() {
        return;
    }
    // Drop pure builtins (println, etc.) and unqualified type-name calls
    // for primitives — these are constructor-like and rarely useful.
    let leaf = name.rsplit('.').next().unwrap_or(&name);
    if is_kotlin_builtin(leaf) || is_kotlin_primitive(leaf) {
        return;
    }

    references.push(ReferenceEntry {
        file: file_path.to_string(),
        name,
        kind: "call".to_string(),
        line: node_line_range(node),
        caller: parent_ctx.map(String::from),
        project: String::new(),
        confidence,
    });
}

// ---------------------------------------------------------------------------
// Comments
// ---------------------------------------------------------------------------

fn extract_kotlin_comment(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    texts: &mut Vec<TextEntry>,
) {
    // Reuse the generic C-style comment handler; `multiline_comment`
    // starting with `/**` is treated as docstring.
    extract_comment(node, source, file_path, parent_ctx, texts);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::treesitter::parse_file;

    fn find_sym<'a>(symbols: &'a [SymbolEntry], name: &str) -> &'a SymbolEntry {
        symbols
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("symbol not found: {name}\nhave: {:?}",
                symbols.iter().map(|s| (&s.name, &s.kind)).collect::<Vec<_>>()))
    }

    #[test]
    fn kotlin_stopwords_filter_pascal_case_types() {
        // `to_lowercase()` is applied to each token before the contains()
        // check, so the stopword list itself must be all-lowercase. A
        // PascalCase entry like `"Int"` would never match. Regression test
        // for that case-mismatch bug.
        let out = filter_kotlin_tokens(Some(
            "name Int Long Boolean myValue helperFn".to_string(),
        ));
        let out = out.expect("non-trivial filtered output");
        assert!(
            !out.split_whitespace().any(|t| t.eq_ignore_ascii_case("int")),
            "`Int` must be filtered as a Kotlin builtin type token, got: {out}",
        );
        assert!(
            !out.split_whitespace().any(|t| t.eq_ignore_ascii_case("long")),
            "`Long` must be filtered, got: {out}",
        );
        // Non-stopword identifiers survive.
        assert!(out.split_whitespace().any(|t| t == "myValue"));
        assert!(out.split_whitespace().any(|t| t == "helperFn"));
    }

    #[test]
    fn kotlin_imported_class_call_gets_tier2_two_edges() {
        // `import foo.Bar` then `Bar.greet()` resolves via the import
        // table. Emit two confidence-0.8 edges: raw + `foo.Bar/greet`.
        let source = b"import foo.Bar\n\nfun caller() { Bar.greet() }\n";
        let (_, _, refs) = parse_file(source, "kotlin", "t.kt").unwrap();
        let raw = refs
            .iter()
            .find(|r| r.kind == "call" && r.name == "Bar.greet")
            .expect("raw Bar.greet");
        assert_eq!(raw.confidence, Some(0.8));
        let resolved = refs
            .iter()
            .find(|r| r.kind == "call" && r.name == "foo.Bar/greet")
            .expect("resolved foo.Bar/greet");
        assert_eq!(resolved.confidence, Some(0.8));
    }

    #[test]
    fn kotlin_imported_alias_call_resolves() {
        // `import foo.Bar as B` then `B.greet()` resolves via alias.
        let source = b"import foo.Bar as B\n\nfun caller() { B.greet() }\n";
        let (_, _, refs) = parse_file(source, "kotlin", "t.kt").unwrap();
        let resolved = refs
            .iter()
            .find(|r| r.kind == "call" && r.name == "foo.Bar/greet")
            .expect("resolved foo.Bar/greet via alias B");
        assert_eq!(resolved.confidence, Some(0.8));
    }

    #[test]
    fn kotlin_bare_call_gets_tier1_confidence() {
        let source = b"fun caller() { helper(); obj.method() }\nfun helper() {}\n";
        let (_, _, refs) = parse_file(source, "kotlin", "t.kt").unwrap();
        let bare = refs
            .iter()
            .find(|r| r.kind == "call" && r.name == "helper")
            .expect("helper() bare call");
        assert_eq!(bare.confidence, Some(0.95));
        let nav = refs.iter().find(|r| r.kind == "call" && r.name == "obj.method");
        if let Some(n) = nav {
            assert_eq!(n.confidence, None);
        }
    }

    #[test]
    fn kotlin_top_level_function() {
        let source = b"fun greet(name: String): String {\n    return \"hi $name\"\n}\n";
        let (symbols, _, _) = parse_file(source, "kotlin", "t.kt").unwrap();
        let g = find_sym(&symbols, "greet");
        assert_eq!(g.kind, "function");
        assert_eq!(g.visibility.as_deref(), Some("public"));
    }

    #[test]
    fn kotlin_class_with_method() {
        let source = b"class Person(val name: String) {\n    fun greet(): String = \"hi $name\"\n}\n";
        let (symbols, _, _) = parse_file(source, "kotlin", "t.kt").unwrap();
        let p = find_sym(&symbols, "Person");
        assert_eq!(p.kind, "class");
        let g = find_sym(&symbols, "Person.greet");
        assert_eq!(g.kind, "method");
    }

    #[test]
    fn kotlin_interface() {
        let source = b"interface Greeter {\n    fun greet(): String\n}\n";
        let (symbols, _, _) = parse_file(source, "kotlin", "t.kt").unwrap();
        let i = find_sym(&symbols, "Greeter");
        assert_eq!(i.kind, "interface");
    }

    #[test]
    fn kotlin_object() {
        let source = b"object Singleton {\n    fun work() {}\n}\n";
        let (symbols, _, _) = parse_file(source, "kotlin", "t.kt").unwrap();
        let s = find_sym(&symbols, "Singleton");
        assert_eq!(s.kind, "object");
        let w = find_sym(&symbols, "Singleton.work");
        assert_eq!(w.kind, "method");
    }

    #[test]
    fn kotlin_top_level_const() {
        let source = b"const val MAX_RETRIES: Int = 3\n";
        let (symbols, _, _) = parse_file(source, "kotlin", "t.kt").unwrap();
        let c = find_sym(&symbols, "MAX_RETRIES");
        assert_eq!(c.kind, "constant");
        assert!(c.sig.as_deref().unwrap_or("").contains("3"));
    }

    #[test]
    fn kotlin_visibility() {
        let source = b"private fun p() {}\nfun pub() {}\ninternal fun i() {}\n";
        let (symbols, _, _) = parse_file(source, "kotlin", "t.kt").unwrap();
        assert_eq!(find_sym(&symbols, "p").visibility.as_deref(), Some("private"));
        assert_eq!(find_sym(&symbols, "pub").visibility.as_deref(), Some("public"));
        assert_eq!(find_sym(&symbols, "i").visibility.as_deref(), Some("internal"));
    }

    #[test]
    fn kotlin_imports() {
        let source = b"import kotlin.collections.List\nimport kotlin.io.println as p\n";
        let (symbols, _, refs) = parse_file(source, "kotlin", "t.kt").unwrap();
        let imports: Vec<_> = symbols.iter().filter(|s| s.kind == "import").collect();
        assert_eq!(imports.len(), 2);
        assert!(imports.iter().any(|s| s.name == "kotlin.collections.List"));
        assert!(imports.iter().any(|s| s.alias.as_deref() == Some("p")));
        let import_refs: Vec<_> = refs.iter().filter(|r| r.kind == "import").collect();
        assert_eq!(import_refs.len(), 2);
    }

    #[test]
    fn kotlin_package() {
        let source = b"package com.example.app\n\nfun foo() {}\n";
        let (symbols, _, _) = parse_file(source, "kotlin", "t.kt").unwrap();
        let pkg = symbols.iter().find(|s| s.kind == "module").unwrap();
        assert_eq!(pkg.name, "com.example.app");
    }

    #[test]
    fn kotlin_companion_object() {
        let source = b"class Person {\n    companion object {\n        const val SPECIES = \"human\"\n        fun create() {}\n    }\n}\n";
        let (symbols, _, _) = parse_file(source, "kotlin", "t.kt").unwrap();
        let species = find_sym(&symbols, "Person.Companion.SPECIES");
        assert_eq!(species.kind, "constant");
        let create = find_sym(&symbols, "Person.Companion.create");
        assert_eq!(create.kind, "method");
    }
}
