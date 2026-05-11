//! PHP symbol and text extraction.
//!
//! Targets the `tree-sitter-php` v0.24 grammar. Covers the core top-level
//! and class-body declarations: namespace, use, function, method, class,
//! interface, trait, enum, property, const, and call references.
//!
//! Members nested inside a class / trait / interface / enum body are
//! emitted with a qualified `Outer::member` name, matching the way PHP
//! source actually addresses them (e.g. `Person::greet`,
//! `Person::$name`). Top-level functions in a namespaced file are still
//! emitted as the bare name — PHP allows calling them as
//! `\Namespace\func`, but the unqualified form is the canonical handle.
//!
//! Traits and enums are emitted with `kind: "class"` to keep the kind
//! set compact and reuse the existing class-aware tooling downstream.
//! The grammatical distinction (trait vs class vs enum) is preserved at
//! the AST level — the `name` itself is enough for callers that care.

use tree_sitter::{Node, Tree};

use crate::parser::format::{ReferenceEntry, SymbolEntry, TextEntry};
use crate::parser::helpers::*;
use crate::parser::treesitter::MAX_DEPTH;

/// PHP keywords and noisy identifiers we don't want surfacing as tokens.
const PHP_STOPWORDS: &[&str] = &[
    "function",
    "class",
    "interface",
    "trait",
    "enum",
    "public",
    "protected",
    "private",
    "static",
    "abstract",
    "final",
    "readonly",
    "const",
    "var",
    "namespace",
    "use",
    "new",
    "return",
    "if",
    "else",
    "foreach",
    "for",
    "while",
    "do",
    "switch",
    "case",
    "default",
    "match",
    "throw",
    "try",
    "catch",
    "finally",
    "self",
    "parent",
    "this",
    "null",
    "true",
    "false",
];

fn filter_php_tokens(tokens: Option<String>) -> Option<String> {
    tokens.and_then(|t| {
        let filtered: Vec<&str> = t
            .split_whitespace()
            .filter(|tok| !PHP_STOPWORDS.contains(&tok.to_lowercase().as_str()))
            .collect();
        if filtered.is_empty() {
            None
        } else {
            Some(filtered.join(" "))
        }
    })
}

/// Common PHP builtins we filter from the call graph. These are language /
/// stdlib primitives that tend to dominate the noise floor without
/// carrying useful coupling information.
fn is_php_builtin(name: &str) -> bool {
    matches!(
        name,
        "print"
            | "echo"
            | "var_dump"
            | "print_r"
            | "count"
            | "array"
            | "isset"
            | "empty"
            | "is_null"
            | "is_array"
            | "is_string"
            | "is_int"
            | "is_numeric"
            | "strlen"
            | "strpos"
            | "str_replace"
            | "implode"
            | "explode"
            | "sprintf"
            | "printf"
            | "json_encode"
            | "json_decode"
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
        "namespace_definition" => {
            extract_namespace(node, source, file_path, symbols);
            // namespace { ... } form: walk the body so nested decls still
            // surface. The bracketless form (namespace Foo;) has no body
            // and following statements are already siblings at program
            // level — those get visited normally by the outer recursion.
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
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
        "namespace_use_declaration" => {
            extract_use(node, source, file_path, symbols, references);
            return;
        }
        "function_definition" => {
            extract_function(node, source, file_path, parent_ctx, symbols);
            // Walk body for call references.
            if let Some(body) = node.child_by_field_name("body") {
                let fn_name = field_name_text(node, source).unwrap_or_default();
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
        "method_declaration" => {
            extract_method(node, source, file_path, parent_ctx, symbols);
            if let Some(body) = node.child_by_field_name("body") {
                let mname = field_name_text(node, source).unwrap_or_default();
                let full = qualify_member(parent_ctx, &mname);
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
        "class_declaration" => {
            extract_class_like(
                node, source, file_path, parent_ctx, "class", symbols, texts, references, depth,
            );
            return;
        }
        "interface_declaration" => {
            extract_class_like(
                node, source, file_path, parent_ctx, "interface", symbols, texts, references, depth,
            );
            return;
        }
        "trait_declaration" => {
            // Traits surface as `class` for compactness; see module-level
            // note.
            extract_class_like(
                node, source, file_path, parent_ctx, "class", symbols, texts, references, depth,
            );
            return;
        }
        "enum_declaration" => {
            // PHP 8.1+ enums; their body is `enum_declaration_list`. Surface
            // as `class` — the cases inside are typically constants.
            extract_class_like(
                node, source, file_path, parent_ctx, "class", symbols, texts, references, depth,
            );
            return;
        }
        "property_declaration" => {
            extract_property(node, source, file_path, parent_ctx, symbols);
            return;
        }
        "const_declaration" => {
            extract_const(node, source, file_path, parent_ctx, symbols);
            return;
        }
        "comment" => {
            extract_php_comment(node, source, file_path, parent_ctx, texts);
            return;
        }
        "string" | "encapsed_string" => {
            extract_string(node, source, file_path, parent_ctx, texts);
            return;
        }
        "function_call_expression" => {
            extract_call_ref(node, source, file_path, parent_ctx, references);
            // Fall through to recurse into arguments — nested calls
            // matter for the call graph.
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

/// Read the `name` field of a node as a string.
fn field_name_text(node: Node, source: &[u8]) -> Option<String> {
    node.child_by_field_name("name").map(|n| node_text(n, source))
}

/// Qualify a top-level child with a namespace / file-scope parent using
/// the PHP `.` convention used elsewhere in sigil for non-class scopes.
fn qualify(parent_ctx: Option<&str>, name: &str) -> String {
    match parent_ctx {
        Some(p) if !p.is_empty() => format!("{p}.{name}"),
        _ => name.to_string(),
    }
}

/// Qualify a class-body member with `::` — matches the form PHP source
/// actually writes (e.g. `Person::greet`, `Person::$name`,
/// `Person::SPECIES`).
fn qualify_member(parent_ctx: Option<&str>, name: &str) -> String {
    match parent_ctx {
        Some(p) if !p.is_empty() => format!("{p}::{name}"),
        _ => name.to_string(),
    }
}

/// PHP `visibility_modifier` extraction. Defaults to `public` — top-level
/// functions don't carry a modifier, and class members without one are
/// public by language convention.
fn extract_php_visibility(node: Node, source: &[u8]) -> String {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            let t = node_text(child, source);
            return match t.as_str() {
                "private" => "private".to_string(),
                "protected" => "internal".to_string(),
                _ => "public".to_string(),
            };
        }
    }
    "public".to_string()
}

// ---------------------------------------------------------------------------
// Declarations
// ---------------------------------------------------------------------------

fn extract_namespace(node: Node, source: &[u8], file_path: &str, symbols: &mut Vec<SymbolEntry>) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = node_text(name_node, source);
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

fn extract_use(
    node: Node,
    source: &[u8],
    file_path: &str,
    symbols: &mut Vec<SymbolEntry>,
    references: &mut Vec<ReferenceEntry>,
) {
    let line = node_line_range(node);

    // Two shapes:
    //   use Foo\Bar;                  → direct namespace_name child
    //   use Foo\Bar as B;             → namespace_use_clause child
    //   use Foo\Bar, Foo\Baz as Z;    → multiple namespace_use_clause children
    let mut cursor = node.walk();
    let mut handled_clause = false;
    let mut direct_name: Option<String> = None;

    for child in node.children(&mut cursor) {
        match child.kind() {
            "namespace_use_clause" => {
                handled_clause = true;
                let mut name: Option<String> = None;
                let mut alias: Option<String> = None;
                let mut c2 = child.walk();
                for sub in child.children(&mut c2) {
                    match sub.kind() {
                        "name" | "qualified_name" | "namespace_name" => {
                            if name.is_none() {
                                name = Some(node_text(sub, source));
                            }
                        }
                        _ => {}
                    }
                }
                // Alias is exposed via a field.
                if let Some(alias_node) = child.child_by_field_name("alias") {
                    alias = Some(node_text(alias_node, source));
                }
                if let Some(n) = name {
                    push_use_symbol(symbols, references, file_path, n, alias, line);
                }
            }
            "namespace_name" | "qualified_name" | "name" => {
                if direct_name.is_none() {
                    direct_name = Some(node_text(child, source));
                }
            }
            _ => {}
        }
    }

    if !handled_clause {
        if let Some(n) = direct_name {
            push_use_symbol(symbols, references, file_path, n, None, line);
        }
    }
}

fn push_use_symbol(
    symbols: &mut Vec<SymbolEntry>,
    references: &mut Vec<ReferenceEntry>,
    file_path: &str,
    name: String,
    alias: Option<String>,
    line: [u32; 2],
) {
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
    let Some(name) = field_name_text(node, source) else {
        return;
    };
    let line = node_line_range(node);
    let full_name = qualify(parent_ctx, &name);

    // Class-level visibility doesn't really exist in PHP; classes are
    // effectively public. Use that as the default for consistency with
    // other languages.
    let visibility = "public".to_string();

    let tokens = node
        .child_by_field_name("body")
        .and_then(|body| filter_php_tokens(extract_tokens(body, source)));

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

fn extract_function(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
) {
    let Some(name) = field_name_text(node, source) else {
        return;
    };
    let line = node_line_range(node);
    // Top-level functions don't take a visibility modifier in PHP.
    let visibility = "public".to_string();

    // function_definition is always top-level / namespace-scoped. Methods
    // are handled via method_declaration. So we tag as "function" here.
    let kind = "function";

    let full_name = qualify(parent_ctx, &name);
    let tokens = node
        .child_by_field_name("body")
        .and_then(|body| filter_php_tokens(extract_tokens(body, source)));

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

fn extract_method(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
) {
    let Some(name) = field_name_text(node, source) else {
        return;
    };
    let line = node_line_range(node);
    let visibility = extract_php_visibility(node, source);
    let full_name = qualify_member(parent_ctx, &name);

    let tokens = node
        .child_by_field_name("body")
        .and_then(|body| filter_php_tokens(extract_tokens(body, source)));

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

fn extract_property(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
) {
    let line = node_line_range(node);
    let visibility = extract_php_visibility(node, source);

    // One property_declaration can declare several properties:
    //   public int $a = 0, $b = 1;
    // → multiple property_element children.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "property_element" {
            continue;
        }
        let Some(var_name_node) = child.child_by_field_name("name") else {
            continue;
        };
        // variable_name → has a single `name` child; node text is `$foo`.
        // Drop the leading `$` for the symbol name so callers can match
        // `Class::name` consistently. The qualified form keeps the `$`
        // visible (`Class::$name`) — that's what PHP source writes.
        let raw = node_text(var_name_node, source);
        let bare = raw.strip_prefix('$').unwrap_or(&raw).to_string();
        if bare.is_empty() {
            continue;
        }
        let qualified = qualify_member(parent_ctx, &format!("${bare}"));

        let sig = child
            .child_by_field_name("default_value")
            .map(|n| truncate_sig(&node_text(n, source)));

        symbols.push(SymbolEntry {
            file: file_path.to_string(),
            name: qualified,
            kind: "property".to_string(),
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

fn extract_const(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
) {
    let line = node_line_range(node);
    let visibility = if parent_ctx.is_some() {
        extract_php_visibility(node, source)
    } else {
        // Top-level `const FOO = ...;` — public by convention.
        "public".to_string()
    };

    // const_declaration → one or more const_element children, each with
    // a `name` (a bare `name` node) and an `expression`.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "const_element" {
            continue;
        }
        // Layout: name '=' expression. The `name` is the first named
        // child; the expression follows.
        let mut name: Option<String> = None;
        let mut rhs: Option<String> = None;
        let mut c2 = child.walk();
        for sub in child.children(&mut c2) {
            if !sub.is_named() {
                continue;
            }
            if sub.kind() == "name" && name.is_none() {
                name = Some(node_text(sub, source));
            } else if name.is_some() && rhs.is_none() {
                rhs = Some(node_text(sub, source));
            }
        }
        let Some(name) = name else { continue };
        let qualified = if parent_ctx.is_some() {
            qualify_member(parent_ctx, &name)
        } else {
            name
        };
        let sig = rhs.map(|r| truncate_sig(&r));

        symbols.push(SymbolEntry {
            file: file_path.to_string(),
            name: qualified,
            kind: "constant".to_string(),
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
    let Some(callee) = node.child_by_field_name("function") else {
        return;
    };
    // We only generate a Reference for plain-named call targets. Method
    // calls (member_call_expression) and dynamic callees aren't
    // statically resolvable here and add noise to the call graph.
    let (name, confidence) = match callee.kind() {
        "name" => (node_text(callee, source), Some(1.0_f64)),
        "qualified_name" => (node_text(callee, source), None),
        _ => return,
    };
    if name.is_empty() {
        return;
    }
    // Drop pure builtins (echo, count, etc.).
    let leaf = name.rsplit('\\').next().unwrap_or(&name);
    if is_php_builtin(leaf) {
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

fn extract_php_comment(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_ctx: Option<&str>,
    texts: &mut Vec<TextEntry>,
) {
    // PHP supports `//`, `#`, and `/* */` (with `/** */` for PHPDoc).
    // The generic helper covers all three.
    extract_comment(node, source, file_path, parent_ctx, texts);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::treesitter::parse_file;

    fn find_sym<'a>(symbols: &'a [SymbolEntry], name: &str) -> &'a SymbolEntry {
        symbols.iter().find(|s| s.name == name).unwrap_or_else(|| {
            panic!(
                "symbol not found: {name}\nhave: {:?}",
                symbols.iter().map(|s| (&s.name, &s.kind)).collect::<Vec<_>>()
            )
        })
    }

    #[test]
    fn php_bare_call_gets_tier1_confidence() {
        // PHP unqualified `name` calls get tier-1 confidence (1.0).
        // Qualified namespace calls (`App\Foo\bar()`) and method/scoped
        // calls stay at None until alias resolution lands.
        let source = b"<?php\nfunction caller() { helper(); }\nfunction helper() {}\n";
        let (_, _, refs) = parse_file(source, "php", "t.php").unwrap();
        let bare = refs
            .iter()
            .find(|r| r.kind == "call" && r.name == "helper")
            .expect("helper() bare call");
        assert_eq!(bare.confidence, Some(1.0));
    }

    #[test]
    fn php_top_level_function() {
        let source = b"<?php\nfunction greet(string $name): string {\n    return \"hi $name\";\n}\n";
        let (symbols, _, _) = parse_file(source, "php", "t.php").unwrap();
        let g = find_sym(&symbols, "greet");
        assert_eq!(g.kind, "function");
        assert_eq!(g.visibility.as_deref(), Some("public"));
    }

    #[test]
    fn php_class_with_method() {
        let source = b"<?php\nclass Person {\n    public string $name;\n    public function greet(): string { return \"hi\"; }\n    private function helper(): void {}\n}\n";
        let (symbols, _, _) = parse_file(source, "php", "t.php").unwrap();
        let p = find_sym(&symbols, "Person");
        assert_eq!(p.kind, "class");
        let g = find_sym(&symbols, "Person::greet");
        assert_eq!(g.kind, "method");
        assert_eq!(g.visibility.as_deref(), Some("public"));
        let h = find_sym(&symbols, "Person::helper");
        assert_eq!(h.visibility.as_deref(), Some("private"));
        let prop = find_sym(&symbols, "Person::$name");
        assert_eq!(prop.kind, "property");
    }

    #[test]
    fn php_protected_maps_to_internal() {
        // Sigil's schema has three visibility buckets — public / internal /
        // private. PHP `protected` aligns with `internal` to match the
        // mapping java / csharp / kotlin already establish.
        let source = b"<?php\nclass Person {\n    protected function helper(): void {}\n}\n";
        let (symbols, _, _) = parse_file(source, "php", "t.php").unwrap();
        let h = find_sym(&symbols, "Person::helper");
        assert_eq!(
            h.visibility.as_deref(),
            Some("internal"),
            "PHP `protected` must map to the `internal` bucket, got {:?}",
            h.visibility,
        );
    }

    #[test]
    fn php_interface() {
        let source = b"<?php\ninterface Greeter {\n    public function greet(): string;\n}\n";
        let (symbols, _, _) = parse_file(source, "php", "t.php").unwrap();
        let i = find_sym(&symbols, "Greeter");
        assert_eq!(i.kind, "interface");
    }

    #[test]
    fn php_trait() {
        let source = b"<?php\ntrait Helpful {\n    public function help(): void {}\n}\n";
        let (symbols, _, _) = parse_file(source, "php", "t.php").unwrap();
        let t = find_sym(&symbols, "Helpful");
        // Traits surface as class — documented choice.
        assert_eq!(t.kind, "class");
        let h = find_sym(&symbols, "Helpful::help");
        assert_eq!(h.kind, "method");
    }

    #[test]
    fn php_enum() {
        let source = b"<?php\nenum Status {\n    case Active;\n    case Inactive;\n}\n";
        let (symbols, _, _) = parse_file(source, "php", "t.php").unwrap();
        let e = find_sym(&symbols, "Status");
        assert_eq!(e.kind, "class");
    }

    #[test]
    fn php_namespace_and_use() {
        let source = b"<?php\nnamespace App\\Service;\nuse App\\Util\\Logger;\nuse App\\Util\\Cache as C;\n";
        let (symbols, _, refs) = parse_file(source, "php", "t.php").unwrap();
        let ns = symbols.iter().find(|s| s.kind == "module").unwrap();
        assert_eq!(ns.name, "App\\Service");
        let imports: Vec<_> = symbols.iter().filter(|s| s.kind == "import").collect();
        assert_eq!(imports.len(), 2);
        assert!(imports.iter().any(|i| i.name == "App\\Util\\Logger"));
        assert!(imports.iter().any(|i| i.alias.as_deref() == Some("C")));
        let import_refs: Vec<_> = refs.iter().filter(|r| r.kind == "import").collect();
        assert_eq!(import_refs.len(), 2);
    }

    #[test]
    fn php_top_level_const() {
        let source = b"<?php\nconst MAX_RETRIES = 3;\n";
        let (symbols, _, _) = parse_file(source, "php", "t.php").unwrap();
        let c = find_sym(&symbols, "MAX_RETRIES");
        assert_eq!(c.kind, "constant");
        assert!(c.sig.as_deref().unwrap_or("").contains("3"));
    }

    #[test]
    fn php_class_const() {
        let source = b"<?php\nclass Person {\n    const SPECIES = \"human\";\n}\n";
        let (symbols, _, _) = parse_file(source, "php", "t.php").unwrap();
        let c = find_sym(&symbols, "Person::SPECIES");
        assert_eq!(c.kind, "constant");
    }

    #[test]
    fn php_call_ref_filters_builtins() {
        let source = b"<?php\nfunction work(): void {\n    helper();\n    echo(\"x\");\n    count([1,2,3]);\n}\n";
        let (_, _, refs) = parse_file(source, "php", "t.php").unwrap();
        let calls: Vec<&str> = refs
            .iter()
            .filter(|r| r.kind == "call")
            .map(|r| r.name.as_str())
            .collect();
        assert!(calls.iter().any(|n| *n == "helper"));
        assert!(!calls.iter().any(|n| *n == "echo"));
        assert!(!calls.iter().any(|n| *n == "count"));
    }
}
