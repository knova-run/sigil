//! Scala symbol and text extraction.
//!
//! Targets the `tree-sitter-scala` grammar (v0.26). Covers the core
//! top-level / template-body declarations: function (def), class, trait,
//! object, val/var, package, import.
//!
//! Nested members (defs / vals inside a class, trait, or object body) are
//! emitted with a qualified `Outer.member` name, mirroring the Java/Kotlin
//! extractor convention so downstream tooling can join consistently.

use tree_sitter::{Node, Tree};

use crate::parser::format::{ReferenceEntry, SymbolEntry, TextEntry};
use crate::parser::helpers::*;
use crate::parser::treesitter::MAX_DEPTH;

/// Scala keywords and common builtin types we don't want surfacing as tokens.
const SCALA_STOPWORDS: &[&str] = &[
    "def",
    "val",
    "var",
    "class",
    "object",
    "trait",
    "case",
    "sealed",
    "abstract",
    "final",
    "lazy",
    "implicit",
    "given",
    "using",
    "extends",
    "with",
    "override",
    "private",
    "protected",
    "package",
    "import",
    "match",
    "if",
    "else",
    "for",
    "yield",
    "return",
    // Common Scala types / standard library names. Lowercase here because
    // the filter calls `tok.to_lowercase()` before contains() —
    // PascalCase entries would silently never match.
    "int",
    "long",
    "short",
    "float",
    "double",
    "boolean",
    "char",
    "string",
    "unit",
    "any",
    "nothing",
    "option",
    "some",
    "none",
    "list",
    "seq",
    "map",
    "set",
    "vector",
    "array",
];

fn filter_scala_tokens(tokens: Option<String>) -> Option<String> {
    tokens.and_then(|t| {
        let filtered: Vec<&str> = t
            .split_whitespace()
            .filter(|tok| !SCALA_STOPWORDS.contains(&tok.to_lowercase().as_str()))
            .filter(|tok| !tok.chars().all(|c| c.is_uppercase() || c == '_'))
            .collect();
        if filtered.is_empty() {
            None
        } else {
            Some(filtered.join(" "))
        }
    })
}

/// Common Scala stdlib / pseudo-keyword calls we filter from the call graph.
fn is_scala_builtin(name: &str) -> bool {
    matches!(
        name,
        "println" | "print" | "assert" | "require" | "Some" | "None" | "List" | "Map" | "Set" | "Option"
    )
}

fn is_scala_primitive(name: &str) -> bool {
    matches!(
        name,
        "Int" | "Long" | "Short" | "Float" | "Double" | "Boolean" | "Char" | "String" | "Unit" | "Any" | "Nothing"
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
    resolve_scala_imports_tier2(symbols, references);
}

/// Tier-2 resolver: upgrade member-head calls (`Head.member`) whose head
/// matches a file-local `import` binding to confidence 0.8, and emit a
/// resolved edge using `/` as the path/member separator.
///
/// Short-name resolution rules:
///   * `import a.b.C`               → short = `C`, path = `a.b.C`
///   * `import a.b.{C => D}`        → short = `D`, path = `a.b.C` (Scala 2)
///   * `import a.b.c as e`          → short = `e`, path = `a.b.c` (Scala 3)
///   * Wildcard (`a.b._`) is skipped — namespace-level shadowing needs
///     cross-file analysis to resolve safely.
fn resolve_scala_imports_tier2(symbols: &[SymbolEntry], references: &mut Vec<ReferenceEntry>) {
    use std::collections::HashMap;
    let imports: HashMap<String, &str> = symbols
        .iter()
        .filter(|s| s.kind == "import")
        .filter_map(|s| {
            let short = match s.alias.as_deref() {
                Some(a) if !a.is_empty() => a.to_string(),
                _ => s.name.rsplit('.').next()?.to_string(),
            };
            if short.is_empty() || short == "_" {
                None
            } else {
                Some((short, s.name.as_str()))
            }
        })
        .collect();
    if imports.is_empty() {
        return;
    }
    let mut added: Vec<ReferenceEntry> = Vec::new();
    for r in references.iter_mut() {
        if r.kind != "call" {
            continue;
        }
        let (head, rest) = match r.name.split_once('.') {
            Some((h, t)) => (h.to_string(), t.to_string()),
            None => (r.name.clone(), String::new()),
        };
        let Some(&path) = imports.get(&head) else { continue };
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
        "package_clause" => {
            extract_package(node, source, file_path, parent_ctx, symbols, texts, references, depth);
            return;
        }
        "import_declaration" => {
            extract_import(node, source, file_path, symbols, references);
            return;
        }
        "class_definition" => {
            extract_class_like(
                node, source, file_path, parent_ctx, "class", symbols, texts, references, depth,
            );
            return;
        }
        "trait_definition" => {
            extract_class_like(
                node, source, file_path, parent_ctx, "interface", symbols, texts, references, depth,
            );
            return;
        }
        "object_definition" => {
            extract_class_like(
                node, source, file_path, parent_ctx, "object", symbols, texts, references, depth,
            );
            return;
        }
        "function_definition" => {
            extract_function(node, source, file_path, parent_ctx, symbols);
            // Walk into body for call references and nested defs.
            if let Some(body) = node.child_by_field_name("body") {
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
        "val_definition" | "var_definition" => {
            extract_value(node, source, file_path, parent_ctx, symbols);
            // Walk into the RHS expression for call references.
            if let Some(val_expr) = node.child_by_field_name("value") {
                let mut cursor = val_expr.walk();
                for child in val_expr.children(&mut cursor) {
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
            return;
        }
        "comment" | "block_comment" => {
            extract_scala_comment(node, source, file_path, parent_ctx, texts);
            return;
        }
        "string" | "interpolated_string_expression" => {
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

fn qualify(parent_ctx: Option<&str>, name: &str) -> String {
    match parent_ctx {
        Some(p) if !p.is_empty() => format!("{p}.{name}"),
        _ => name.to_string(),
    }
}

/// Visibility extraction. Scala default is public. `private` / `protected`
/// (with or without `[scope]`) both map to "private" in sigil's two-level
/// public/private surface. The `private`/`protected` keyword lives inside
/// the `access_modifier` named child of the `modifiers` node.
fn extract_scala_visibility(node: Node, source: &[u8]) -> String {
    if let Some(mods) = first_child_of_kind(node, "modifiers") {
        let mut cursor = mods.walk();
        for child in mods.children(&mut cursor) {
            if child.kind() == "access_modifier" {
                let raw = node_text(child, source);
                let trimmed = raw.trim_start();
                if trimmed.starts_with("private") || trimmed.starts_with("protected") {
                    return "private".to_string();
                }
            }
        }
    }
    "public".to_string()
}

/// Check whether the `modifiers` child of a declaration node contains a
/// specific anonymous keyword (e.g. "final", "sealed", "abstract").
fn has_modifier_token(node: Node, source: &[u8], token: &str) -> bool {
    if let Some(mods) = first_child_of_kind(node, "modifiers") {
        let mut cursor = mods.walk();
        for child in mods.children(&mut cursor) {
            // Anonymous (un-named) children of `modifiers` carry the keyword text.
            if !child.is_named() && node_text(child, source) == token {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Declarations
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn extract_package(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
    texts: &mut Vec<TextEntry>,
    references: &mut Vec<ReferenceEntry>,
    depth: usize,
) {
    // package_clause has a `name` field of type `package_identifier`. Some
    // package_clause forms wrap a body (chained `package a.b { ... }`); when
    // present, recurse into it under the package as parent context for
    // nested members. The package symbol itself is emitted as `module`.
    if let Some(name_node) = node.child_by_field_name("name") {
        let name = node_text(name_node, source);
        let line = node_line_range(node);
        push_symbol(
            symbols,
            file_path,
            name.clone(),
            "module",
            line,
            None,
            None,
            None,
            Some("public".to_string()),
        );

        // Optional body: bracketed `package a.b { ... }` (Scala 2 form).
        // Members inside the braces are package-scoped top-level
        // declarations — `def`s are functions, not methods — so we walk
        // with `parent_ctx = None` rather than forwarding the package
        // name. The package itself is already surfaced as a `module`
        // symbol above; the namespace is recoverable via the file path
        // + the module entry.
        if let Some(body) = node.child_by_field_name("body") {
            let mut cursor = body.walk();
            for child in body.children(&mut cursor) {
                walk_node(
                    child,
                    source,
                    file_path,
                    None,
                    symbols,
                    texts,
                    references,
                    depth + 1,
                );
            }
        }
        return;
    }

    // Fallback: walk children using the existing parent context.
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

fn extract_import(
    node: Node,
    source: &[u8],
    file_path: &str,
    symbols: &mut Vec<SymbolEntry>,
    references: &mut Vec<ReferenceEntry>,
) {
    let line = node_line_range(node);

    // Path: dotted identifier sequence (the `path` field). Each
    // `_identifier` along the dotted path is tagged with the `path`
    // field, so we iterate and re-join with `.`. Selectors and aliases
    // live under `namespace_selectors` / `as_renamed_identifier` children.
    // We emit one symbol per imported name:
    //   `import a.b.{C, D => E}`  ->  a.b.C, a.b.D (alias=E)
    //   `import a.b.c`            ->  a.b.c
    //   `import a.b.c as e`       ->  a.b.c (alias=e)  // Scala 3
    //   `import a.b._`            ->  a.b._
    let path_text = collect_import_path(node, source);

    let mut emitted_any = false;

    // Look for namespace_selectors `{X, Y => Z, _}` or single as_renamed_identifier.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "namespace_selectors" => {
                let mut inner = child.walk();
                for sel in child.children(&mut inner) {
                    if !sel.is_named() {
                        continue;
                    }
                    let (sel_name, sel_alias) = selector_name_and_alias(sel, source);
                    let Some(sel_name) = sel_name else { continue };
                    let full = if path_text.is_empty() {
                        sel_name
                    } else {
                        format!("{path_text}.{sel_name}")
                    };
                    push_symbol(
                        symbols,
                        file_path,
                        full.clone(),
                        "import",
                        line,
                        None,
                        None,
                        sel_alias,
                        Some("private".to_string()),
                    );
                    references.push(ReferenceEntry {
                        file: file_path.to_string(),
                        name: full,
                        kind: "import".to_string(),
                        line,
                        caller: None,
                        project: String::new(),
                    confidence: None,
    });
                    emitted_any = true;
                }
            }
            "namespace_wildcard" => {
                let full = if path_text.is_empty() {
                    "_".to_string()
                } else {
                    format!("{path_text}._")
                };
                push_symbol(
                    symbols,
                    file_path,
                    full.clone(),
                    "import",
                    line,
                    None,
                    None,
                    None,
                    Some("private".to_string()),
                );
                references.push(ReferenceEntry {
                    file: file_path.to_string(),
                    name: full,
                    kind: "import".to_string(),
                    line,
                    caller: None,
                    project: String::new(),
                confidence: None,
    });
                emitted_any = true;
            }
            "as_renamed_identifier" => {
                // Scala 3: `import a.b.c as e` at the top level.
                let name_node = child.child_by_field_name("name");
                let alias_node = child.child_by_field_name("alias");
                let name = name_node.map(|n| node_text(n, source)).unwrap_or_default();
                let alias = alias_node.map(|n| node_text(n, source));
                let full = if path_text.is_empty() {
                    name
                } else if name.is_empty() {
                    path_text.clone()
                } else {
                    format!("{path_text}.{name}")
                };
                push_symbol(
                    symbols,
                    file_path,
                    full.clone(),
                    "import",
                    line,
                    None,
                    None,
                    alias,
                    Some("private".to_string()),
                );
                references.push(ReferenceEntry {
                    file: file_path.to_string(),
                    name: full,
                    kind: "import".to_string(),
                    line,
                    caller: None,
                    project: String::new(),
                confidence: None,
    });
                emitted_any = true;
            }
            _ => {}
        }
    }

    if !emitted_any {
        if path_text.is_empty() {
            return;
        }
        push_symbol(
            symbols,
            file_path,
            path_text.clone(),
            "import",
            line,
            None,
            None,
            None,
            Some("private".to_string()),
        );
        references.push(ReferenceEntry {
            file: file_path.to_string(),
            name: path_text,
            kind: "import".to_string(),
            line,
            caller: None,
            project: String::new(),
        confidence: None,
    });
    }
}

/// Collect the dotted `path` of an `import_declaration` (or
/// `_namespace_expression`) by walking `children_by_field_name("path")`.
/// The grammar tags each identifier on the dotted path with the `path`
/// field, so we re-join them with `.` to reconstruct the source form.
fn collect_import_path(node: Node, source: &[u8]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut cursor = node.walk();
    for child in node.children_by_field_name("path", &mut cursor) {
        if child.is_named() {
            let t = node_text(child, source);
            if !t.is_empty() {
                parts.push(t);
            }
        }
    }
    parts.join(".")
}

/// For a child of `namespace_selectors`, return its imported name and
/// optional alias. Handles plain identifiers and `as_renamed_identifier`
/// / `arrow_renamed_identifier` (Scala 2 `=>` form).
fn selector_name_and_alias(node: Node, source: &[u8]) -> (Option<String>, Option<String>) {
    match node.kind() {
        "identifier" | "type_identifier" | "operator_identifier" => {
            (Some(node_text(node, source)), None)
        }
        "as_renamed_identifier" | "arrow_renamed_identifier" => {
            let name = node
                .child_by_field_name("name")
                .map(|n| node_text(n, source));
            let alias = node
                .child_by_field_name("alias")
                .map(|n| node_text(n, source));
            (name, alias)
        }
        "namespace_wildcard" | "wildcard" => (Some("_".to_string()), None),
        _ => (None, None),
    }
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
    let name = match node.child_by_field_name("name") {
        Some(n) => node_text(n, source),
        None => return,
    };

    let line = node_line_range(node);
    let visibility = extract_scala_visibility(node, source);
    let full_name = qualify(parent_ctx, &name);

    let tokens = node
        .child_by_field_name("body")
        .and_then(|body| filter_scala_tokens(extract_tokens(body, source)));

    push_symbol(
        symbols,
        file_path,
        full_name.clone(),
        kind,
        line,
        parent_ctx,
        tokens,
        None,
        Some(visibility),
    );

    if let Some(body) = node.child_by_field_name("body") {
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
    node.child_by_field_name("name").map(|n| node_text(n, source))
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
    let visibility = extract_scala_visibility(node, source);
    let full_name = qualify(parent_ctx, &name);

    // Kind: top-level / direct-object members surface as "function"; defs
    // nested inside a class or trait surface as "method", matching the
    // Kotlin/Java convention.
    let kind = if parent_ctx.is_some() { "method" } else { "function" };

    let tokens = node
        .child_by_field_name("body")
        .and_then(|body| filter_scala_tokens(extract_tokens(body, source)));

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

fn extract_value(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
) {
    let line = node_line_range(node);
    let visibility = extract_scala_visibility(node, source);
    let is_val = node.kind() == "val_definition";
    let is_final = has_modifier_token(node, source, "final");

    // Collect each identifier in the binding pattern. Common cases:
    //   val x: Int = 1                    -> single identifier
    //   val (a, b) = pair                 -> tuple pattern → multiple names
    //   val Pattern(a, b) = obj           -> extractor (we skip the matched names)
    //   val x, y = 1 (rare)               -> `identifiers` node with multiple
    let pattern = match node.child_by_field_name("pattern") {
        Some(p) => p,
        None => return,
    };

    let mut names: Vec<String> = Vec::new();
    collect_pattern_names(pattern, source, &mut names);
    if names.is_empty() {
        return;
    }

    // Try to grab the RHS value text for constants so `code.context` shows
    // the literal value inline (matches the Java/Rust/Kotlin convention).
    let value_text = node
        .child_by_field_name("value")
        .map(|v| node_text(v, source));

    for name in names {
        let is_caps = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit());

        // Scala's idiomatic constant is `val NAME` (all caps) or `final val`.
        // `var` is never a constant. Inside a class/trait, a plain (non-caps,
        // non-final) `val` is a property; at file scope it's a variable.
        let kind = if !is_val {
            // var
            if parent_ctx.is_some() {
                "property"
            } else {
                "variable"
            }
        } else if is_caps || is_final {
            "constant"
        } else if parent_ctx.is_some() {
            "property"
        } else {
            "variable"
        };

        let sig = if kind == "constant" {
            value_text.as_deref().map(truncate_sig)
        } else {
            None
        };

        let full_name = qualify(parent_ctx, &name);
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

/// Recursively pull identifier names out of a `val`/`var` binding pattern.
/// Handles simple identifiers, `identifiers` (comma-list), and tuple
/// patterns. Extractor patterns (`Pattern(a, b)`) contribute their bound
/// variable names too.
fn collect_pattern_names(node: Node, source: &[u8], out: &mut Vec<String>) {
    match node.kind() {
        "identifier" => out.push(node_text(node, source)),
        "identifiers" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "identifier" {
                    out.push(node_text(child, source));
                }
            }
        }
        _ => {
            // Recurse for tuple_pattern, extractor patterns, etc.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.is_named() {
                    collect_pattern_names(child, source, out);
                }
            }
        }
    }
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
    // call_expression has a `function` field. Pull its text; drop builtins
    // and bare primitive constructor-like calls.
    let function = match node.child_by_field_name("function") {
        Some(f) => f,
        None => return,
    };
    let (name, confidence) = match function.kind() {
        "identifier" | "operator_identifier" => (node_text(function, source), Some(0.95_f64)),
        "field_expression" | "generic_function" => (node_text(function, source), None),
        _ => (node_text(function, source), None),
    };
    if name.is_empty() {
        return;
    }

    let leaf = name.rsplit('.').next().unwrap_or(&name);
    if is_scala_builtin(leaf) || is_scala_primitive(leaf) {
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

fn extract_scala_comment(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    texts: &mut Vec<TextEntry>,
) {
    // Scala uses C-style `//` and `/* */` (with `/** */` Scaladoc).
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
            .unwrap_or_else(|| {
                panic!(
                    "symbol not found: {name}\nhave: {:?}",
                    symbols.iter().map(|s| (&s.name, &s.kind)).collect::<Vec<_>>()
                )
            })
    }

    #[test]
    fn scala_bare_call_gets_tier1_confidence() {
        let source = b"def caller(): Unit = { helper(); obj.method() }\ndef helper(): Unit = ()\n";
        let (_, _, refs) = parse_file(source, "scala", "t.scala").unwrap();
        let bare = refs
            .iter()
            .find(|r| r.kind == "call" && r.name == "helper")
            .expect("helper() bare call");
        assert_eq!(bare.confidence, Some(0.95));
        let field = refs.iter().find(|r| r.kind == "call" && r.name == "obj.method");
        if let Some(f) = field {
            assert_eq!(f.confidence, None);
        }
    }

    #[test]
    fn scala_imported_object_call_gets_tier2_two_edges() {
        // `import a.b.C` then `C.greet()`:
        //   * raw `C.greet` upgraded to 0.8
        //   * resolved `a.b.C/greet` at 0.8
        let source = b"import a.b.C\n\ndef caller(): Unit = { C.greet() }\n";
        let (_, _, refs) = parse_file(source, "scala", "t.scala").unwrap();
        let raw = refs
            .iter()
            .find(|r| r.kind == "call" && r.name == "C.greet")
            .expect("raw C.greet call");
        assert_eq!(raw.confidence, Some(0.8));
        let resolved = refs
            .iter()
            .find(|r| r.kind == "call" && r.name == "a.b.C/greet")
            .expect("resolved a.b.C/greet form");
        assert_eq!(resolved.confidence, Some(0.8));
    }

    #[test]
    fn scala_renamed_import_call_resolves_tier2() {
        // Scala 2: `import a.b.{C => D}` then `D.greet()`:
        //   * raw `D.greet` upgraded to 0.8
        //   * resolved `a.b.C/greet` at 0.8
        let source = b"import a.b.{C => D}\n\ndef caller(): Unit = { D.greet() }\n";
        let (_, _, refs) = parse_file(source, "scala", "t.scala").unwrap();
        let raw = refs
            .iter()
            .find(|r| r.kind == "call" && r.name == "D.greet")
            .expect("raw D.greet call");
        assert_eq!(raw.confidence, Some(0.8));
        let resolved = refs
            .iter()
            .find(|r| r.kind == "call" && r.name == "a.b.C/greet")
            .expect("resolved a.b.C/greet form");
        assert_eq!(resolved.confidence, Some(0.8));
    }

    #[test]
    fn scala_top_level_function() {
        let source = b"def greet(name: String): String = \"hi \" + name\n";
        let (symbols, _, _) = parse_file(source, "scala", "t.scala").unwrap();
        let g = find_sym(&symbols, "greet");
        assert_eq!(g.kind, "function");
        assert_eq!(g.visibility.as_deref(), Some("public"));
    }

    #[test]
    fn scala_class_with_method() {
        let source = b"class Person(val name: String) {\n  def greet(): String = \"hi \" + name\n}\n";
        let (symbols, _, _) = parse_file(source, "scala", "t.scala").unwrap();
        let p = find_sym(&symbols, "Person");
        assert_eq!(p.kind, "class");
        let g = find_sym(&symbols, "Person.greet");
        assert_eq!(g.kind, "method");
    }

    #[test]
    fn scala_trait() {
        let source = b"trait Greeter {\n  def greet(): String\n}\n";
        let (symbols, _, _) = parse_file(source, "scala", "t.scala").unwrap();
        let i = find_sym(&symbols, "Greeter");
        assert_eq!(i.kind, "interface");
    }

    #[test]
    fn scala_object() {
        let source = b"object Singleton {\n  def work(): Unit = ()\n}\n";
        let (symbols, _, _) = parse_file(source, "scala", "t.scala").unwrap();
        let s = find_sym(&symbols, "Singleton");
        assert_eq!(s.kind, "object");
        let w = find_sym(&symbols, "Singleton.work");
        // `work` lives directly inside an `object` — surface as method.
        assert_eq!(w.kind, "method");
    }

    #[test]
    fn scala_top_level_const() {
        let source = b"val MAX_RETRIES: Int = 3\n";
        let (symbols, _, _) = parse_file(source, "scala", "t.scala").unwrap();
        let c = find_sym(&symbols, "MAX_RETRIES");
        assert_eq!(c.kind, "constant");
        assert!(c.sig.as_deref().unwrap_or("").contains("3"));
    }

    #[test]
    fn scala_visibility() {
        let source =
            b"private def p(): Unit = ()\ndef pub(): Unit = ()\nprotected def pr(): Unit = ()\n";
        let (symbols, _, _) = parse_file(source, "scala", "t.scala").unwrap();
        assert_eq!(find_sym(&symbols, "p").visibility.as_deref(), Some("private"));
        assert_eq!(find_sym(&symbols, "pub").visibility.as_deref(), Some("public"));
        assert_eq!(find_sym(&symbols, "pr").visibility.as_deref(), Some("private"));
    }

    #[test]
    fn scala_package() {
        let source = b"package com.example.app\n\ndef foo(): Unit = ()\n";
        let (symbols, _, _) = parse_file(source, "scala", "t.scala").unwrap();
        let pkg = symbols.iter().find(|s| s.kind == "module").unwrap();
        assert_eq!(pkg.name, "com.example.app");
    }

    #[test]
    fn scala_bracketed_package_def_is_function_not_method() {
        // Scala 2's bracketed `package a.b { def foo() = ... }` form:
        // `def`s inside the braces are top-level package-scoped functions,
        // not class members. They must classify as `function`. The bug
        // was that the walker forwarded the package name as `parent_ctx`
        // into `extract_function`, which then saw `parent_ctx.is_some()`
        // and emitted `method`.
        let source = b"package com.example {\n  def helper(): Int = 1\n}\n";
        let (symbols, _, _) = parse_file(source, "scala", "t.scala").unwrap();
        let h = symbols
            .iter()
            .find(|s| s.name == "helper")
            .expect("helper symbol present");
        assert_eq!(
            h.kind, "function",
            "package-scoped def must be a function, got {:?}",
            h.kind,
        );
    }

    #[test]
    fn scala_sealed_class() {
        let source = b"sealed class Shape\n";
        let (symbols, _, _) = parse_file(source, "scala", "t.scala").unwrap();
        let s = find_sym(&symbols, "Shape");
        assert_eq!(s.kind, "class");
    }
}
