//! Rust symbol and text extraction.

use tree_sitter::{Node, Tree};

use crate::parser::format::{ReferenceEntry, SymbolEntry, TextEntry};
use crate::parser::helpers::*;
use crate::parser::treesitter::MAX_DEPTH;

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
    // Prevent stack overflow on deeply nested code
    if depth > MAX_DEPTH {
        return;
    }

    let kind = node.kind();

    match kind {
        "function_item" => {
            extract_function(
                node, source, file_path, parent_ctx, symbols, texts, references, depth,
            );
            return; // handled recursively
        }
        "struct_item" => {
            extract_struct(node, source, file_path, parent_ctx, symbols, references);
        }
        "enum_item" => {
            extract_named_symbol(node, source, file_path, "enum", parent_ctx, symbols);
        }
        "trait_item" => {
            extract_named_symbol(node, source, file_path, "interface", parent_ctx, symbols);
        }
        "type_item" => {
            extract_named_symbol(node, source, file_path, "type_alias", parent_ctx, symbols);
        }
        "mod_item" => {
            extract_named_symbol(node, source, file_path, "module", parent_ctx, symbols);
        }
        "const_item" | "static_item" => {
            extract_rust_const(node, source, file_path, parent_ctx, symbols);
        }
        "use_declaration" => {
            extract_use(node, source, file_path, symbols, references);
        }
        "impl_item" => {
            extract_impl(node, source, file_path, symbols, texts, references, depth);
            return; // impl is handled recursively inside extract_impl
        }
        "call_expression" => {
            extract_call(node, source, file_path, parent_ctx, references);
        }
        "line_comment" | "block_comment" => {
            extract_rust_comment(node, source, file_path, parent_ctx, texts);
            return;
        }
        "string_literal" | "raw_string_literal" => {
            extract_string(node, source, file_path, parent_ctx, texts);
            return;
        }
        "macro_invocation" => {
            // Try to parse macro body as Rust code (works for cfg_*, feature gates, etc.)
            try_parse_macro_body(
                node, source, file_path, parent_ctx, symbols, texts, references, depth,
            );
            return;
        }
        _ => {}
    }

    // Recurse into children, threading a "pending docs" buffer across siblings
    // so that contiguous `///` / `//!` / `/** */` doc-comments immediately
    // preceding an item can be attached as that item's doc TextEntry.
    let mut cursor = node.walk();
    let mut pending_docs: Vec<String> = Vec::new();
    for child in node.children(&mut cursor) {
        let child_kind = child.kind();
        if matches!(child_kind, "line_comment" | "block_comment") {
            if let Some(line) = rust_doc_comment_text(child, source) {
                pending_docs.push(line);
                // Doc-comments still get emitted as TextEntry rows by
                // `extract_rust_comment` (kind="docstring") for consumers
                // querying texts directly.
                extract_rust_comment(child, source, file_path, parent_ctx, texts);
            } else {
                // Regular non-doc comment severs the chain but still goes
                // through the regular comment extraction so kind="comment"
                // TextEntries continue to be emitted.
                pending_docs.clear();
                extract_rust_comment(child, source, file_path, parent_ctx, texts);
            }
        } else {
            if !pending_docs.is_empty() {
                if let Some(item_name) = rust_item_name(child, source, parent_ctx) {
                    texts.push(TextEntry {
                        file: file_path.to_string(),
                        kind: "docstring".to_string(),
                        line: node_line_range(child),
                        text: pending_docs.join("\n"),
                        parent: Some(item_name),
                        project: String::new(),
                    });
                }
                pending_docs.clear();
            }
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
}

/// Return the cleaned doc-comment body when `node` is a `///`, `//!`,
/// `/**`, or `/*!` comment. Returns `None` for plain `//` and `/*`
/// comments — they aren't doc-comments and shouldn't be attached to
/// the next item.
fn rust_doc_comment_text(node: Node, source: &[u8]) -> Option<String> {
    let raw = node_text(node, source);
    if raw.starts_with("///") || raw.starts_with("//!") {
        Some(strip_doc_comment_prefix(&raw))
    } else if raw.starts_with("/**") || raw.starts_with("/*!") {
        Some(strip_block_comment(&raw))
    } else {
        None
    }
}

/// Return the qualified entity name a Rust item AST node would emit,
/// matching the names produced by `extract_function`, `extract_struct`,
/// `extract_named_symbol`, `extract_rust_const`, and `extract_impl`.
/// `None` for nodes that don't produce an entity row.
fn rust_item_name(node: Node, source: &[u8], parent_ctx: Option<&str>) -> Option<String> {
    let kind = node.kind();
    match kind {
        "function_item"
        | "struct_item"
        | "enum_item"
        | "trait_item"
        | "type_item"
        | "mod_item"
        | "const_item"
        | "static_item" => {
            let name = node_text(find_child_by_field(node, "name")?, source);
            // Methods/inner items inherit a parent context — match the
            // qualified name produced by extract_function in that case.
            if matches!(kind, "function_item") {
                if let Some(parent) = parent_ctx {
                    return Some(format!("{parent}.{name}"));
                }
            }
            Some(name)
        }
        "impl_item" => Some(extract_impl_type_name(node, source)),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_function(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
    texts: &mut Vec<TextEntry>,
    references: &mut Vec<ReferenceEntry>,
    depth: usize,
) {
    let name = match find_child_by_field(node, "name") {
        Some(n) => node_text(n, source),
        None => return,
    };

    let visibility = extract_visibility(node, source);
    let line = node_line_range(node);

    // Extract tokens from function body for FTS
    let tokens = find_child_by_field(node, "body")
        .and_then(|body| extract_tokens(body, source))
        .map(|t| filter_rust_tokens(&t));

    let kind = if parent_ctx.is_some() {
        "method"
    } else {
        "function"
    };

    let full_name = if let Some(parent) = parent_ctx {
        format!("{parent}.{name}")
    } else {
        name
    };

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

    // Extract type references from parameters
    if let Some(params) = find_child_by_field(node, "parameters") {
        extract_type_refs_from_node(params, source, file_path, Some(&full_name), references);
    }

    // Extract type references from return type
    if let Some(ret_type) = find_child_by_field(node, "return_type") {
        extract_type_refs_from_node(ret_type, source, file_path, Some(&full_name), references);
    }

    // Recurse into function body with function name as context
    if let Some(body) = find_child_by_field(node, "body") {
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

/// Extract a `const_item` or `static_item` with its RHS value as the sig.
/// Same kind ("constant") for both — agents asking "what does X equal" don't
/// care about the static/const distinction, and rolling them together keeps
/// the entity surface tight.
fn extract_rust_const(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
) {
    let name = match find_child_by_field(node, "name") {
        Some(n) => node_text(n, source),
        None => return,
    };

    let visibility = extract_visibility(node, source);
    let line = node_line_range(node);
    let sig = find_child_by_field(node, "value")
        .map(|v| truncate_sig(&node_text(v, source)));

    symbols.push(SymbolEntry {
        file: file_path.to_string(),
        name,
        kind: "constant".to_string(),
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

fn extract_named_symbol(
    node: Node,
    source: &[u8],
    file_path: &str,
    kind: &str,
    parent_ctx: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
) {
    let name = match find_child_by_field(node, "name") {
        Some(n) => node_text(n, source),
        None => return,
    };

    let visibility = extract_visibility(node, source);
    let line = node_line_range(node);

    push_symbol(
        symbols,
        file_path,
        name,
        kind,
        line,
        parent_ctx,
        None,
        None,
        Some(visibility),
    );
}

fn extract_struct(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
    references: &mut Vec<ReferenceEntry>,
) {
    let name = match find_child_by_field(node, "name") {
        Some(n) => node_text(n, source),
        None => return,
    };

    let visibility = extract_visibility(node, source);
    let line = node_line_range(node);

    push_symbol(
        symbols,
        file_path,
        name.clone(),
        "struct",
        line,
        parent_ctx,
        None,
        None,
        Some(visibility),
    );

    // Extract type references from struct fields
    if let Some(body) = find_child_by_field(node, "body") {
        extract_type_refs_from_node(body, source, file_path, Some(&name), references);
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_impl(
    node: Node,
    source: &[u8],
    file_path: &str,
    symbols: &mut Vec<SymbolEntry>,
    texts: &mut Vec<TextEntry>,
    references: &mut Vec<ReferenceEntry>,
    depth: usize,
) {
    let impl_type_name = extract_impl_type_name(node, source);
    let line = node_line_range(node);
    let visibility = extract_visibility(node, source);

    let trait_name = find_child_by_field(node, "trait").map(|n| node_text(n, source));

    let kind = if trait_name.is_some() {
        "trait_impl"
    } else {
        "impl"
    };

    // impl blocks are containers, no meaningful tokens
    push_symbol(
        symbols,
        file_path,
        impl_type_name.clone(),
        kind,
        line,
        None,
        None,
        None,
        Some(visibility),
    );

    // Walk children of the body to find methods. Same doc-comment
    // attachment as in `walk_node` — `///` / `/** */` lines immediately
    // preceding a method are emitted as a docstring TextEntry parented
    // to the method's qualified name.
    if let Some(body) = find_child_by_field(node, "body") {
        let mut cursor = body.walk();
        let mut pending_docs: Vec<String> = Vec::new();
        for child in body.children(&mut cursor) {
            let child_kind = child.kind();
            if matches!(child_kind, "line_comment" | "block_comment") {
                if let Some(line) = rust_doc_comment_text(child, source) {
                    pending_docs.push(line);
                } else {
                    pending_docs.clear();
                }
                // Both doc and non-doc comments inside an impl flow through
                // extract_rust_comment so the existing TextEntry stream is
                // unchanged for consumers that query texts directly.
                extract_rust_comment(child, source, file_path, Some(&impl_type_name), texts);
                continue;
            }
            if !pending_docs.is_empty() {
                if let Some(item_name) =
                    rust_item_name(child, source, Some(&impl_type_name))
                {
                    texts.push(TextEntry {
                        file: file_path.to_string(),
                        kind: "docstring".to_string(),
                        line: node_line_range(child),
                        text: pending_docs.join("\n"),
                        parent: Some(item_name),
                        project: String::new(),
                    });
                }
                pending_docs.clear();
            }
            match child_kind {
                "function_item" => {
                    extract_function(
                        child,
                        source,
                        file_path,
                        Some(&impl_type_name),
                        symbols,
                        texts,
                        references,
                        depth + 1,
                    );
                }
                "const_item" => {
                    extract_rust_const(
                        child,
                        source,
                        file_path,
                        Some(&impl_type_name),
                        symbols,
                    );
                }
                "type_item" => {
                    extract_named_symbol(
                        child,
                        source,
                        file_path,
                        "type_alias",
                        Some(&impl_type_name),
                        symbols,
                    );
                }
                _ => {
                    walk_node(
                        child,
                        source,
                        file_path,
                        Some(&impl_type_name),
                        symbols,
                        texts,
                        references,
                        depth + 1,
                    );
                }
            }
        }
    }
}

fn extract_impl_type_name(node: Node, source: &[u8]) -> String {
    if let Some(type_node) = find_child_by_field(node, "type") {
        return node_text(type_node, source);
    }
    "Unknown".to_string()
}

fn extract_use(
    node: Node,
    source: &[u8],
    file_path: &str,
    symbols: &mut Vec<SymbolEntry>,
    references: &mut Vec<ReferenceEntry>,
) {
    let line = node_line_range(node);
    let visibility = extract_visibility(node, source);

    if let Some(arg) = find_child_by_field(node, "argument") {
        extract_use_paths(
            arg,
            source,
            file_path,
            &line,
            &visibility,
            symbols,
            references,
        );
    }
}

fn extract_use_paths(
    node: Node,
    source: &[u8],
    file_path: &str,
    line: &[u32; 2],
    visibility: &str,
    symbols: &mut Vec<SymbolEntry>,
    references: &mut Vec<ReferenceEntry>,
) {
    match node.kind() {
        "use_as_clause" => {
            if let Some(path_node) = find_child_by_field(node, "path") {
                let name = node_text(path_node, source);
                let alias = find_child_by_field(node, "alias").map(|n| node_text(n, source));
                push_symbol(
                    symbols,
                    file_path,
                    name.clone(),
                    "import",
                    *line,
                    None,
                    None,
                    alias,
                    Some(visibility.to_string()),
                );
                // Also record as import reference
                references.push(ReferenceEntry {
                    file: file_path.to_string(),
                    name,
                    kind: "import".to_string(),
                    line: *line,
                    caller: None,
                    project: String::new(),
                    confidence: None,
                });
            }
        }
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_use_paths(
                    child, source, file_path, line, visibility, symbols, references,
                );
            }
        }
        "use_wildcard" | "scoped_use_list" => {
            let name = node_text(node, source);
            push_symbol(
                symbols,
                file_path,
                name.clone(),
                "import",
                *line,
                None,
                None,
                None,
                Some(visibility.to_string()),
            );
            // Also record as import reference
            references.push(ReferenceEntry {
                file: file_path.to_string(),
                name,
                kind: "import".to_string(),
                line: *line,
                caller: None,
                project: String::new(),
                confidence: None,
            });
        }
        "scoped_identifier" | "identifier" => {
            let name = node_text(node, source);
            push_symbol(
                symbols,
                file_path,
                name.clone(),
                "import",
                *line,
                None,
                None,
                None,
                Some(visibility.to_string()),
            );
            // Also record as import reference
            references.push(ReferenceEntry {
                file: file_path.to_string(),
                name,
                kind: "import".to_string(),
                line: *line,
                caller: None,
                project: String::new(),
                confidence: None,
            });
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_use_paths(
                    child, source, file_path, line, visibility, symbols, references,
                );
            }
        }
    }
}

fn extract_visibility(node: Node, source: &[u8]) -> String {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            let text = node_text(child, source);
            if text.contains("pub(crate)") || text.contains("pub(super)") || text.contains("pub(in")
            {
                return "internal".to_string();
            }
            return "public".to_string();
        }
    }
    "private".to_string()
}

/// Extract a function call as a reference.
fn extract_call(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    references: &mut Vec<ReferenceEntry>,
) {
    let line = node_line_range(node);

    // The "function" field contains the callable expression
    let Some(func) = find_child_by_field(node, "function") else {
        return;
    };

    // Tier-1 confidence on bare identifiers; scoped / field-expression
    // calls stay at None until full `use`-alias resolution lands.
    let (name, confidence) = match func.kind() {
        "identifier" => (node_text(func, source), Some(1.0_f64)),
        "scoped_identifier" | "field_expression" => (node_text(func, source), None),
        _ => return,
    };

    // Skip macros and builtins
    if is_rust_builtin_call(&name) {
        return;
    }

    references.push(ReferenceEntry {
        file: file_path.to_string(),
        name,
        kind: "call".to_string(),
        line,
        caller: parent_ctx.map(String::from),
        project: String::new(),
        confidence,
    });
}

/// Check if a call is to a Rust builtin/macro that we want to skip.
fn is_rust_builtin_call(name: &str) -> bool {
    let base = name.split("::").last().unwrap_or(name);
    matches!(
        base,
        "println"
            | "print"
            | "eprintln"
            | "eprint"
            | "format"
            | "write"
            | "writeln"
            | "panic"
            | "assert"
            | "assert_eq"
            | "assert_ne"
            | "debug_assert"
            | "debug_assert_eq"
            | "debug_assert_ne"
            | "unreachable"
            | "unimplemented"
            | "todo"
            | "vec"
            | "dbg"
            | "cfg"
            | "include"
            | "include_str"
            | "include_bytes"
            | "concat"
            | "stringify"
            | "env"
            | "option_env"
            | "file"
            | "line"
            | "column"
            | "module_path"
            | "Default"
            | "Clone"
            | "Copy"
            | "Drop"
            | "Some"
            | "None"
            | "Ok"
            | "Err"
    )
}

/// Extract type references from a node (parameters, return types, fields).
fn extract_type_refs_from_node(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    references: &mut Vec<ReferenceEntry>,
) {
    let mut stack = vec![node];

    while let Some(n) = stack.pop() {
        match n.kind() {
            "type_identifier" => {
                let name = node_text(n, source);
                if !is_rust_primitive_type(&name) {
                    references.push(ReferenceEntry {
                        file: file_path.to_string(),
                        name,
                        kind: "type_annotation".to_string(),
                        line: node_line_range(n),
                        caller: parent_ctx.map(String::from),
                        project: String::new(),
                        confidence: None,
                    });
                }
            }
            "scoped_type_identifier" => {
                let name = node_text(n, source);
                if !is_rust_primitive_type(&name) {
                    references.push(ReferenceEntry {
                        file: file_path.to_string(),
                        name,
                        kind: "type_annotation".to_string(),
                        line: node_line_range(n),
                        caller: parent_ctx.map(String::from),
                        project: String::new(),
                        confidence: None,
                    });
                }
                continue; // Don't recurse into children
            }
            "generic_type" => {
                // Extract the base type name from generic
                if let Some(type_node) = find_child_by_field(n, "type") {
                    let name = node_text(type_node, source);
                    if !is_rust_primitive_type(&name) {
                        references.push(ReferenceEntry {
                            file: file_path.to_string(),
                            name,
                            kind: "type_annotation".to_string(),
                            line: node_line_range(type_node),
                            caller: parent_ctx.map(String::from),
                            project: String::new(),
                            confidence: None,
                        });
                    }
                }
                // Also extract type arguments
                if let Some(args) = find_child_by_field(n, "type_arguments") {
                    stack.push(args);
                }
                continue;
            }
            _ => {}
        }

        // Recurse into children
        let mut cursor = n.walk();
        for child in n.children(&mut cursor) {
            stack.push(child);
        }
    }
}

/// Check if a type is a Rust primitive.
fn is_rust_primitive_type(name: &str) -> bool {
    matches!(
        name,
        "i8" | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "f32"
            | "f64"
            | "bool"
            | "char"
            | "str"
            | "Self"
    )
}

/// Rust-specific stopwords to filter from tokens.
const RUST_STOPWORDS: &[&str] = &[
    // Keywords
    "self", "Self", "crate", "mod", "pub", "mut", "ref", "let", "type", "impl", "trait", "fn",
    "where", "loop", "match", "unsafe", "async", "await", "dyn", "move", "use", "as", "Some",
    "None", "Ok", "Err", // Common std types/modules
    "std", "core", "alloc", // Very common short names in Rust
    "cx", "rx", "tx", "io", "buf", "drop",
];

/// Filter Rust-specific tokens from the extracted token string.
fn filter_rust_tokens(tokens: &str) -> String {
    tokens
        .split_whitespace()
        .filter(|t| !RUST_STOPWORDS.contains(t))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Try to parse macro body as valid Rust code.
/// Works for cfg_*, feature gates, and other macros that wrap valid Rust items.
/// DSL macros (html!, query!, etc.) will fail to parse cleanly and are skipped.
#[allow(clippy::too_many_arguments)]
fn try_parse_macro_body(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
    texts: &mut Vec<TextEntry>,
    references: &mut Vec<ReferenceEntry>,
    depth: usize,
) {
    // Find the token_tree (macro body)
    let token_tree = match node
        .children(&mut node.walk())
        .find(|c| c.kind() == "token_tree")
    {
        Some(tt) => tt,
        None => return,
    };

    // Extract text between braces
    let body_text = node_text(token_tree, source);
    let body_trimmed = body_text.trim();

    // Strip outer braces/parens if present
    let inner = if (body_trimmed.starts_with('{') && body_trimmed.ends_with('}'))
        || (body_trimmed.starts_with('(') && body_trimmed.ends_with(')'))
        || (body_trimmed.starts_with('[') && body_trimmed.ends_with(']'))
    {
        &body_trimmed[1..body_trimmed.len() - 1]
    } else {
        body_trimmed
    };

    // Try to parse as Rust code
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .is_err()
    {
        return;
    }

    let tree = match parser.parse(inner.as_bytes(), None) {
        Some(t) => t,
        None => return,
    };

    // Only proceed if the parse was clean (no errors = valid Rust code)
    if tree.root_node().has_error() {
        return;
    }

    // Extract symbols from the parsed macro body
    // Adjust line numbers to be relative to the original file
    let macro_start_line = token_tree.start_position().row as u32;

    let mut macro_symbols = Vec::new();
    let mut macro_texts = Vec::new();
    let mut macro_refs = Vec::new();

    walk_node(
        tree.root_node(),
        inner.as_bytes(),
        file_path,
        parent_ctx,
        &mut macro_symbols,
        &mut macro_texts,
        &mut macro_refs,
        depth + 1,
    );

    // Adjust line numbers and add to output
    for mut sym in macro_symbols {
        sym.line[0] += macro_start_line;
        sym.line[1] += macro_start_line;
        symbols.push(sym);
    }

    for mut text in macro_texts {
        text.line[0] += macro_start_line;
        text.line[1] += macro_start_line;
        texts.push(text);
    }

    for mut r in macro_refs {
        r.line[0] += macro_start_line;
        r.line[1] += macro_start_line;
        references.push(r);
    }
}

/// Rust-specific comment extraction (handles ///, //!, /**, etc.)
fn extract_rust_comment(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    texts: &mut Vec<TextEntry>,
) {
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
            .unwrap_or_else(|| panic!("symbol not found: {name}"))
    }

    #[test]
    fn test_rust_functions() {
        let source = b"pub fn hello(name: &str) -> String {
    format!(\"Hello, {}!\", name)
}

fn private_helper() {
    println!(\"private\");
}";
        let (symbols, _texts, _refs) = parse_file(source, "rust", "test.rs").unwrap();
        assert_eq!(symbols.len(), 2);

        let hello = find_sym(&symbols, "hello");
        assert_eq!(hello.kind, "function");
        // Tokens contain identifiers from function body (format, name)
        // Token may be None if all identifiers are filtered as stopwords
        assert_eq!(hello.visibility.as_deref(), Some("public"));

        let helper = find_sym(&symbols, "private_helper");
        assert_eq!(helper.kind, "function");
        assert_eq!(helper.visibility.as_deref(), Some("private"));
    }

    #[test]
    fn test_rust_struct() {
        let source = b"pub struct Point {
    pub x: i32,
    pub y: i32,
}

struct Private;";
        let (symbols, _texts, _refs) = parse_file(source, "rust", "test.rs").unwrap();
        assert_eq!(symbols.len(), 2);

        let point = find_sym(&symbols, "Point");
        assert_eq!(point.kind, "struct");
        assert_eq!(point.visibility.as_deref(), Some("public"));

        let priv_struct = find_sym(&symbols, "Private");
        assert_eq!(priv_struct.kind, "struct");
        assert_eq!(priv_struct.visibility.as_deref(), Some("private"));
    }

    #[test]
    fn test_rust_impl() {
        let source = b"struct Foo;

impl Foo {
    pub fn new() -> Self {
        Foo
    }

    fn private_method(&self) {}
}";
        let (symbols, _texts, _refs) = parse_file(source, "rust", "test.rs").unwrap();
        assert_eq!(symbols.len(), 4); // struct + impl + 2 methods

        let _impl_sym = find_sym(&symbols, "Foo");
        // First is struct, second is impl
        let _impl_entry = symbols.iter().find(|s| s.kind == "impl").unwrap();
        // Impl tokens now contain the signature "impl Foo"

        let new = find_sym(&symbols, "Foo.new");
        assert_eq!(new.kind, "method");
        assert_eq!(new.parent.as_deref(), Some("Foo"));
        assert_eq!(new.visibility.as_deref(), Some("public"));

        let priv_method = find_sym(&symbols, "Foo.private_method");
        assert_eq!(priv_method.kind, "method");
        assert_eq!(priv_method.visibility.as_deref(), Some("private"));
    }

    #[test]
    fn test_rust_trait() {
        let source = b"pub trait Display {
    fn fmt(&self) -> String;
}

impl Display for Foo {
    fn fmt(&self) -> String {
        String::new()
    }
}";
        let (symbols, _texts, _refs) = parse_file(source, "rust", "test.rs").unwrap();

        let trait_sym = symbols
            .iter()
            .find(|s| s.name == "Display" && s.kind == "interface")
            .unwrap();
        assert_eq!(trait_sym.visibility.as_deref(), Some("public"));

        let trait_impl = symbols.iter().find(|s| s.kind == "trait_impl").unwrap();
        // Trait impls are containers, no tokens
        assert!(trait_impl.tokens.is_none());
    }

    #[test]
    fn test_rust_use() {
        let source = b"use std::collections::HashMap;
use std::io::{self, Read};
pub use std::fmt::Debug;";
        let (symbols, _texts, _refs) = parse_file(source, "rust", "test.rs").unwrap();

        let hashmap = symbols
            .iter()
            .find(|s| s.name == "std::collections::HashMap")
            .unwrap();
        assert_eq!(hashmap.kind, "import");
        assert_eq!(hashmap.visibility.as_deref(), Some("private"));

        let debug = symbols.iter().find(|s| s.name.contains("Debug")).unwrap();
        assert_eq!(debug.kind, "import");
        assert_eq!(debug.visibility.as_deref(), Some("public"));
    }

    #[test]
    fn test_rust_enum() {
        let source = b"pub enum Result<T, E> {
    Ok(T),
    Err(E),
}";
        let (symbols, _texts, _refs) = parse_file(source, "rust", "test.rs").unwrap();
        let result = find_sym(&symbols, "Result");
        assert_eq!(result.kind, "enum");
        assert_eq!(result.visibility.as_deref(), Some("public"));
    }

    #[test]
    fn test_rust_mod() {
        let source = b"pub mod utils;
mod private_mod;";
        let (symbols, _texts, _refs) = parse_file(source, "rust", "test.rs").unwrap();

        let utils = find_sym(&symbols, "utils");
        assert_eq!(utils.kind, "module");
        assert_eq!(utils.visibility.as_deref(), Some("public"));

        let priv_mod = find_sym(&symbols, "private_mod");
        assert_eq!(priv_mod.visibility.as_deref(), Some("private"));
    }

    #[test]
    fn test_rust_const() {
        let source = b"pub const MAX: usize = 100;
static GLOBAL: i32 = 0;";
        let (symbols, _texts, _refs) = parse_file(source, "rust", "test.rs").unwrap();

        let max = find_sym(&symbols, "MAX");
        assert_eq!(max.kind, "constant");
        assert_eq!(max.visibility.as_deref(), Some("public"));

        let global = find_sym(&symbols, "GLOBAL");
        assert_eq!(global.kind, "constant");
        assert_eq!(global.visibility.as_deref(), Some("private"));
    }

    #[test]
    fn test_rust_comments() {
        let source = b"/// This is a doc comment
/// for the function
pub fn documented() {}

// Regular comment
fn helper() {}";
        let (_symbols, texts, _refs) = parse_file(source, "rust", "test.rs").unwrap();
        assert!(texts.iter().any(|t| t.kind == "comment"));
    }

    #[test]
    fn rust_bare_call_gets_tier1_confidence() {
        // Tier 1: bare `identifier` call → confidence 1.0. Scoped /
        // field-expression calls stay at None until full `use`-alias
        // resolution lands.
        let source = b"fn caller() { helper(); module::other(); }\nfn helper() {}\n";
        let (_, _, refs) = parse_file(source, "rust", "t.rs").unwrap();
        let bare = refs
            .iter()
            .find(|r| r.kind == "call" && r.name == "helper")
            .expect("helper() call");
        assert_eq!(bare.confidence, Some(1.0));
        let scoped = refs.iter().find(|r| r.kind == "call" && r.name == "module::other");
        if let Some(s) = scoped {
            assert_eq!(s.confidence, None);
        }
    }

    #[test]
    fn test_rust_call_references() {
        let source = b"fn caller() {
    some_function();
    module::nested_call();
    obj.method_call();
}

fn some_function() {}";
        let (_symbols, _texts, refs) = parse_file(source, "rust", "test.rs").unwrap();

        let call_refs: Vec<_> = refs.iter().filter(|r| r.kind == "call").collect();
        assert!(
            call_refs.iter().any(|r| r.name == "some_function"),
            "should find some_function call"
        );
        assert!(
            call_refs
                .iter()
                .any(|r| r.name.contains("module::nested_call")),
            "should find nested call"
        );
    }

    #[test]
    fn test_rust_import_references() {
        let source = b"use std::collections::HashMap;
use std::io::{Read, Write};";
        let (_symbols, _texts, refs) = parse_file(source, "rust", "test.rs").unwrap();

        let import_refs: Vec<_> = refs.iter().filter(|r| r.kind == "import").collect();
        assert!(
            import_refs
                .iter()
                .any(|r| r.name == "std::collections::HashMap"),
            "should find HashMap import"
        );
        assert!(
            import_refs.iter().any(|r| r.name.contains("Read")),
            "should find Read import"
        );
    }

    #[test]
    fn test_rust_type_references() {
        let source = b"struct MyStruct {
    field: OtherType,
}

fn process(input: CustomType) -> ResultType {
    todo!()
}";
        let (_symbols, _texts, refs) = parse_file(source, "rust", "test.rs").unwrap();

        let type_refs: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == "type_annotation")
            .collect();
        assert!(
            type_refs.iter().any(|r| r.name == "OtherType"),
            "should find OtherType reference"
        );
        assert!(
            type_refs.iter().any(|r| r.name == "CustomType"),
            "should find CustomType reference"
        );
        assert!(
            type_refs.iter().any(|r| r.name == "ResultType"),
            "should find ResultType reference"
        );
    }

    #[test]
    fn test_rust_macro_body_parsing() {
        // Test that we can extract symbols from inside macro invocations
        let source = b"cfg_rt! {
    pub fn spawn<F>(future: F) -> JoinHandle<F::Output> {
        todo!()
    }

    pub struct Runtime {
        inner: Inner,
    }
}

// DSL macros should be skipped (won't parse as valid Rust)
html! {
    <div class=\"foo\">Hello</div>
}

// Regular function outside macro
fn regular_fn() {}";
        let (symbols, _texts, _refs) = parse_file(source, "rust", "test.rs").unwrap();

        // Should find spawn function inside cfg_rt! macro
        let spawn = symbols.iter().find(|s| s.name == "spawn");
        assert!(spawn.is_some(), "should find spawn function inside macro");
        let spawn = spawn.unwrap();
        assert_eq!(spawn.kind, "function");
        assert_eq!(spawn.visibility.as_deref(), Some("public"));

        // Should find Runtime struct inside cfg_rt! macro
        let runtime = symbols.iter().find(|s| s.name == "Runtime");
        assert!(runtime.is_some(), "should find Runtime struct inside macro");
        assert_eq!(runtime.unwrap().kind, "struct");

        // Should still find regular function
        let regular = find_sym(&symbols, "regular_fn");
        assert_eq!(regular.kind, "function");

        // Should NOT find anything from html! macro (DSL, invalid Rust)
        assert!(
            !symbols.iter().any(|s| s.name.contains("div")),
            "should not parse DSL macro content"
        );
    }
}
