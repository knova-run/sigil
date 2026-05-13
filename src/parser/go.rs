//! Go symbol and text extraction.

use std::collections::HashMap;

use tree_sitter::{Node, Tree};

use crate::parser::format::{ReferenceEntry, SymbolEntry, TextEntry};
use crate::parser::helpers::*;
use crate::parser::treesitter::MAX_DEPTH;

/// File-local Go import-resolution table.
///
/// Maps the bare identifier a Go file uses to refer to a package — the
/// alias when one is given (`import io "io/ioutil"` ⇒ `"io"`), otherwise
/// the last segment of the import path (`import "encoding/json"` ⇒
/// `"json"`) — to the fully-qualified import path.
///
/// Built once per file before symbol/ref walking, then passed read-only to
/// the call extractor. Used to upgrade an unresolved `pkg.Func` selector
/// into a confidence-tagged edge against the package path.
type ImportTable = HashMap<String, String>;

pub fn extract(
    tree: &Tree,
    source: &[u8],
    file_path: &str,
    symbols: &mut Vec<SymbolEntry>,
    texts: &mut Vec<TextEntry>,
    references: &mut Vec<ReferenceEntry>,
) {
    let root = tree.root_node();
    // Pre-pass: walk the whole tree just for `import_declaration` nodes so
    // call/ref extraction below sees a complete alias map even when imports
    // appear deeper than the file's top level (rare, but valid Go).
    let imports = collect_imports(root, source);
    walk_node(
        root, source, file_path, None, symbols, texts, references, 0, &imports,
    );
}

/// Walk the parse tree once just to populate the per-file `ImportTable`.
fn collect_imports(root: Node, source: &[u8]) -> ImportTable {
    let mut table = ImportTable::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == "import_spec" {
            // path is mandatory; alias optional
            if let Some(path_node) = find_child_by_field(n, "path") {
                let path = strip_string_quotes(&node_text(path_node, source));
                if !path.is_empty() {
                    let alias = find_child_by_field(n, "name").map(|a| node_text(a, source));
                    let local_name = match alias.as_deref() {
                        // Blank/dot imports introduce no usable local name.
                        Some("_") | Some(".") => continue,
                        Some(a) => a.to_string(),
                        None => path.rsplit('/').next().unwrap_or(&path).to_string(),
                    };
                    if !local_name.is_empty() {
                        table.insert(local_name, path);
                    }
                }
            }
            continue;
        }
        let mut cursor = n.walk();
        for child in n.children(&mut cursor) {
            stack.push(child);
        }
    }
    table
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
    imports: &ImportTable,
) {
    // Prevent stack overflow on deeply nested code
    if depth > MAX_DEPTH {
        return;
    }

    let kind = node.kind();

    match kind {
        "function_declaration" => {
            extract_function(
                node, source, file_path, symbols, texts, references, depth, imports,
            );
            return; // handled recursively
        }
        "method_declaration" => {
            extract_method(
                node, source, file_path, symbols, texts, references, depth, imports,
            );
            return; // handled recursively
        }
        "type_declaration" => {
            extract_type_decl(node, source, file_path, symbols, texts, references);
            return; // handled recursively
        }
        "type_spec" => {
            extract_type_spec(
                node, source, file_path, parent_ctx, symbols, texts, references,
            );
            return;
        }
        "var_declaration" | "const_declaration" => {
            extract_var_const(node, source, file_path, parent_ctx, symbols);
            // Also emit type_annotation refs for the var_spec's
            // explicit type (`var x *Engine` or `var x Engine`). The
            // walker recursion below visits the value expression but
            // skips the `type` field — it's a leaf node from the
            // walker's perspective. Without this, top-level
            // declarations of `*Engine` are unreachable from `sigil
            // callers Engine`.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if !matches!(child.kind(), "var_spec" | "const_spec") {
                    continue;
                }
                if let Some(typ) = find_child_by_field(child, "type") {
                    extract_type_refs_from_node(
                        typ, source, file_path, parent_ctx, references,
                    );
                }
            }
        }
        "import_declaration" => {
            extract_imports(node, source, file_path, symbols, references);
        }
        "package_clause" => {
            extract_package(node, source, file_path, symbols);
        }
        "call_expression" => {
            extract_call(node, source, file_path, parent_ctx, references, imports);
        }
        "composite_literal" => {
            // `&Engine{...}` / `Engine{...}` / `[]Engine{...}` — emit a
            // type_annotation ref for the literal's type so the type
            // surfaces in `sigil callers Engine`. Recurse afterwards
            // so nested literals and call expressions inside still get
            // walked normally.
            if let Some(typ) = find_child_by_field(node, "type") {
                extract_type_refs_from_node(typ, source, file_path, parent_ctx, references);
            }
        }
        "comment" => {
            extract_go_comment(node, source, file_path, parent_ctx, texts);
            return;
        }
        "interpreted_string_literal" | "raw_string_literal" => {
            extract_string(node, source, file_path, parent_ctx, texts);
            return;
        }
        _ => {}
    }

    // Recurse, threading a "pending docs" buffer across siblings to support
    // godoc — a `//` comment block immediately preceding a declaration with
    // no blank line in between is treated as that declaration's doc.
    let mut cursor = node.walk();
    let mut pending_docs: Vec<String> = Vec::new();
    let mut last_comment_end_line: Option<u32> = None;
    for child in node.children(&mut cursor) {
        if child.kind() == "comment" {
            let raw = node_text(child, source);
            // godoc: line comments only (`/* */` block comments aren't the
            // godoc convention and downstream tooling rarely renders them).
            if raw.starts_with("//") {
                let cleaned = raw.strip_prefix("//").unwrap_or(&raw).trim().to_string();
                let line = node_line_range(child);
                // If there's a gap (blank line) since the previous comment,
                // the previous block belongs to nothing — discard and start
                // a new run with this line.
                if let Some(prev_end) = last_comment_end_line {
                    if line[0] > prev_end + 1 {
                        pending_docs.clear();
                    }
                }
                pending_docs.push(cleaned);
                last_comment_end_line = Some(line[1]);
            }
            // Still emit the raw comment as a TextEntry so consumers that
            // query texts directly continue to see them — the godoc
            // attachment above only adds the parent linkage, it doesn't
            // replace the regular comment stream.
            extract_go_comment(child, source, file_path, parent_ctx, texts);
            continue;
        }
        // Anything not a comment: maybe attach pending docs, then reset.
        if !pending_docs.is_empty() {
            let item_line = node_line_range(child)[0];
            // Blank-line check: if the most recent comment isn't on the line
            // immediately above the declaration, godoc rules drop the doc.
            let attached = match last_comment_end_line {
                Some(end) if item_line == end + 1 => true,
                _ => false,
            };
            if attached {
                if let Some(item_name) = go_item_name(child, source) {
                    texts.push(TextEntry {
                        file: file_path.to_string(),
                        kind: "docstring".to_string(),
                        line: node_line_range(child),
                        text: pending_docs.join("\n"),
                        parent: Some(item_name),
                        project: String::new(),
                    });
                }
            }
            pending_docs.clear();
            last_comment_end_line = None;
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
            imports,
        );
    }
}

/// Match the qualified entity name a Go declaration would produce so the
/// `kind="docstring"` TextEntry's `parent` matches the entity's `name`
/// after the SymbolEntry → Entity translation in `index.rs`.
fn go_item_name(node: Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "function_declaration" => {
            find_child_by_field(node, "name").map(|n| node_text(n, source))
        }
        "method_declaration" => {
            let name = node_text(find_child_by_field(node, "name")?, source);
            // Mirror extract_method's receiver parsing.
            let receiver = find_child_by_field(node, "receiver").map(|recv| {
                node_text(recv, source)
                    .trim_matches(|c: char| c == '(' || c == ')' || c.is_whitespace())
                    .split_whitespace()
                    .last()
                    .unwrap_or("")
                    .trim_start_matches('*')
                    .to_string()
            });
            match receiver {
                Some(r) if !r.is_empty() => Some(format!("{r}.{name}")),
                _ => Some(name),
            }
        }
        "type_declaration" => {
            // type Foo ... or type ( ... ) — only the simple form gets a doc.
            let mut c = node.walk();
            for child in node.children(&mut c) {
                if child.kind() == "type_spec"
                    && let Some(n) = find_child_by_field(child, "name")
                {
                    return Some(node_text(n, source));
                }
            }
            None
        }
        "var_declaration" | "const_declaration" => {
            let mut c = node.walk();
            for child in node.children(&mut c) {
                if matches!(child.kind(), "var_spec" | "const_spec")
                    && let Some(n) = find_child_by_field(child, "name")
                {
                    return Some(node_text(n, source));
                }
            }
            None
        }
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_function(
    node: Node,
    source: &[u8],
    file_path: &str,
    symbols: &mut Vec<SymbolEntry>,
    texts: &mut Vec<TextEntry>,
    references: &mut Vec<ReferenceEntry>,
    depth: usize,
    imports: &ImportTable,
) {
    let name = match find_child_by_field(node, "name") {
        Some(n) => node_text(n, source),
        None => return,
    };

    let line = node_line_range(node);
    let visibility = go_visibility(&name);

    // Extract tokens from function body for FTS
    let tokens = find_child_by_field(node, "body")
        .and_then(|body| extract_tokens(body, source))
        .map(|t| filter_go_tokens(&t));

    push_symbol(
        symbols,
        file_path,
        name.clone(),
        "function",
        line,
        None,
        tokens,
        None,
        Some(visibility),
    );

    // Extract type references from parameters
    if let Some(params) = find_child_by_field(node, "parameters") {
        extract_type_refs_from_node(params, source, file_path, Some(&name), references);
    }

    // Extract type references from result
    if let Some(result) = find_child_by_field(node, "result") {
        extract_type_refs_from_node(result, source, file_path, Some(&name), references);
    }

    // Recurse into function body with function name as context
    if let Some(body) = find_child_by_field(node, "body") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            walk_node(
                child,
                source,
                file_path,
                Some(&name),
                symbols,
                texts,
                references,
                depth + 1,
                imports,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_method(
    node: Node,
    source: &[u8],
    file_path: &str,
    symbols: &mut Vec<SymbolEntry>,
    texts: &mut Vec<TextEntry>,
    references: &mut Vec<ReferenceEntry>,
    depth: usize,
    imports: &ImportTable,
) {
    let name = match find_child_by_field(node, "name") {
        Some(n) => node_text(n, source),
        None => return,
    };

    // Extract receiver type: `func (r *Receiver) Method()`
    let receiver = find_child_by_field(node, "receiver")
        .map(|recv| {
            // The receiver is a parameter_list with one entry
            // Try to extract the type name
            let text = node_text(recv, source);
            // Strip parens and pointer/reference
            text.trim_matches(|c: char| c == '(' || c == ')' || c.is_whitespace())
                .split_whitespace()
                .last()
                .unwrap_or("")
                .trim_start_matches('*')
                .to_string()
        })
        .unwrap_or_default();

    let line = node_line_range(node);
    let visibility = go_visibility(&name);

    // Extract tokens from method body for FTS
    let tokens = find_child_by_field(node, "body")
        .and_then(|body| extract_tokens(body, source))
        .map(|t| filter_go_tokens(&t));

    let full_name = if receiver.is_empty() {
        name
    } else {
        format!("{receiver}.{name}")
    };

    let parent = if receiver.is_empty() {
        None
    } else {
        Some(receiver.as_str())
    };

    push_symbol(
        symbols,
        file_path,
        full_name.clone(),
        "method",
        line,
        parent,
        tokens,
        None,
        Some(visibility),
    );

    // Extract type references from parameters
    if let Some(params) = find_child_by_field(node, "parameters") {
        extract_type_refs_from_node(params, source, file_path, Some(&full_name), references);
    }

    // Extract type references from result
    if let Some(result) = find_child_by_field(node, "result") {
        extract_type_refs_from_node(result, source, file_path, Some(&full_name), references);
    }

    // Extract type reference from receiver — `func (e *Engine) Foo()`
    // emits an `Engine` type_annotation. Without this, `sigil callers
    // Engine` misses every method declared on the type.
    if let Some(recv) = find_child_by_field(node, "receiver") {
        extract_type_refs_from_node(recv, source, file_path, Some(&full_name), references);
    }

    // Recurse into method body with method name as context
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
                imports,
            );
        }
    }
}

fn extract_type_decl(
    node: Node,
    source: &[u8],
    file_path: &str,
    symbols: &mut Vec<SymbolEntry>,
    texts: &mut Vec<TextEntry>,
    references: &mut Vec<ReferenceEntry>,
) {
    // `type (...)` block or `type Foo ...`
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "type_spec" {
            extract_type_spec(child, source, file_path, None, symbols, texts, references);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_type_spec(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
    texts: &mut Vec<TextEntry>,
    references: &mut Vec<ReferenceEntry>,
) {
    let name = match find_child_by_field(node, "name") {
        Some(n) => node_text(n, source),
        None => return,
    };

    let type_node = find_child_by_field(node, "type");
    let line = node_line_range(node);
    let visibility = go_visibility(&name);

    // Determine kind from the type definition
    let kind = type_node
        .map(|t| match t.kind() {
            "struct_type" => "struct",
            "interface_type" => "interface",
            _ => "type_alias",
        })
        .unwrap_or("type_alias");

    // Embed targets collected here, then attached to the just-pushed
    // SymbolEntry once we know its index. Go interfaces could also embed
    // interfaces, but interface-impl detection is deferred (interfaces in
    // Go are structural — would need a separate cross-file pass).
    let mut embeds: Vec<(String, String)> = Vec::new();

    // For structs, extract fields and their type references
    if let Some(type_n) = type_node {
        if type_n.kind() == "struct_type"
            && let Some(field_list) = find_child_by_field(type_n, "fields").or_else(|| {
                let mut c = type_n.walk();
                type_n
                    .children(&mut c)
                    .find(|n| n.kind() == "field_declaration_list")
            })
        {
            let mut cursor = field_list.walk();
            for child in field_list.children(&mut cursor) {
                if child.kind() == "field_declaration" {
                    if let Some(field_name_node) = find_child_by_field(child, "name") {
                        let field_name = node_text(field_name_node, source);
                        let field_line = node_line_range(child);
                        let field_vis = go_visibility(&field_name);
                        push_symbol(
                            symbols,
                            file_path,
                            format!("{name}.{field_name}"),
                            "property",
                            field_line,
                            Some(&name),
                            None,
                            None,
                            Some(field_vis),
                        );
                    } else if let Some(embed_target) =
                        detect_embedded_field(child, source)
                    {
                        // Anonymous field == struct embedding. Emit a
                        // heritage edge against the embedded type name.
                        embeds.push(("embed".to_string(), embed_target));
                    }
                    // Extract type references from field type
                    if let Some(field_type) = find_child_by_field(child, "type") {
                        extract_type_refs_from_node(
                            field_type,
                            source,
                            file_path,
                            Some(&name),
                            references,
                        );
                    }
                }
                // Extract comments inside struct
                if child.kind() == "comment" {
                    extract_go_comment(child, source, file_path, Some(&name), texts);
                }
            }
        }
        // For interfaces, extract method signatures
        if type_n.kind() == "interface_type" {
            let mut cursor = type_n.walk();
            for child in type_n.children(&mut cursor) {
                if child.kind() == "method_spec"
                    && let Some(method_name_node) = find_child_by_field(child, "name")
                {
                    let method_name = node_text(method_name_node, source);
                    let method_line = node_line_range(child);
                    let method_vis = go_visibility(&method_name);
                    let method_sig = collapse_whitespace(node_text(child, source).trim());
                    push_symbol(
                        symbols,
                        file_path,
                        format!("{name}.{method_name}"),
                        "method",
                        method_line,
                        Some(&name),
                        Some(method_sig),
                        None,
                        Some(method_vis),
                    );
                }
            }
        }
    }

    // Push the parent type symbol. We do this AFTER scanning fields so the
    // heritage vec is already populated. The struct/interface symbol must
    // also live before its child property/method symbols in the sorted
    // output, so the placement here is fine — `index.rs` re-sorts by
    // (file, line_start) before serializing.
    symbols.push(SymbolEntry {
        file: file_path.to_string(),
        name: name.clone(),
        kind: kind.to_string(),
        line,
        parent: parent_ctx.map(String::from),
        tokens: None,
        alias: None,
        visibility: Some(visibility),
        sig: None,
        project: String::new(),
        heritage: embeds,
    });
}

/// Recognise a struct embed: a `field_declaration` whose body is just a
/// bare type name (no field name).
///
/// Returns the embedded type's name. Handles four cases:
/// * `Bar`           → `Bar` (type_identifier)
/// * `*Bar`          → `Bar` (pointer_type wrapping type_identifier)
/// * `pkg.Bar`       → `pkg.Bar` (qualified_type)
/// * `*pkg.Bar`      → `pkg.Bar` (pointer_type wrapping qualified_type)
///
/// Returns `None` for nominal fields (those with names) and for malformed
/// nodes the grammar might surface.
fn detect_embedded_field(field: Node, source: &[u8]) -> Option<String> {
    // Tree-sitter-go represents embeds as `field_declaration` nodes with a
    // `type` field and no `name` field. We've already established no name
    // above, but double-check here for safety against grammar quirks.
    if find_child_by_field(field, "name").is_some() {
        return None;
    }
    let type_node = find_child_by_field(field, "type")?;
    fn extract_target(t: Node, source: &[u8]) -> Option<String> {
        match t.kind() {
            "type_identifier" => Some(node_text(t, source)),
            "qualified_type" => Some(node_text(t, source)),
            "pointer_type" => {
                // pointer_type wraps `_type` field — unwrap and recurse.
                let inner = find_child_by_field(t, "type").or_else(|| {
                    let mut c = t.walk();
                    t.children(&mut c)
                        .find(|n| matches!(n.kind(), "type_identifier" | "qualified_type"))
                })?;
                extract_target(inner, source)
            }
            _ => None,
        }
    }
    extract_target(type_node, source)
}

fn extract_var_const(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
) {
    let is_const = node.kind() == "const_declaration";
    let kind = if is_const { "constant" } else { "variable" };

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "var_spec" || child.kind() == "const_spec" {
            // The "value" field on a (var|const)_spec is an expression_list:
            // `var a, b = 1, 2`. We capture only the first expression for now;
            // multi-name specs are rare in const blocks where this matters most
            // (RETRY_TIMEOUT, ANTHROPIC_BETA_HEADER style).
            let value_text = find_child_by_field(child, "value")
                .and_then(|v| {
                    let mut c = v.walk();
                    v.children(&mut c)
                        .find(|n| !matches!(n.kind(), "," | "(" | ")"))
                        .or(Some(v))
                })
                .map(|n| truncate_sig(&node_text(n, source)));

            if let Some(name_node) = find_child_by_field(child, "name") {
                let name = node_text(name_node, source);
                let line = node_line_range(child);
                let visibility = go_visibility(&name);

                symbols.push(SymbolEntry {
                    file: file_path.to_string(),
                    name,
                    kind: kind.to_string(),
                    line,
                    parent: parent_ctx.map(String::from),
                    tokens: None,
                    alias: None,
                    visibility: Some(visibility),
                    sig: value_text.clone(),
                    project: String::new(),
                    heritage: Vec::new(),
                });
            }
            // Handle multiple names in one spec: `var a, b, c int`
            let mut spec_cursor = child.walk();
            for spec_child in child.children(&mut spec_cursor) {
                if spec_child.kind() == "identifier" {
                    // First identifier is captured by field "name", subsequent ones need manual check
                    if find_child_by_field(child, "name")
                        .map(|n| n.id() == spec_child.id())
                        .unwrap_or(false)
                    {
                        continue; // already captured
                    }
                    let extra_name = node_text(spec_child, source);
                    let extra_line = node_line_range(spec_child);
                    let extra_vis = go_visibility(&extra_name);
                    symbols.push(SymbolEntry {
                        file: file_path.to_string(),
                        name: extra_name,
                        kind: kind.to_string(),
                        line: extra_line,
                        parent: parent_ctx.map(String::from),
                        tokens: None,
                        alias: None,
                        visibility: Some(extra_vis),
                        sig: value_text.clone(),
                        project: String::new(),
                        heritage: Vec::new(),
                    });
                }
            }
        }
    }
}

fn extract_imports(
    node: Node,
    source: &[u8],
    file_path: &str,
    symbols: &mut Vec<SymbolEntry>,
    references: &mut Vec<ReferenceEntry>,
) {
    let line = node_line_range(node);

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "import_spec" {
            let path_node = find_child_by_field(child, "path");
            let name_node = find_child_by_field(child, "name");

            if let Some(p) = path_node {
                let path = strip_string_quotes(&node_text(p, source));
                let alias = name_node.map(|n| node_text(n, source));

                push_symbol(
                    symbols,
                    file_path,
                    path.clone(),
                    "import",
                    line,
                    None,
                    None,
                    alias,
                    Some("private".to_string()),
                );
                // Also record as import reference
                references.push(ReferenceEntry {
                    file: file_path.to_string(),
                    name: path,
                    kind: "import".to_string(),
                    line,
                    caller: None,
                    project: String::new(),
                    confidence: None,
                });
            }
        }
        // Also handle single import: `import "fmt"`
        if child.kind() == "import_spec_list" {
            let mut list_cursor = child.walk();
            for spec in child.children(&mut list_cursor) {
                if spec.kind() == "import_spec" {
                    let path_node = find_child_by_field(spec, "path");
                    let name_node = find_child_by_field(spec, "name");

                    if let Some(p) = path_node {
                        let path = strip_string_quotes(&node_text(p, source));
                        let alias = name_node.map(|n| node_text(n, source));
                        let spec_line = node_line_range(spec);

                        push_symbol(
                            symbols,
                            file_path,
                            path.clone(),
                            "import",
                            spec_line,
                            None,
                            None,
                            alias,
                            Some("private".to_string()),
                        );
                        // Also record as import reference
                        references.push(ReferenceEntry {
                            file: file_path.to_string(),
                            name: path,
                            kind: "import".to_string(),
                            line: spec_line,
                            caller: None,
                            project: String::new(),
                            confidence: None,
                        });
                    }
                }
            }
        }
    }
}

fn extract_package(node: Node, source: &[u8], file_path: &str, symbols: &mut Vec<SymbolEntry>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "package_identifier" {
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

/// Extract a function call as a reference.
///
/// 3-tier resolver:
/// * Selector calls (`pkg.Func`) whose leading segment matches a file-local
///   import alias emit two refs:
///     1. The raw selector form `pkg.Func` with `confidence=0.8`, for
///        backwards-compatible textual `callers`/`callees` queries.
///     2. The resolved qualified form `<import-path>/Func` with
///        `confidence=0.8`, for cross-file/cross-repo resolution.
///   Both edges share line + caller so consumers can dedupe by line.
/// * Plain identifier calls (`SomeFunc`) emit a single edge with
///   `confidence=0.95` — same-file resolution is implicit at this layer
///   (the caller and callee both live in the file's symbol table).
/// * Selector calls that don't resolve through the import table (method
///   calls on a local value, calls into nested receivers) keep the old
///   behaviour: emit the selector verbatim with `confidence=None`.
fn extract_call(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    references: &mut Vec<ReferenceEntry>,
    imports: &ImportTable,
) {
    let line = node_line_range(node);

    // The "function" field contains the callable expression
    let Some(func) = find_child_by_field(node, "function") else {
        return;
    };

    // Extract the name of the called function
    let (name, is_selector) = match func.kind() {
        "identifier" => (node_text(func, source), false),
        "selector_expression" => (node_text(func, source), true),
        // `(*Engine)(nil)` or `(Engine)(x)` — parenthesized type used as
        // a cast/conversion. Tree-sitter parses these ambiguously
        // because at parse-time `Engine` is just an identifier (no
        // type/value distinction): `*Engine` shows up as
        // `unary_expression` over `identifier`, not `pointer_type` over
        // `type_identifier`. So we walk inside the parens looking for
        // any identifier and emit it as a type_annotation ref — the
        // surrounding call context disambiguates it as a type cast.
        // We skip builtin/primitive Go type names to avoid noise.
        "parenthesized_expression" => {
            emit_cast_type_refs(func, source, file_path, parent_ctx, references);
            return;
        }
        _ => return,
    };

    // Skip builtins
    if is_go_builtin_call(&name) {
        return;
    }

    if is_selector {
        // Selector form: `<head>.<rest>` — head is the leftmost segment.
        if let Some((head, rest)) = name.split_once('.') {
            if let Some(import_path) = imports.get(head) {
                // Tier 2 — resolved through file-local import alias.
                // Emit the bare textual form for legacy text-match consumers
                // first…
                references.push(ReferenceEntry {
                    file: file_path.to_string(),
                    name: name.clone(),
                    kind: "call".to_string(),
                    line,
                    caller: parent_ctx.map(String::from),
                    project: String::new(),
                    confidence: Some(0.8),
                });
                // …then the qualified form (`encoding/json/Marshal`).
                // We use `/` as the join character to stay consistent with
                // Go's package-path convention; consumers that want
                // `pkg.Marshal` can split on `/` and take the tail.
                references.push(ReferenceEntry {
                    file: file_path.to_string(),
                    name: format!("{import_path}/{rest}"),
                    kind: "call".to_string(),
                    line,
                    caller: parent_ctx.map(String::from),
                    project: String::new(),
                    confidence: Some(0.8),
                });
                return;
            }
        }
        // Unresolved selector — keep legacy behaviour, no confidence tag.
        references.push(ReferenceEntry {
            file: file_path.to_string(),
            name,
            kind: "call".to_string(),
            line,
            caller: parent_ctx.map(String::from),
            project: String::new(),
            confidence: None,
        });
        return;
    }

    // Bare identifier — tier 1, exact same-file resolution.
    references.push(ReferenceEntry {
        file: file_path.to_string(),
        name,
        kind: "call".to_string(),
        line,
        caller: parent_ctx.map(String::from),
        project: String::new(),
        confidence: Some(0.95),
    });
}

/// Check if a call is to a Go builtin that we want to skip.
fn is_go_builtin_call(name: &str) -> bool {
    let base = name.split('.').next_back().unwrap_or(name);
    matches!(
        base,
        "make"
            | "len"
            | "cap"
            | "append"
            | "copy"
            | "delete"
            | "close"
            | "panic"
            | "recover"
            | "print"
            | "println"
            | "new"
            | "complex"
            | "real"
            | "imag"
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
                if !is_go_primitive_type(&name) {
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
            "qualified_type" => {
                let name = node_text(n, source);
                references.push(ReferenceEntry {
                    file: file_path.to_string(),
                    name,
                    kind: "type_annotation".to_string(),
                    line: node_line_range(n),
                    caller: parent_ctx.map(String::from),
                    project: String::new(),
                    confidence: None,
                });
                continue; // Don't recurse into children
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

/// Emit type_annotation refs for the inner identifier of a parenthesized
/// expression in cast position (`(*T)(x)` / `(T)(x)`). Tree-sitter-go
/// parses these ambiguously — `*T` becomes `unary_expression` over
/// `identifier`, not `pointer_type` over `type_identifier`. Walk for
/// any plain identifier and emit it (filtering primitives + builtin
/// function names like `len`/`make` to avoid noise).
fn emit_cast_type_refs(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    references: &mut Vec<ReferenceEntry>,
) {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "identifier" | "type_identifier" => {
                let name = node_text(n, source);
                if !is_go_primitive_type(&name) && !is_go_builtin_call(&name) {
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
            "qualified_type" | "selector_expression" => {
                // `(*pkg.Engine)(nil)` — emit the qualified form.
                references.push(ReferenceEntry {
                    file: file_path.to_string(),
                    name: node_text(n, source),
                    kind: "type_annotation".to_string(),
                    line: node_line_range(n),
                    caller: parent_ctx.map(String::from),
                    project: String::new(),
                    confidence: None,
                });
                continue;
            }
            _ => {}
        }
        let mut cursor = n.walk();
        for child in n.children(&mut cursor) {
            stack.push(child);
        }
    }
}

/// Check if a type is a Go primitive.
fn is_go_primitive_type(name: &str) -> bool {
    matches!(
        name,
        "int"
            | "int8"
            | "int16"
            | "int32"
            | "int64"
            | "uint"
            | "uint8"
            | "uint16"
            | "uint32"
            | "uint64"
            | "uintptr"
            | "float32"
            | "float64"
            | "complex64"
            | "complex128"
            | "bool"
            | "byte"
            | "rune"
            | "string"
            | "error"
    )
}

fn extract_go_comment(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    texts: &mut Vec<TextEntry>,
) {
    extract_comment(node, source, file_path, parent_ctx, texts);
}

fn go_visibility(name: &str) -> String {
    if name.starts_with(|c: char| c.is_uppercase()) {
        "public".to_string()
    } else {
        "private".to_string()
    }
}

/// Go-specific stopwords to filter from tokens.
const GO_STOPWORDS: &[&str] = &[
    // Keywords and builtins
    "nil",
    "iota",
    "func",
    "var",
    "type",
    "interface",
    "map",
    "chan",
    "range",
    "defer",
    "go",
    "select",
    "goto",
    "package",
    "import",
    // Common short names
    "err",
    "ctx",
    "ok",
    "n",
    "i",
    "j",
    "k",
    // Builtins
    "make",
    "len",
    "cap",
    "append",
    "copy",
    "delete",
    "close",
    "panic",
    "recover",
    "print",
    "println",
    // Test framework
    "require",
];

/// Filter Go-specific tokens from the extracted token string.
fn filter_go_tokens(tokens: &str) -> String {
    tokens
        .split_whitespace()
        .filter(|t| !GO_STOPWORDS.contains(t))
        .collect::<Vec<_>>()
        .join(" ")
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
    fn test_go_functions() {
        let source = b"package main

func Hello(name string) string {
    return \"Hello, \" + name
}

func privateHelper() {
    println(\"private\")
}";
        let (symbols, _texts, _refs) = parse_file(source, "go", "test.go").unwrap();

        let hello = find_sym(&symbols, "Hello");
        assert_eq!(hello.kind, "function");
        // Tokens contain identifiers from function body
        // Token may be None if all identifiers are filtered as stopwords
        assert_eq!(hello.visibility.as_deref(), Some("public"));

        let helper = find_sym(&symbols, "privateHelper");
        assert_eq!(helper.visibility.as_deref(), Some("private"));
    }

    #[test]
    fn test_go_methods() {
        let source = b"package main

type Person struct {
    Name string
}

func (p *Person) Greet() string {
    return \"Hello, \" + p.Name
}

func (p Person) privateMethod() {}";
        let (symbols, _texts, _refs) = parse_file(source, "go", "test.go").unwrap();

        let person = find_sym(&symbols, "Person");
        assert_eq!(person.kind, "struct");

        let greet = find_sym(&symbols, "Person.Greet");
        assert_eq!(greet.kind, "method");
        assert_eq!(greet.parent.as_deref(), Some("Person"));
        assert_eq!(greet.visibility.as_deref(), Some("public"));

        let priv_method = find_sym(&symbols, "Person.privateMethod");
        assert_eq!(priv_method.visibility.as_deref(), Some("private"));
    }

    #[test]
    fn test_go_structs() {
        let source = b"package main

type Point struct {
    X int
    Y int
    z int
}";
        let (symbols, _texts, _refs) = parse_file(source, "go", "test.go").unwrap();

        let point = find_sym(&symbols, "Point");
        assert_eq!(point.kind, "struct");

        let x = find_sym(&symbols, "Point.X");
        assert_eq!(x.kind, "property");
        assert_eq!(x.visibility.as_deref(), Some("public"));

        let z = find_sym(&symbols, "Point.z");
        assert_eq!(z.visibility.as_deref(), Some("private"));
    }

    #[test]
    fn test_go_interfaces() {
        let source = b"package main

type Reader interface {
    Read() (int, error)
    close()
}";
        let (symbols, _texts, _refs) = parse_file(source, "go", "test.go").unwrap();

        let reader = find_sym(&symbols, "Reader");
        assert_eq!(reader.kind, "interface");
        assert_eq!(reader.visibility.as_deref(), Some("public"));

        // Interface methods may or may not be extracted depending on implementation
        // Just verify the interface itself is extracted correctly
        assert!(symbols.len() >= 2); // at least package + interface
    }

    #[test]
    fn test_go_variables() {
        let source = b"package main

var GlobalVar = 100
var privateVar = 200

const MaxSize = 1000
const minSize = 10";
        let (symbols, _texts, _refs) = parse_file(source, "go", "test.go").unwrap();

        let global = find_sym(&symbols, "GlobalVar");
        assert_eq!(global.kind, "variable");
        assert_eq!(global.visibility.as_deref(), Some("public"));

        let max = find_sym(&symbols, "MaxSize");
        assert_eq!(max.kind, "constant");
        assert_eq!(max.visibility.as_deref(), Some("public"));

        let min = find_sym(&symbols, "minSize");
        assert_eq!(min.visibility.as_deref(), Some("private"));
    }

    #[test]
    fn test_go_imports() {
        let source = b"package main

import \"fmt\"
import (
    \"os\"
    io \"io/ioutil\"
)";
        let (symbols, _texts, _refs) = parse_file(source, "go", "test.go").unwrap();

        let fmt = symbols.iter().find(|s| s.name == "fmt").unwrap();
        assert_eq!(fmt.kind, "import");

        let os = symbols.iter().find(|s| s.name == "os").unwrap();
        assert_eq!(os.kind, "import");

        let io = symbols.iter().find(|s| s.name == "io/ioutil").unwrap();
        assert_eq!(io.alias.as_deref(), Some("io"));
    }

    #[test]
    fn test_go_type_alias() {
        let source = b"package main

type UserID int
type Handler func(string) error";
        let (symbols, _texts, _refs) = parse_file(source, "go", "test.go").unwrap();

        let user_id = find_sym(&symbols, "UserID");
        assert_eq!(user_id.kind, "type_alias");

        let handler = find_sym(&symbols, "Handler");
        assert_eq!(handler.kind, "type_alias");
    }

    #[test]
    fn test_go_package() {
        let source = b"package mypackage

func Foo() {}";
        let (symbols, _texts, _refs) = parse_file(source, "go", "test.go").unwrap();

        let pkg = symbols.iter().find(|s| s.kind == "module").unwrap();
        assert_eq!(pkg.name, "mypackage");
    }

    #[test]
    fn test_go_comments() {
        let source = b"package main

// Single line comment
func Helper() {}

/* Block comment */";
        let (_symbols, texts, _refs) = parse_file(source, "go", "test.go").unwrap();
        assert!(texts.iter().any(|t| t.kind == "comment"));
    }

    #[test]
    fn test_go_call_references() {
        let source = b"package main

func caller() {
    someFunction()
    pkg.NestedCall()
}

func someFunction() {}";
        let (_symbols, _texts, refs) = parse_file(source, "go", "test.go").unwrap();

        let call_refs: Vec<_> = refs.iter().filter(|r| r.kind == "call").collect();
        assert!(
            call_refs.iter().any(|r| r.name == "someFunction"),
            "should find someFunction call"
        );
        assert!(
            call_refs.iter().any(|r| r.name.contains("NestedCall")),
            "should find nested call"
        );
    }

    #[test]
    fn test_go_import_references() {
        let source = b"package main

import \"fmt\"
import (
    \"os\"
)";
        let (_symbols, _texts, refs) = parse_file(source, "go", "test.go").unwrap();

        let import_refs: Vec<_> = refs.iter().filter(|r| r.kind == "import").collect();
        assert!(
            import_refs.iter().any(|r| r.name == "fmt"),
            "should find fmt import"
        );
        assert!(
            import_refs.iter().any(|r| r.name == "os"),
            "should find os import"
        );
    }

    #[test]
    fn test_go_method_receiver_type_ref() {
        // Gap surfaced by the gin-gonic/gin audit: a method declaration
        // `func (engine *Engine) Foo()` records `Engine` as the
        // method's parent, but never emits a type_annotation Reference
        // for the receiver type. `sigil callers Engine` should reach
        // every method declared on it.
        let source = b"package main

type Engine struct { v int }

func (engine *Engine) First() {}
func (e Engine) Second() {}
func (other *Other) Skip() {}
";
        let (_symbols, _texts, refs) = parse_file(source, "go", "test.go").unwrap();
        let engine_recv: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == "type_annotation" && r.name == "Engine")
            .collect();
        assert_eq!(
            engine_recv.len(),
            2,
            "expected 2 Engine receiver refs (pointer + value); got {:?}",
            refs.iter().map(|r| (&r.kind, &r.name)).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn test_go_composite_literal_type_ref() {
        // `&Engine{...}` and `Engine{...}` struct literals should
        // surface `Engine` as a type_annotation ref so it shows up in
        // `sigil callers Engine`. Gin's `New()` returns `&Engine{...}`.
        let source = b"package main

type Engine struct { v int }

func New() *Engine {
    return &Engine{v: 1}
}

func makeValue() Engine {
    return Engine{v: 2}
}
";
        let (_symbols, _texts, refs) = parse_file(source, "go", "test.go").unwrap();
        let engine_lit: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == "type_annotation" && r.name == "Engine")
            .collect();
        // Two struct literals + two return types = at least 2 literal-driven refs.
        assert!(
            engine_lit.len() >= 4,
            "expected ≥4 Engine type-annotation refs (2 returns + 2 literals); got {} -> {:?}",
            engine_lit.len(),
            refs.iter().map(|r| (&r.kind, &r.name, r.line)).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn test_go_var_type_annotation_ref() {
        // Top-level `var x *Engine` / `var x Engine` should emit an
        // Engine type_annotation ref. Without this, declarations like
        // `var defaultLogger *Logger` aren't reachable via `callers
        // Logger`.
        let source = b"package main

type Engine struct { v int }

var globalEngine *Engine
var localEngine Engine
const _ = 1

func makeIt() {
    var inner *Engine
    _ = inner
}
";
        let (_symbols, _texts, refs) = parse_file(source, "go", "test.go").unwrap();
        let engine_var: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == "type_annotation" && r.name == "Engine")
            .collect();
        assert!(
            engine_var.len() >= 3,
            "expected ≥3 Engine type_annotation refs (2 top-level vars + 1 in-func var); got {} -> {:?}",
            engine_var.len(),
            refs.iter().map(|r| (&r.kind, &r.name, r.line)).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn test_go_parenthesized_type_cast_ref() {
        // `var _ IRouter = (*Engine)(nil)` is a type cast — the
        // call expression's "function" is a parenthesized pointer_type.
        // `Engine` should surface as a type_annotation ref so
        // `sigil callers Engine` reaches the cast site. Gin's gin.go:191
        // is the canonical example: `var _ IRouter = (*Engine)(nil)`.
        let source = b"package main

type Engine struct { v int }
type IRouter interface { GET() }

var _ IRouter = (*Engine)(nil)
";
        let (_symbols, _texts, refs) = parse_file(source, "go", "test.go").unwrap();
        let engine_cast: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == "type_annotation" && r.name == "Engine")
            .collect();
        assert!(
            !engine_cast.is_empty(),
            "expected ≥1 Engine type_annotation from the cast; got refs {:?}",
            refs.iter().map(|r| (&r.kind, &r.name)).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn test_go_type_references() {
        let source = b"package main

type MyStruct struct {
    field OtherType
}

func process(input CustomType) ResultType {
    return nil
}";
        let (_symbols, _texts, refs) = parse_file(source, "go", "test.go").unwrap();

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
}
