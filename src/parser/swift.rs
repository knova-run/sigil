//! Swift symbol and text extraction.
//!
//! Targets the `tree-sitter-swift` v0.7 grammar.
//!
//! Notable shape choices in this grammar:
//!
//! - `class_declaration` is used for `class`, `struct`, `enum`, `actor`, and
//!   `extension`. The leading anonymous keyword child (`class` / `struct` /
//!   `enum` / `actor` / `extension`) disambiguates them. Name comes from a
//!   `type_identifier` child for `class` / `struct` / `enum` / `actor`, and
//!   from a `user_type` child for `extension` (so `extension Foo` resolves
//!   to `Foo`).
//! - `protocol_declaration` is separate and surfaces as `interface` to align
//!   with the Java/Kotlin convention.
//! - `property_declaration` carries a `value_binding_pattern` whose first
//!   anonymous child is `let` or `var`; the bound name lives in a sibling
//!   `pattern` → `simple_identifier`. Top-level `let` with an ALL_CAPS name
//!   maps to `constant`; nested `let` and `var` both surface as `property`
//!   (matching Kotlin's class-body property convention); top-level `var`
//!   maps to `variable`.
//! - Visibility lives under a `modifiers` → `visibility_modifier` chain.
//!   Swift exposes five levels: `open`, `public`, `internal` (default),
//!   `fileprivate`, `private`. We map `open` → `public` and
//!   `fileprivate` → `internal` to fit the three-bucket sigil schema.
//!
//! Nested members emit qualified `Outer.member` names, mirroring the
//! Kotlin / Java extractor convention so downstream tooling can join
//! consistently.

use tree_sitter::{Node, Tree};

use crate::parser::format::{ReferenceEntry, SymbolEntry, TextEntry};
use crate::parser::helpers::*;
use crate::parser::treesitter::MAX_DEPTH;

/// Swift keywords and noisy identifiers we don't want surfacing as tokens.
const SWIFT_STOPWORDS: &[&str] = &[
    "func",
    "let",
    "var",
    "class",
    "struct",
    "enum",
    "protocol",
    "actor",
    "extension",
    "public",
    "internal",
    "fileprivate",
    "private",
    "open",
    "final",
    "static",
    "lazy",
    "weak",
    "unowned",
    "override",
    "mutating",
    "nonmutating",
    "convenience",
    "required",
    "import",
    // Common Swift builtin types. Lowercase here because the filter
    // calls `tok.to_lowercase()` before contains() — PascalCase entries
    // would silently never match.
    "int",
    "float",
    "double",
    "bool",
    "string",
    "character",
    "optional",
    "array",
    "dictionary",
    "set",
    "void",
    "any",
    "self",
];

fn filter_swift_tokens(tokens: Option<String>) -> Option<String> {
    tokens.and_then(|t| {
        let filtered: Vec<&str> = t
            .split_whitespace()
            .filter(|tok| !SWIFT_STOPWORDS.contains(&tok.to_lowercase().as_str()))
            // Drop ALL-CAPS shouting (likely macros / constants used as
            // configuration switches, not interesting search tokens).
            .filter(|tok| !tok.chars().all(|c| c.is_uppercase() || c == '_'))
            .collect();
        if filtered.is_empty() {
            None
        } else {
            Some(filtered.join(" "))
        }
    })
}

/// Common Swift stdlib calls we filter from the call graph.
fn is_swift_builtin(name: &str) -> bool {
    matches!(
        name,
        "print" | "assert" | "precondition" | "fatalError" | "debugPrint" | "dump"
    )
}

fn is_swift_primitive(name: &str) -> bool {
    matches!(
        name,
        "Int"
            | "Float"
            | "Double"
            | "Bool"
            | "String"
            | "Character"
            | "Optional"
            | "Array"
            | "Dictionary"
            | "Set"
            | "Void"
            | "Any"
            | "Self"
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
        "import_declaration" => {
            extract_import(node, source, file_path, symbols, references);
            return;
        }
        "class_declaration" => {
            // Disambiguate class / struct / enum / actor / extension via the
            // leading anonymous keyword.
            let decl_kw = first_anonymous_keyword(node, source);
            let class_kind = match decl_kw.as_deref() {
                Some("extension") => "class", // extensions surface as `class`
                Some("actor") => "class",
                _ => "class",
            };
            let _ = class_kind;
            extract_class_like(
                node, source, file_path, parent_ctx, symbols, texts, references, depth,
            );
            return;
        }
        "protocol_declaration" => {
            extract_protocol(
                node, source, file_path, parent_ctx, symbols, texts, references, depth,
            );
            return;
        }
        "function_declaration" => {
            extract_function(node, source, file_path, parent_ctx, symbols);
            // Walk body for call refs / nested string literals / comments.
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
        "init_declaration" => {
            extract_init(node, source, file_path, parent_ctx, symbols);
            if let Some(body) = first_child_of_kind(node, "function_body") {
                let full = qualify(parent_ctx, "init");
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
        "protocol_function_declaration" => {
            extract_protocol_function(node, source, file_path, parent_ctx, symbols);
            return;
        }
        "protocol_property_declaration" => {
            extract_protocol_property(node, source, file_path, parent_ctx, symbols);
            return;
        }
        "property_declaration" => {
            extract_property(node, source, file_path, parent_ctx, symbols);
            return;
        }
        "enum_entry" => {
            extract_enum_entry(node, source, file_path, parent_ctx, symbols);
            return;
        }
        "comment" | "multiline_comment" => {
            extract_comment(node, source, file_path, parent_ctx, texts);
            return;
        }
        "line_string_literal" | "multi_line_string_literal" | "raw_string_literal" => {
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
// Tree-walking helpers
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

/// First anonymous (un-named) child of a node, as text. Used to find the
/// declaration keyword (`struct`, `class`, `extension`, `let`, `var`, …)
/// when the grammar doesn't expose it as a field.
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

/// Visibility from a `modifiers` child. Swift defaults to `internal`.
///
/// `open` → `public` (callable + overridable from other modules)
/// `fileprivate` → `internal` (broader than `private`; sigil only has three buckets)
fn extract_swift_visibility(node: Node, source: &[u8]) -> String {
    if let Some(mods) = first_child_of_kind(node, "modifiers") {
        let mut cursor = mods.walk();
        for child in mods.children(&mut cursor) {
            if child.kind() == "visibility_modifier" {
                let t = node_text(child, source);
                return match t.as_str() {
                    "open" | "public" => "public".to_string(),
                    "private" => "private".to_string(),
                    "fileprivate" | "internal" => "internal".to_string(),
                    _ => "internal".to_string(),
                };
            }
        }
    }
    // Swift default visibility is internal.
    "internal".to_string()
}

// ---------------------------------------------------------------------------
// Declarations
// ---------------------------------------------------------------------------

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

    push_symbol(
        symbols,
        file_path,
        name.clone(),
        "import",
        line,
        None,
        None,
        None,
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

/// Pull the textual name of a `class_declaration`. For `class` / `struct` /
/// `enum` / `actor` this is a `type_identifier` child. For `extension`,
/// the grammar uses a `user_type` wrapping a `type_identifier`.
fn class_like_name(node: Node, source: &[u8]) -> Option<String> {
    if let Some(t) = first_child_of_kind(node, "type_identifier") {
        return Some(node_text(t, source));
    }
    if let Some(u) = first_child_of_kind(node, "user_type") {
        // Walk into the user_type for its leading type_identifier.
        if let Some(t) = first_child_of_kind(u, "type_identifier") {
            return Some(node_text(t, source));
        }
        return Some(node_text(u, source));
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn extract_class_like(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
    texts: &mut Vec<TextEntry>,
    references: &mut Vec<ReferenceEntry>,
    depth: usize,
) {
    let name = match class_like_name(node, source) {
        Some(n) => n,
        None => return,
    };

    let line = node_line_range(node);
    let visibility = extract_swift_visibility(node, source);
    let full_name = qualify(parent_ctx, &name);

    // The body is either `class_body` or `enum_class_body`.
    let body = first_child_of_kind(node, "class_body")
        .or_else(|| first_child_of_kind(node, "enum_class_body"));

    let tokens = body.and_then(|b| filter_swift_tokens(extract_tokens(b, source)));

    // All class-like declarations surface as `class` per the spec — the
    // distinction between class / struct / enum / actor / extension lives
    // in the source and is recoverable, but the symbol kind is uniform
    // so downstream queries don't need to special-case each variant.
    push_symbol(
        symbols,
        file_path,
        full_name.clone(),
        "class",
        line,
        parent_ctx,
        tokens,
        None,
        Some(visibility),
    );

    if let Some(body) = body {
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

#[allow(clippy::too_many_arguments)]
fn extract_protocol(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
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
    let visibility = extract_swift_visibility(node, source);
    let full_name = qualify(parent_ctx, &name);

    let body = first_child_of_kind(node, "protocol_body");
    let tokens = body.and_then(|b| filter_swift_tokens(extract_tokens(b, source)));

    push_symbol(
        symbols,
        file_path,
        full_name.clone(),
        "interface",
        line,
        parent_ctx,
        tokens,
        None,
        Some(visibility),
    );

    if let Some(body) = body {
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

fn function_name(node: Node, source: &[u8]) -> Option<String> {
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
) {
    let name = match function_name(node, source) {
        Some(n) => n,
        None => return,
    };

    let line = node_line_range(node);
    let visibility = extract_swift_visibility(node, source);
    let full_name = qualify(parent_ctx, &name);

    // Top-level = "function"; nested inside class/struct/enum/extension/protocol = "method".
    let kind = if parent_ctx.is_some() { "method" } else { "function" };

    let tokens = first_child_of_kind(node, "function_body")
        .and_then(|body| filter_swift_tokens(extract_tokens(body, source)));

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

/// Swift `init` — emitted as a method named `init` under the enclosing type.
fn extract_init(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
) {
    let line = node_line_range(node);
    let visibility = extract_swift_visibility(node, source);
    let full_name = qualify(parent_ctx, "init");

    let tokens = first_child_of_kind(node, "function_body")
        .and_then(|body| filter_swift_tokens(extract_tokens(body, source)));

    // `init` only ever appears inside a class/struct/actor/extension, so it
    // is unconditionally a method.
    push_symbol(
        symbols,
        file_path,
        full_name,
        "method",
        line,
        parent_ctx,
        tokens,
        None,
        Some(visibility),
    );
}

fn extract_protocol_function(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
) {
    let name = match function_name(node, source) {
        Some(n) => n,
        None => return,
    };

    let line = node_line_range(node);
    let visibility = extract_swift_visibility(node, source);
    let full_name = qualify(parent_ctx, &name);

    push_symbol(
        symbols,
        file_path,
        full_name,
        "method",
        line,
        parent_ctx,
        None,
        None,
        Some(visibility),
    );
}

/// `protocol_property_declaration` shape:
///   pattern { value_binding_pattern(var) simple_identifier(name) }
fn extract_protocol_property(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
) {
    let line = node_line_range(node);
    let visibility = extract_swift_visibility(node, source);

    let Some(pat) = first_child_of_kind(node, "pattern") else {
        return;
    };
    let Some(ident) = first_child_of_kind(pat, "simple_identifier") else {
        return;
    };
    let name = node_text(ident, source);
    let full_name = qualify(parent_ctx, &name);

    push_symbol(
        symbols,
        file_path,
        full_name,
        "property",
        line,
        parent_ctx,
        None,
        None,
        Some(visibility),
    );
}

/// `property_declaration` shape:
///   modifiers? value_binding_pattern(let|var) pattern{simple_identifier}
///   type_annotation? (= expr)?
fn extract_property(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
) {
    let line = node_line_range(node);
    let visibility = extract_swift_visibility(node, source);

    // let vs var
    let binding_is_let = first_child_of_kind(node, "value_binding_pattern")
        .map(|b| node_text(b, source).trim() == "let")
        .unwrap_or(false);

    let name = match first_child_of_kind(node, "pattern") {
        Some(p) => match first_child_of_kind(p, "simple_identifier") {
            Some(s) => node_text(s, source),
            None => return,
        },
        None => return,
    };

    // Classification:
    //   - inside a class/struct/extension/etc.: `property` (regardless of let/var)
    //   - top-level `let` with ALL_CAPS name: `constant`
    //   - top-level `let` otherwise: `constant` (immutable bindings at module
    //     scope behave like Rust `static` / Java `final`)
    //   - top-level `var`: `variable`
    let kind = if parent_ctx.is_some() {
        "property"
    } else if binding_is_let {
        let is_all_caps = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit());
        if is_all_caps {
            "constant"
        } else {
            // Top-level `let` is immutable; we still classify non-ALL_CAPS
            // bindings as `variable` to match the Kotlin convention (only
            // ALL_CAPS top-level `val` becomes `constant`).
            "variable"
        }
    } else {
        "variable"
    };

    let full_name = qualify(parent_ctx, &name);

    // For constants, surface the RHS literal as sig.
    let sig = if kind == "constant" {
        property_rhs_text(node, source).map(|s| truncate_sig(&s))
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
        visibility: Some(visibility),
        sig,
        project: String::new(),
    heritage: Vec::new(),
    });
}

/// Best-effort RHS extraction: collect the first named child that appears
/// after the `=` anonymous token. The Swift grammar emits the initializer
/// as a sibling of `value_binding_pattern` / `pattern` / `type_annotation`.
fn property_rhs_text(node: Node, source: &[u8]) -> Option<String> {
    let mut saw_eq = false;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            let t = node_text(child, source);
            if t == "=" {
                saw_eq = true;
            }
            continue;
        }
        if saw_eq {
            return Some(node_text(child, source));
        }
    }
    None
}

/// `enum_entry` — Swift `case foo` inside an enum. Surface each case as a
/// `constant` so it shows up in symbol search; the parent is the enum name.
fn extract_enum_entry(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
) {
    let line = node_line_range(node);
    let Some(ident) = first_child_of_kind(node, "simple_identifier") else {
        return;
    };
    let name = node_text(ident, source);
    let full_name = qualify(parent_ctx, &name);

    push_symbol(
        symbols,
        file_path,
        full_name,
        "constant",
        line,
        parent_ctx,
        None,
        None,
        Some("public".to_string()),
    );
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
    // call_expression → first named child is the callee, then `call_suffix`.
    let mut cursor = node.walk();
    let callee = node
        .children(&mut cursor)
        .find(|c| c.is_named() && c.kind() != "call_suffix");
    let Some(callee) = callee else {
        return;
    };
    let (name, confidence) = match callee.kind() {
        "simple_identifier" => (node_text(callee, source), Some(1.0_f64)),
        "navigation_expression" => (node_text(callee, source), None),
        _ => return,
    };
    if name.is_empty() {
        return;
    }
    let leaf = name.rsplit('.').next().unwrap_or(&name);
    if is_swift_builtin(leaf) || is_swift_primitive(leaf) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::treesitter::parse_file;

    fn find_sym<'a>(symbols: &'a [SymbolEntry], name: &str) -> &'a SymbolEntry {
        symbols.iter().find(|s| s.name == name).unwrap_or_else(|| {
            panic!(
                "symbol not found: {name}\nhave: {:?}",
                symbols
                    .iter()
                    .map(|s| (&s.name, &s.kind))
                    .collect::<Vec<_>>()
            )
        })
    }

    #[test]
    fn swift_bare_call_gets_tier1_confidence() {
        let source = b"func caller() { helper(); obj.method() }\nfunc helper() {}\n";
        let (_, _, refs) = parse_file(source, "swift", "t.swift").unwrap();
        let bare = refs
            .iter()
            .find(|r| r.kind == "call" && r.name == "helper")
            .expect("helper() bare call");
        assert_eq!(bare.confidence, Some(1.0));
        let nav = refs.iter().find(|r| r.kind == "call" && r.name == "obj.method");
        if let Some(n) = nav {
            assert_eq!(n.confidence, None);
        }
    }

    #[test]
    fn swift_top_level_function() {
        let source = b"func greet(name: String) -> String {\n    return \"hi\"\n}\n";
        let (symbols, _, _) = parse_file(source, "swift", "t.swift").unwrap();
        let g = find_sym(&symbols, "greet");
        assert_eq!(g.kind, "function");
        // No modifier → default visibility is internal.
        assert_eq!(g.visibility.as_deref(), Some("internal"));
    }

    #[test]
    fn swift_struct_with_method_and_property() {
        let source = b"struct Point {\n    let x: Double\n    func len() -> Double { return 0 }\n}\n";
        let (symbols, _, _) = parse_file(source, "swift", "t.swift").unwrap();
        let p = find_sym(&symbols, "Point");
        assert_eq!(p.kind, "class");
        let x = find_sym(&symbols, "Point.x");
        assert_eq!(x.kind, "property");
        let l = find_sym(&symbols, "Point.len");
        assert_eq!(l.kind, "method");
    }

    #[test]
    fn swift_class_with_method() {
        let source =
            b"class Person {\n    public func greet() -> String { return \"hi\" }\n}\n";
        let (symbols, _, _) = parse_file(source, "swift", "t.swift").unwrap();
        let p = find_sym(&symbols, "Person");
        assert_eq!(p.kind, "class");
        let g = find_sym(&symbols, "Person.greet");
        assert_eq!(g.kind, "method");
        assert_eq!(g.visibility.as_deref(), Some("public"));
    }

    #[test]
    fn swift_protocol_surfaces_as_interface() {
        let source = b"protocol Greeter {\n    func greet() -> String\n}\n";
        let (symbols, _, _) = parse_file(source, "swift", "t.swift").unwrap();
        let i = find_sym(&symbols, "Greeter");
        assert_eq!(i.kind, "interface");
    }

    #[test]
    fn swift_extension_surfaces_as_class() {
        let source = b"extension Person {\n    func describe() -> String { return \"\" }\n}\n";
        let (symbols, _, _) = parse_file(source, "swift", "t.swift").unwrap();
        let e = find_sym(&symbols, "Person");
        assert_eq!(e.kind, "class");
        let d = find_sym(&symbols, "Person.describe");
        assert_eq!(d.kind, "method");
    }

    #[test]
    fn swift_top_level_const() {
        let source = b"let MAX_RETRIES: Int = 3\n";
        let (symbols, _, _) = parse_file(source, "swift", "t.swift").unwrap();
        let c = find_sym(&symbols, "MAX_RETRIES");
        assert_eq!(c.kind, "constant");
        assert!(c.sig.as_deref().unwrap_or("").contains('3'));
    }

    #[test]
    fn swift_visibility_mapping() {
        let source = b"private func p() {}\npublic func pb() {}\ninternal func i() {}\nfileprivate func fp() {}\nopen func o() {}\n";
        let (symbols, _, _) = parse_file(source, "swift", "t.swift").unwrap();
        assert_eq!(find_sym(&symbols, "p").visibility.as_deref(), Some("private"));
        assert_eq!(find_sym(&symbols, "pb").visibility.as_deref(), Some("public"));
        assert_eq!(find_sym(&symbols, "i").visibility.as_deref(), Some("internal"));
        // fileprivate → internal (sigil has three buckets)
        assert_eq!(find_sym(&symbols, "fp").visibility.as_deref(), Some("internal"));
        // open → public
        assert_eq!(find_sym(&symbols, "o").visibility.as_deref(), Some("public"));
    }

    #[test]
    fn swift_imports() {
        let source = b"import Foundation\nimport UIKit\n";
        let (symbols, _, refs) = parse_file(source, "swift", "t.swift").unwrap();
        let imports: Vec<_> = symbols.iter().filter(|s| s.kind == "import").collect();
        assert_eq!(imports.len(), 2);
        assert!(imports.iter().any(|s| s.name == "Foundation"));
        assert!(imports.iter().any(|s| s.name == "UIKit"));
        let import_refs: Vec<_> = refs.iter().filter(|r| r.kind == "import").collect();
        assert_eq!(import_refs.len(), 2);
    }

    #[test]
    fn swift_enum_cases() {
        let source = b"enum Direction {\n    case north\n    case south\n}\n";
        let (symbols, _, _) = parse_file(source, "swift", "t.swift").unwrap();
        let d = find_sym(&symbols, "Direction");
        assert_eq!(d.kind, "class");
        let n = find_sym(&symbols, "Direction.north");
        assert_eq!(n.kind, "constant");
    }
}
