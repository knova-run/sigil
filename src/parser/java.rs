//! Java symbol and text extraction.

use tree_sitter::{Node, Tree};

use crate::parser::format::{ReferenceEntry, SymbolEntry, TextEntry};
use crate::parser::helpers::*;
use crate::parser::treesitter::MAX_DEPTH;

/// Java-specific stopwords (keywords and common patterns)
const JAVA_STOPWORDS: &[&str] = &[
    // Keywords
    "null",
    "interface",
    "extends",
    "implements",
    "abstract",
    "final",
    "finally",
    "throws",
    "synchronized",
    "volatile",
    "transient",
    "native",
    "strictfp",
    "instanceof",
    "import",
    "package",
    // Primitive types
    "int",
    "long",
    "short",
    "byte",
    "float",
    "double",
    "boolean",
    "char",
    // Common class names (typically imported)
    "String",
    "Integer",
    "Long",
    "Double",
    "Float",
    "Boolean",
    "Object",
    "System",
    "Exception",
    "RuntimeException",
    "Override",
    "Deprecated",
    "List",
    "Map",
    "Set",
    "ArrayList",
    "HashMap",
    "HashSet",
    // Common variable patterns
    "args",
    "main",
];

/// Filter Java-specific stopwords from extracted tokens.
fn filter_java_tokens(tokens: Option<String>) -> Option<String> {
    tokens.and_then(|t| {
        let filtered: Vec<&str> = t
            .split_whitespace()
            .filter(|tok| !JAVA_STOPWORDS.contains(&tok.to_lowercase().as_str()))
            // Also filter out uppercase-only tokens (likely type names)
            .filter(|tok| !tok.chars().all(|c| c.is_uppercase() || c == '_'))
            .collect();
        if filtered.is_empty() {
            None
        } else {
            Some(filtered.join(" "))
        }
    })
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
    resolve_java_imports_tier2(symbols, references);
}

/// Tier-2 resolver: for each call ref whose name is `Class.method` where
/// `Class` matches a file-local Java `import` (last segment of the
/// dotted path), upgrade the edge from `confidence: None` to `Some(0.8)`
/// AND emit a sibling resolved-form edge `<full-import-path>/method`
/// also at `Some(0.8)`. Matches the Go resolver shape.
fn resolve_java_imports_tier2(
    symbols: &[SymbolEntry],
    references: &mut Vec<ReferenceEntry>,
) {
    use std::collections::HashMap;
    // Build short-name → full-path table from import symbols.
    let imports: HashMap<&str, &str> = symbols
        .iter()
        .filter(|s| s.kind == "import")
        .filter_map(|s| {
            let short = s.name.rsplit('.').next()?;
            if short.is_empty() {
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
        let Some((head, rest)) = r.name.split_once('.') else {
            continue;
        };
        let Some(&path) = imports.get(head) else {
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

// ---------------------------------------------------------------------------
// Builtin detection for filtering noisy references
// ---------------------------------------------------------------------------

/// Check if a name is a Java builtin or common library class/method.
fn is_java_builtin(name: &str) -> bool {
    matches!(
        name,
        // System methods
        "System"
        | "out"
        | "err"
        | "in"
        | "println"
        | "print"
        | "printf"
        | "exit"
        | "currentTimeMillis"
        | "nanoTime"
        | "gc"
        | "getenv"
        | "getProperty"
        // Object methods
        | "toString"
        | "equals"
        | "hashCode"
        | "getClass"
        | "clone"
        | "wait"
        | "notify"
        | "notifyAll"
        // String methods
        | "length"
        | "charAt"
        | "substring"
        | "indexOf"
        | "lastIndexOf"
        | "contains"
        | "startsWith"
        | "endsWith"
        | "replace"
        | "replaceAll"
        | "split"
        | "trim"
        | "toLowerCase"
        | "toUpperCase"
        | "format"
        | "valueOf"
        | "isEmpty"
        // Collection methods
        | "add"
        | "remove"
        | "get"
        | "set"
        | "size"
        | "clear"
        | "iterator"
        | "toArray"
        | "stream"
        | "forEach"
        | "put"
        | "containsKey"
        | "containsValue"
        | "keySet"
        | "values"
        | "entrySet"
        // Common types
        | "Object"
        | "String"
        | "Integer"
        | "Long"
        | "Double"
        | "Float"
        | "Boolean"
        | "Byte"
        | "Short"
        | "Character"
        | "Number"
        | "Math"
        | "Arrays"
        | "Collections"
        | "List"
        | "ArrayList"
        | "LinkedList"
        | "Map"
        | "HashMap"
        | "TreeMap"
        | "Set"
        | "HashSet"
        | "TreeSet"
        | "Queue"
        | "Deque"
        | "Stack"
        | "Optional"
        | "Stream"
        | "Collectors"
        | "Class"
        | "Enum"
        | "Exception"
        | "RuntimeException"
        | "Error"
        | "Throwable"
        | "Thread"
        | "Runnable"
        // Test framework
        | "assertEquals"
        | "assertTrue"
        | "assertFalse"
        | "assertNull"
        | "assertNotNull"
        | "assertThrows"
        | "fail"
    )
}

/// Check if a type name is a Java primitive or common type.
fn is_java_primitive_type(name: &str) -> bool {
    matches!(
        name,
        "int"
            | "long"
            | "short"
            | "byte"
            | "float"
            | "double"
            | "boolean"
            | "char"
            | "void"
            | "String"
            | "Integer"
            | "Long"
            | "Short"
            | "Byte"
            | "Float"
            | "Double"
            | "Boolean"
            | "Character"
            | "Void"
            | "Object"
            | "Number"
            | "Class"
    )
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
        "class_declaration" => {
            extract_class(
                node, source, file_path, parent_ctx, "class", symbols, texts, references, depth,
            );
            return;
        }
        "interface_declaration" => {
            extract_class(
                node,
                source,
                file_path,
                parent_ctx,
                "interface",
                symbols,
                texts,
                references,
                depth,
            );
            return;
        }
        "enum_declaration" => {
            extract_class(
                node, source, file_path, parent_ctx, "enum", symbols, texts, references, depth,
            );
            return;
        }
        "annotation_type_declaration" => {
            extract_class(
                node,
                source,
                file_path,
                parent_ctx,
                "annotation",
                symbols,
                texts,
                references,
                depth,
            );
            return;
        }
        "record_declaration" => {
            extract_class(
                node, source, file_path, parent_ctx, "struct", symbols, texts, references, depth,
            );
            return;
        }
        "method_declaration" => {
            extract_method(node, source, file_path, parent_ctx, symbols, references);
        }
        "constructor_declaration" => {
            extract_constructor(node, source, file_path, parent_ctx, symbols, references);
        }
        "field_declaration" => {
            extract_field(node, source, file_path, parent_ctx, symbols, references);
        }
        "import_declaration" => {
            extract_import(node, source, file_path, symbols, references);
        }
        "package_declaration" => {
            extract_package(node, source, file_path, symbols);
        }
        "line_comment" | "block_comment" => {
            extract_java_comment(node, source, file_path, parent_ctx, texts);
            return;
        }
        "string_literal" | "text_block" => {
            extract_string(node, source, file_path, parent_ctx, texts);
            return;
        }

        // --- Reference extraction ---
        "method_invocation" => {
            extract_call_ref(node, source, file_path, parent_ctx, references);
        }
        "object_creation_expression" => {
            extract_new_ref(node, source, file_path, parent_ctx, references);
        }

        _ => {}
    }

    // Recurse with Javadoc tracking — `/** … */` blocks immediately
    // preceding a class/method/field declaration attach as that item's doc.
    walk_children_with_docs(node, source, file_path, parent_ctx, symbols, texts, references, depth);
}

/// Iterate `node.children` while tracking preceding Javadoc-style block
/// comments. Each contiguous run of `/** … */` (or `/*! … */`) blocks is
/// attached to the next declaration as a kind="docstring" TextEntry.
#[allow(clippy::too_many_arguments)]
pub(crate) fn walk_children_with_docs(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
    texts: &mut Vec<TextEntry>,
    references: &mut Vec<ReferenceEntry>,
    depth: usize,
) {
    let mut cursor = node.walk();
    let mut pending_docs: Vec<String> = Vec::new();
    for child in node.children(&mut cursor) {
        let child_kind = child.kind();
        if matches!(child_kind, "line_comment" | "block_comment") {
            if let Some(text) = java_javadoc_text(child, source) {
                pending_docs.push(text);
            } else {
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
            continue;
        }
        if !pending_docs.is_empty() {
            if let Some(item_name) = java_item_name(child, source, parent_ctx) {
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

fn java_javadoc_text(node: Node, source: &[u8]) -> Option<String> {
    let raw = node_text(node, source);
    if raw.starts_with("/**") || raw.starts_with("/*!") {
        Some(strip_block_comment(&raw))
    } else {
        None
    }
}

/// Return the entity name a Java declaration would emit, mirroring the
/// names produced by extract_class / extract_method / extract_field /
/// extract_constructor so the docstring TextEntry's `parent` field joins
/// on equality with the resulting Entity's `name`.
fn java_item_name(node: Node, source: &[u8], parent_ctx: Option<&str>) -> Option<String> {
    let kind = node.kind();
    let qualify = |n: &str| match parent_ctx {
        Some(p) => format!("{p}.{n}"),
        None => n.to_string(),
    };
    match kind {
        "class_declaration"
        | "interface_declaration"
        | "enum_declaration"
        | "annotation_type_declaration"
        | "record_declaration"
        | "method_declaration"
        | "constructor_declaration" => find_child_by_field(node, "name")
            .map(|n| qualify(&node_text(n, source))),
        "field_declaration" => {
            // Walk into the variable_declarator for the first name.
            let mut c = node.walk();
            for ch in node.children(&mut c) {
                if ch.kind() == "variable_declarator"
                    && let Some(n) = find_child_by_field(ch, "name")
                {
                    return Some(qualify(&node_text(n, source)));
                }
            }
            None
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Reference extraction
// ---------------------------------------------------------------------------

/// Extract a method invocation reference.
fn extract_call_ref(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    references: &mut Vec<ReferenceEntry>,
) {
    let name = get_call_name(node, source);
    if name.is_empty() || is_java_builtin(&name) {
        return;
    }
    // Tier-1 confidence when the invocation has no `object` qualifier —
    // same-file resolution. Qualified invocations (`obj.method()`,
    // `pkg.Class.method()`) stay None until import-table resolution lands.
    let confidence = if find_child_by_field(node, "object").is_none() {
        Some(1.0_f64)
    } else {
        None
    };

    let line = node_line_range(node);
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

/// Extract a `new` expression reference (instantiation).
fn extract_new_ref(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    references: &mut Vec<ReferenceEntry>,
) {
    // The type is the first child of object_creation_expression
    let type_node = match find_child_by_field(node, "type") {
        Some(t) => t,
        None => return,
    };

    let name = get_type_name(type_node, source);
    if name.is_empty() || is_java_builtin(&name) || is_java_primitive_type(&name) {
        return;
    }

    let line = node_line_range(node);
    references.push(ReferenceEntry {
        file: file_path.to_string(),
        name,
        kind: "instantiation".to_string(),
        line,
        caller: parent_ctx.map(String::from),
        project: String::new(),
        confidence: None,
    });
}

/// Get the name of a method invocation.
fn get_call_name(node: Node, source: &[u8]) -> String {
    // method_invocation has "name" and optionally "object" fields
    let method_name = find_child_by_field(node, "name")
        .map(|n| node_text(n, source))
        .unwrap_or_default();

    if let Some(obj) = find_child_by_field(node, "object") {
        let obj_name = node_text(obj, source);
        if !obj_name.is_empty() {
            return format!("{}.{}", obj_name, method_name);
        }
    }

    method_name
}

/// Get the name of a type node.
fn get_type_name(node: Node, source: &[u8]) -> String {
    match node.kind() {
        "type_identifier" | "identifier" => node_text(node, source),
        "generic_type" => {
            // Generic<T> - extract the base type
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "type_identifier" {
                    return node_text(child, source);
                }
            }
            String::new()
        }
        "scoped_type_identifier" => {
            // com.example.MyClass
            node_text(node, source)
        }
        "superclass" | "super_interfaces" | "extends_interfaces" => {
            // Wrapper nodes - find the actual type child
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                let child_kind = child.kind();
                if matches!(
                    child_kind,
                    "type_identifier" | "generic_type" | "scoped_type_identifier"
                ) {
                    return get_type_name(child, source);
                }
            }
            String::new()
        }
        _ => String::new(),
    }
}

/// Extract type references from a type node.
fn extract_type_refs(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    references: &mut Vec<ReferenceEntry>,
) {
    let name = get_type_name(node, source);
    if name.is_empty() || is_java_primitive_type(&name) || is_java_builtin(&name) {
        return;
    }

    let line = node_line_range(node);
    references.push(ReferenceEntry {
        file: file_path.to_string(),
        name,
        kind: "type_annotation".to_string(),
        line,
        caller: parent_ctx.map(String::from),
        project: String::new(),
        confidence: None,
    });
}

#[allow(clippy::too_many_arguments)]
fn extract_class(
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
    let name = match find_child_by_field(node, "name") {
        Some(n) => node_text(n, source),
        None => return,
    };

    let line = node_line_range(node);
    let visibility = extract_java_visibility(node, source);

    // Build signature
    let _sig = build_class_signature(node, source, &name, kind);

    let full_name = if let Some(parent) = parent_ctx {
        format!("{parent}.{name}")
    } else {
        name.clone()
    };

    // Extract superclass reference
    if let Some(superclass) = find_child_by_field(node, "superclass") {
        extract_type_refs(superclass, source, file_path, Some(&full_name), references);
    }

    // Extract interfaces references
    if let Some(interfaces) = find_child_by_field(node, "interfaces") {
        let mut cursor = interfaces.walk();
        for child in interfaces.children(&mut cursor) {
            if child.kind() == "type_list" {
                let mut type_cursor = child.walk();
                for type_child in child.children(&mut type_cursor) {
                    extract_type_refs(type_child, source, file_path, Some(&full_name), references);
                }
            } else {
                extract_type_refs(child, source, file_path, Some(&full_name), references);
            }
        }
    }

    // Extract tokens from class body
    let tokens = find_child_by_field(node, "body")
        .and_then(|body| filter_java_tokens(extract_tokens(body, source)));

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

    // Walk class body with Javadoc tracking so that `/** … */` blocks before
    // each method/field attach as that member's doc.
    if let Some(body) = find_child_by_field(node, "body") {
        walk_children_with_docs(
            body,
            source,
            file_path,
            Some(&full_name),
            symbols,
            texts,
            references,
            depth,
        );
    }
}

fn extract_method(
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

    let line = node_line_range(node);
    let visibility = extract_java_visibility(node, source);
    let _sig = extract_signature_to_brace(node, source);

    let full_name = if let Some(parent) = parent_ctx {
        format!("{parent}.{name}")
    } else {
        name
    };

    // Extract return type reference
    if let Some(return_type) = find_child_by_field(node, "type") {
        extract_type_refs(return_type, source, file_path, Some(&full_name), references);
    }

    // Extract parameter type references
    if let Some(params) = find_child_by_field(node, "parameters") {
        let mut cursor = params.walk();
        for child in params.children(&mut cursor) {
            if (child.kind() == "formal_parameter" || child.kind() == "spread_parameter")
                && let Some(type_node) = find_child_by_field(child, "type")
            {
                extract_type_refs(type_node, source, file_path, Some(&full_name), references);
            }
        }
    }

    // Extract tokens from method body
    let tokens = find_child_by_field(node, "body")
        .and_then(|body| filter_java_tokens(extract_tokens(body, source)));

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

fn extract_constructor(
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

    let line = node_line_range(node);
    let visibility = extract_java_visibility(node, source);
    let _sig = extract_signature_to_brace(node, source);

    let full_name = if let Some(parent) = parent_ctx {
        format!("{parent}.{name}")
    } else {
        name
    };

    // Extract parameter type references
    if let Some(params) = find_child_by_field(node, "parameters") {
        let mut cursor = params.walk();
        for child in params.children(&mut cursor) {
            if (child.kind() == "formal_parameter" || child.kind() == "spread_parameter")
                && let Some(type_node) = find_child_by_field(child, "type")
            {
                extract_type_refs(type_node, source, file_path, Some(&full_name), references);
            }
        }
    }

    // Extract tokens from constructor body
    let tokens = find_child_by_field(node, "body")
        .and_then(|body| filter_java_tokens(extract_tokens(body, source)));

    push_symbol(
        symbols,
        file_path,
        full_name,
        "constructor",
        line,
        parent_ctx,
        tokens,
        None,
        Some(visibility),
    );
}

fn extract_field(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
    references: &mut Vec<ReferenceEntry>,
) {
    let line = node_line_range(node);
    let visibility = extract_java_visibility(node, source);

    // Check for static final → constant
    let is_static = has_modifier(node, source, "static");
    let is_final = has_modifier(node, source, "final");

    let kind = if is_static && is_final {
        "constant"
    } else {
        "property"
    };

    // Extract field type reference
    if let Some(type_node) = find_child_by_field(node, "type") {
        extract_type_refs(type_node, source, file_path, parent_ctx, references);
    }

    // Find declarators
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "variable_declarator"
            && let Some(name_node) = find_child_by_field(child, "name")
        {
            let name = node_text(name_node, source);

            let full_name = if let Some(parent) = parent_ctx {
                format!("{parent}.{name}")
            } else {
                name
            };

            // For static final fields (constants), surface the initializer as
            // sig so `code.context FOO` can return the literal value inline.
            // Plain fields (kind=property) skip this — the type already lives
            // in the surrounding declaration captured by signature.rs.
            let sig = if kind == "constant" {
                find_child_by_field(child, "value")
                    .map(|v| truncate_sig(&node_text(v, source)))
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
}

fn extract_import(
    node: Node,
    source: &[u8],
    file_path: &str,
    symbols: &mut Vec<SymbolEntry>,
    references: &mut Vec<ReferenceEntry>,
) {
    let line = node_line_range(node);

    // Get the import path
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "scoped_identifier" || child.kind() == "identifier" {
            let name = node_text(child, source);
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
            // Also add import reference
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
    }
}

fn extract_package(node: Node, source: &[u8], file_path: &str, symbols: &mut Vec<SymbolEntry>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "scoped_identifier" || child.kind() == "identifier" {
            let name = node_text(child, source);
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
}

fn extract_java_comment(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    texts: &mut Vec<TextEntry>,
) {
    extract_comment(node, source, file_path, parent_ctx, texts);
}

fn extract_java_visibility(node: Node, source: &[u8]) -> String {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifiers" {
            let text = node_text(child, source);
            if text.contains("public") {
                return "public".to_string();
            }
            if text.contains("protected") {
                return "internal".to_string();
            }
            if text.contains("private") {
                return "private".to_string();
            }
            // package-private (no explicit modifier)
            return "internal".to_string();
        }
    }
    "internal".to_string() // default: package-private
}

fn has_modifier(node: Node, source: &[u8], modifier: &str) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifiers" {
            let text = node_text(child, source);
            return text.contains(modifier);
        }
    }
    false
}

fn build_class_signature(node: Node, source: &[u8], name: &str, kind: &str) -> String {
    let type_params = find_child_by_field(node, "type_parameters")
        .map(|n| node_text(n, source))
        .unwrap_or_default();

    let extends = find_child_by_field(node, "superclass")
        .map(|n| format!(" extends {}", node_text(n, source)))
        .unwrap_or_default();

    let implements = find_child_by_field(node, "interfaces")
        .map(|n| format!(" implements {}", node_text(n, source)))
        .unwrap_or_default();

    format!("{kind} {name}{type_params}{extends}{implements}")
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
    fn test_java_class() {
        let source = b"public class Person {
    private String name;

    public Person(String name) {
        this.name = name;
    }

    public String getName() {
        return name;
    }

    private void helper() {}
}";
        let (symbols, _texts, _refs) = parse_file(source, "java", "test.java").unwrap();

        let person = find_sym(&symbols, "Person");
        assert_eq!(person.kind, "class");
        assert_eq!(person.visibility.as_deref(), Some("public"));
        // Token extraction extracts identifiers from class body
        // Token may be None if all identifiers are filtered as stopwords

        let name = find_sym(&symbols, "Person.name");
        assert_eq!(name.kind, "property");
        assert_eq!(name.visibility.as_deref(), Some("private"));

        let constructor = find_sym(&symbols, "Person.Person");
        assert_eq!(constructor.kind, "constructor");

        let get_name = find_sym(&symbols, "Person.getName");
        assert_eq!(get_name.kind, "method");
        assert_eq!(get_name.visibility.as_deref(), Some("public"));

        let helper = find_sym(&symbols, "Person.helper");
        assert_eq!(helper.visibility.as_deref(), Some("private"));
    }

    #[test]
    fn test_java_interface() {
        let source = b"public interface Runnable {
    void run();
    default void start() {}
}";
        let (symbols, _texts, _refs) = parse_file(source, "java", "test.java").unwrap();

        let runnable = find_sym(&symbols, "Runnable");
        assert_eq!(runnable.kind, "interface");
        assert_eq!(runnable.visibility.as_deref(), Some("public"));
    }

    #[test]
    fn test_java_enum() {
        let source = b"public enum Status {
    ACTIVE,
    INACTIVE,
    PENDING
}";
        let (symbols, _texts, _refs) = parse_file(source, "java", "test.java").unwrap();

        let status = find_sym(&symbols, "Status");
        assert_eq!(status.kind, "enum");
        assert_eq!(status.visibility.as_deref(), Some("public"));
    }

    #[test]
    fn test_java_fields() {
        let source = b"class Config {
    public static final int MAX_SIZE = 100;
    private int value;
    protected String name;
}";
        let (symbols, _texts, _refs) = parse_file(source, "java", "test.java").unwrap();

        let max_size = find_sym(&symbols, "Config.MAX_SIZE");
        assert_eq!(max_size.kind, "constant");
        assert_eq!(max_size.visibility.as_deref(), Some("public"));

        let value = find_sym(&symbols, "Config.value");
        assert_eq!(value.kind, "property");
        assert_eq!(value.visibility.as_deref(), Some("private"));

        let name = find_sym(&symbols, "Config.name");
        assert_eq!(name.visibility.as_deref(), Some("internal"));
    }

    #[test]
    fn java_imported_class_call_gets_tier2_two_edges() {
        // `import com.foo.Bar;` then `Bar.greet()` resolves via the
        // file-local import table. Emit TWO edges, both confidence 0.8:
        //   1. raw selector form `Bar.greet`
        //   2. resolved qualified form `com.foo.Bar/greet`
        let source = b"import com.foo.Bar;\nclass C { void caller() { Bar.greet(); } }\n";
        let (_, _, refs) = parse_file(source, "java", "T.java").unwrap();
        let raw = refs
            .iter()
            .find(|r| r.kind == "call" && r.name == "Bar.greet")
            .expect("raw selector Bar.greet");
        assert_eq!(raw.confidence, Some(0.8));
        let resolved = refs
            .iter()
            .find(|r| r.kind == "call" && r.name == "com.foo.Bar/greet")
            .expect("resolved com.foo.Bar/greet");
        assert_eq!(resolved.confidence, Some(0.8));
    }

    #[test]
    fn java_bare_call_gets_tier1_confidence() {
        let source = b"class C { void caller() { helper(); obj.method(); } void helper() {} }\n";
        let (_, _, refs) = parse_file(source, "java", "T.java").unwrap();
        let bare = refs
            .iter()
            .find(|r| r.kind == "call" && r.name == "helper")
            .expect("helper() bare call");
        assert_eq!(bare.confidence, Some(1.0));
        let member = refs.iter().find(|r| r.kind == "call" && r.name == "obj.method");
        if let Some(m) = member {
            assert_eq!(m.confidence, None);
        }
    }

    #[test]
    fn test_java_methods() {
        let source = b"class Calculator {
    public int add(int a, int b) {
        return a + b;
    }

    protected double divide(double x, double y) {
        return x / y;
    }

    private void log(String msg) {}
}";
        let (symbols, _texts, _refs) = parse_file(source, "java", "test.java").unwrap();

        let add = find_sym(&symbols, "Calculator.add");
        assert_eq!(add.kind, "method");
        assert_eq!(add.visibility.as_deref(), Some("public"));

        let divide = find_sym(&symbols, "Calculator.divide");
        assert_eq!(divide.visibility.as_deref(), Some("internal"));

        let log = find_sym(&symbols, "Calculator.log");
        assert_eq!(log.visibility.as_deref(), Some("private"));
    }

    #[test]
    fn test_java_imports() {
        let source = b"import java.util.List;
import java.util.*;
import java.io.File;";
        let (symbols, _texts, _refs) = parse_file(source, "java", "test.java").unwrap();

        // Check at least one import is extracted
        let imports: Vec<_> = symbols.iter().filter(|s| s.kind == "import").collect();
        assert!(!imports.is_empty());

        // Check if we have any java.util imports
        let has_util = symbols
            .iter()
            .any(|s| s.name.contains("util") && s.kind == "import");
        assert!(has_util);
    }

    #[test]
    fn test_java_package() {
        let source = b"package com.example.app;

class Foo {}";
        let (symbols, _texts, _refs) = parse_file(source, "java", "test.java").unwrap();

        let pkg = symbols.iter().find(|s| s.kind == "module").unwrap();
        assert_eq!(pkg.name, "com.example.app");
    }

    #[test]
    fn test_java_visibility_default() {
        let source = b"class Foo {
    void packagePrivate() {}
}";
        let (symbols, _texts, _refs) = parse_file(source, "java", "test.java").unwrap();

        let foo = find_sym(&symbols, "Foo");
        assert_eq!(foo.visibility.as_deref(), Some("internal")); // default = package-private

        let method = find_sym(&symbols, "Foo.packagePrivate");
        assert_eq!(method.visibility.as_deref(), Some("internal"));
    }

    #[test]
    fn test_java_comments() {
        let source = b"/** Javadoc comment */
class Documented {}

// Single line
/* Block comment */";
        let (_symbols, texts, _refs) = parse_file(source, "java", "test.java").unwrap();
        assert!(texts.iter().any(|t| t.kind == "comment"));
    }

    #[test]
    fn test_java_call_references() {
        let source = b"class Foo {
    void bar() {
        myService.doSomething();
        helper();
    }
}";
        let (_symbols, _texts, refs) = parse_file(source, "java", "test.java").unwrap();

        let calls: Vec<_> = refs.iter().filter(|r| r.kind == "call").collect();
        assert!(calls.iter().any(|r| r.name.contains("doSomething")));
    }

    #[test]
    fn test_java_import_references() {
        let source = b"import java.util.List;
import java.io.File;";
        let (_symbols, _texts, refs) = parse_file(source, "java", "test.java").unwrap();

        let imports: Vec<_> = refs.iter().filter(|r| r.kind == "import").collect();
        assert!(!imports.is_empty());
        assert!(imports.iter().any(|r| r.name.contains("util")));
    }

    #[test]
    fn test_java_instantiation_references() {
        let source = b"class Foo {
    void bar() {
        MyService svc = new MyService();
        CustomClass obj = new CustomClass();
    }
}";
        let (_symbols, _texts, refs) = parse_file(source, "java", "test.java").unwrap();

        let instantiations: Vec<_> = refs.iter().filter(|r| r.kind == "instantiation").collect();
        assert!(instantiations.iter().any(|r| r.name == "MyService"));
        assert!(instantiations.iter().any(|r| r.name == "CustomClass"));
    }

    #[test]
    fn test_java_type_references() {
        let source = b"class Dog extends Animal implements Runnable {
    private MyService service;

    public CustomResult process(InputData data) {
        return null;
    }
}";
        let (_symbols, _texts, refs) = parse_file(source, "java", "test.java").unwrap();

        let type_refs: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == "type_annotation")
            .collect();
        assert!(type_refs.iter().any(|r| r.name == "Animal"));
        assert!(type_refs.iter().any(|r| r.name == "MyService"));
        assert!(type_refs.iter().any(|r| r.name == "CustomResult"));
        assert!(type_refs.iter().any(|r| r.name == "InputData"));
    }
}
