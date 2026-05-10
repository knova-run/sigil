use std::path::{Path, PathBuf};

use crate::cache::{self, Cache};
use crate::entity::{Entity, Reference};
use crate::hasher;
use crate::meta;
use crate::signature;

pub struct IndexResult {
    pub entities: Vec<Entity>,
    pub refs: Vec<Reference>,
}

/// Parse a single file's source code and return entities and references.
/// Used by the diff engine to parse file content fetched from git refs.
pub fn parse_single_file(
    source: &str,
    file_path: &str,
    language: &str,
) -> Result<(Vec<Entity>, Vec<Reference>), String> {
    if language == "json" {
        return crate::json_index::parse_json_file(source, file_path);
    }
    if language == "yaml" {
        return crate::yaml_index::parse_yaml_file(source, file_path);
    }
    if language == "toml" {
        return crate::toml_index::parse_toml_file(source, file_path);
    }
    if language == "markdown" {
        return crate::markdown_index::parse_markdown_file(source, file_path);
    }

    let (symbols, texts, references) = crate::parser::treesitter::parse_file(
        source.as_bytes(), language, file_path
    ).map_err(|e| format!("parse error: {}", e))?;

    // Index docstrings by parent symbol name so we can attach them in O(1)
    // during the entity build below. Python parsers emit docstrings as
    // TextEntry rows with `parent` set to the qualified function/class name.
    // For Rust/Go-style leading doc-comments, parsers will also surface them
    // through the same TextEntry pipeline (kind="docstring") in later phases;
    // the lookup here is intentionally generic.
    let docs_by_parent: std::collections::HashMap<&str, &str> = texts
        .iter()
        .filter(|t| t.kind == "docstring")
        .filter_map(|t| t.parent.as_deref().map(|p| (p, t.text.as_str())))
        .collect();

    let mut entities = Vec::new();
    let mut refs = Vec::new();

    for sym in &symbols {
        let line_start = sym.line[0] as usize;
        let line_end = sym.line[1] as usize;

        let raw_text = hasher::extract_raw_bytes(source, line_start, line_end);
        let (extracted_sig, body_start) = signature::extract_signature(
            source, line_start, line_end, language
        );
        // Prefer a parser-provided sig (e.g. constant/variable RHS captured
        // directly from the AST) over the line-range textual extractor.
        let sig = sym.sig.clone().or(extracted_sig);
        let meta_start = find_decorator_start(source, line_start, language);
        let markers = meta::extract_markers(source, meta_start, line_end, language);

        let (sig, sig_hash) = if is_import_kind(&sym.kind) {
            (None, None)
        } else {
            let sh = hasher::sig_hash(sig.as_deref());
            (sig, sh)
        };

        let doc = docs_by_parent
            .get(sym.name.as_str())
            .and_then(|raw| crate::entity::truncate_doc(raw));

        let parent = sym.parent.clone();
        let qualified_name = crate::entity::compose_qualified_name(parent.as_deref(), &sym.name);
        entities.push(Entity {
            file: file_path.to_string(),
            name: sym.name.clone(),
            kind: normalize_kind(&sym.kind),
            line_start: sym.line[0],
            line_end: sym.line[1],
            parent,
            qualified_name,
            sig,
            meta: markers,
            body_hash: hasher::body_hash(source, body_start, line_end),
            sig_hash,
            struct_hash: hasher::struct_hash(raw_text.as_bytes()),
            // Phase 1: capture visibility from the parser (public/private/pub/
            // pub(crate)/etc.) so rank multipliers can favor exported symbols.
            // `rank` and `blast_radius` get populated later by src/rank.rs.
            visibility: sym.visibility.clone(),
            rank: None,
            blast_radius: None,
            doc,
        });
    }

    for refentry in &references {
        refs.push(Reference {
            file: file_path.to_string(),
            caller: refentry.caller.clone(),
            name: refentry.name.clone(),
            ref_kind: refentry.kind.clone(),
            line: refentry.line[0],
        });
    }

    Ok((entities, refs))
}

pub fn build_index(
    root: &Path,
    files: Option<&[PathBuf]>,
    full: bool,
    include_refs: bool,
    verbose: bool,
) -> IndexResult {
    let files_to_index = match files {
        Some(f) => f.to_vec(),
        None => discover_source_files(root),
    };

    let sigil_dir = root.join(".sigil");
    let prev_cache = if full { None } else { Cache::load(&sigil_dir) };
    let prev_entities = if full { Vec::new() } else { load_previous_entities(&sigil_dir) };
    let prev_refs = if full || !include_refs { Vec::new() } else { load_previous_refs(&sigil_dir) };

    let mut all_entities: Vec<Entity> = Vec::new();
    let mut all_refs: Vec<Reference> = Vec::new();
    let mut new_cache = Cache::new();
    let mut parsed_count = 0usize;
    let mut cached_count = 0usize;

    for filepath in &files_to_index {
        let relative_path = filepath.strip_prefix(root).unwrap_or(filepath);
        let relative_str = relative_path.to_string_lossy().replace('\\', "/");

        let source_bytes = match std::fs::read(filepath) {
            Ok(b) => b,
            Err(e) => {
                if verbose {
                    eprintln!("skip (read error): {}: {}", relative_str, e);
                }
                continue;
            }
        };

        let file_hash = cache::hash_file_contents(&source_bytes);

        // Check cache
        if let Some(ref cache) = prev_cache {
            if !cache.file_changed(&relative_str, &file_hash) {
                all_entities.extend(
                    prev_entities.iter().filter(|e| e.file == relative_str).cloned()
                );
                if include_refs {
                    all_refs.extend(
                        prev_refs.iter().filter(|r| r.file == relative_str).cloned()
                    );
                }
                new_cache.files.insert(relative_str, file_hash);
                cached_count += 1;
                continue;
            }
        }

        let ext = filepath.extension().and_then(|e| e.to_str()).unwrap_or("");
        let lang = if ext == "json" {
            "json"
        } else if ext == "yaml" || ext == "yml" {
            "yaml"
        } else if ext == "toml" {
            "toml"
        } else if ext == "md" || ext == "markdown" || ext == "mdx" {
            "markdown"
        } else {
            match crate::parser::languages::detect_language(ext) {
                Some(l) => l,
                None => {
                    if verbose {
                        eprintln!("skip (unsupported): {}", relative_str);
                    }
                    continue;
                }
            }
        };

        let source = match String::from_utf8(source_bytes) {
            Ok(s) => s,
            Err(_) => {
                if verbose {
                    eprintln!("skip (not UTF-8): {}", relative_str);
                }
                continue;
            }
        };

        match parse_single_file(&source, &relative_str, lang) {
            Ok((file_entities, file_refs)) => {
                let file_entity_count = file_entities.len();
                all_entities.extend(file_entities);
                if include_refs {
                    all_refs.extend(file_refs);
                }
                new_cache.files.insert(relative_str.clone(), file_hash);
                parsed_count += 1;
                if verbose {
                    eprintln!("indexed: {} ({} entities)", relative_str, file_entity_count);
                }
            }
            Err(e) => {
                if verbose {
                    eprintln!("skip (parse error): {}: {}", relative_str, e);
                }
                continue;
            }
        }
    }

    // Sort deterministically
    all_entities.sort_by(|a, b| {
        a.file.cmp(&b.file).then(a.line_start.cmp(&b.line_start))
    });
    all_refs.sort_by(|a, b| {
        a.file.cmp(&b.file).then(a.line.cmp(&b.line))
    });

    // Save cache
    std::fs::create_dir_all(&sigil_dir).ok();
    new_cache.save(&sigil_dir).ok();

    if verbose {
        eprintln!(
            "done: {} files parsed, {} cached, {} entities total",
            parsed_count, cached_count, all_entities.len()
        );
    }

    IndexResult { entities: all_entities, refs: all_refs }
}

/// Check if a parser-emitted entity kind represents an import.
fn is_import_kind(kind: &str) -> bool {
    kind == "import" || kind == "use" || kind == "package"
}

/// Scan backwards from `line_start` to find all consecutive decorator/attribute lines.
fn find_decorator_start(source: &str, line_start: usize, lang: &str) -> usize {
    let all_lines: Vec<&str> = source.lines().collect();
    let mut start = line_start;
    while start > 1 {
        let prev_line = all_lines[start - 2].trim();
        let is_decorator = match lang {
            "python" => prev_line.starts_with('@'),
            "rust" => prev_line.starts_with("#[") || prev_line.starts_with("#!["),
            "java" | "csharp" | "kotlin" => prev_line.starts_with('@'),
            "typescript" | "javascript" | "tsx" => prev_line.starts_with('@'),
            _ => false,
        };
        if is_decorator {
            start -= 1;
        } else {
            break;
        }
    }
    start
}

fn normalize_kind(kind: &str) -> String {
    match kind {
        "trait_impl" => "impl".to_string(),
        "field" => "property".to_string(),
        "procedure" => "function".to_string(),
        other => other.to_string(),
    }
}

fn discover_source_files(root: &Path) -> Vec<PathBuf> {
    ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .build()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_some_and(|ft| ft.is_file()))
        .filter(|entry| {
            entry.path().extension()
                .and_then(|e| e.to_str())
                .map(|ext| {
                    ext == "md" || ext == "markdown" || ext == "mdx"
                        || ext == "json" || ext == "yaml" || ext == "yml" || ext == "toml"
                        || crate::parser::languages::detect_language(ext).is_some()
                })
                .unwrap_or(false)
        })
        .map(|entry| entry.into_path())
        .collect()
}

fn load_previous_entities(sigil_dir: &Path) -> Vec<Entity> {
    let path = sigil_dir.join("entities.jsonl");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    content.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn load_previous_refs(sigil_dir: &Path) -> Vec<Reference> {
    let path = sigil_dir.join("refs.jsonl");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    content.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_kind_mappings() {
        assert_eq!(normalize_kind("trait_impl"), "impl");
        assert_eq!(normalize_kind("field"), "property");
        assert_eq!(normalize_kind("procedure"), "function");
        assert_eq!(normalize_kind("function"), "function");
        assert_eq!(normalize_kind("class"), "class");
    }

    #[test]
    fn load_previous_entities_missing_file() {
        let entities = load_previous_entities(Path::new("/nonexistent"));
        assert!(entities.is_empty());
    }

    #[test]
    fn load_previous_entities_roundtrip() {
        let dir = std::env::temp_dir().join("sigil_index_test");
        std::fs::create_dir_all(&dir).unwrap();
        let entity_json = r#"{"file":"a.py","name":"foo","kind":"function","line_start":1,"line_end":2,"parent":null,"sig":"def foo():","meta":null,"body_hash":"abc","sig_hash":"def","struct_hash":"ghi"}"#;
        std::fs::write(dir.join("entities.jsonl"), format!("{}\n", entity_json)).unwrap();
        let entities = load_previous_entities(&dir);
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].name, "foo");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_previous_refs_missing_file() {
        let refs = load_previous_refs(Path::new("/nonexistent"));
        assert!(refs.is_empty());
    }

    #[test]
    fn parse_single_file_python() {
        let source = "def foo(x: int) -> bool:\n    return True\n";
        let (entities, _refs) = parse_single_file(source, "test.py", "python").unwrap();
        assert!(!entities.is_empty());
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"foo"));
    }

    #[test]
    fn parse_single_file_rust() {
        let source = "pub fn bar(x: i32) -> bool {\n    true\n}\n";
        let (entities, _refs) = parse_single_file(source, "test.rs", "rust").unwrap();
        assert!(!entities.is_empty());
    }

    #[test]
    fn parse_single_file_empty_source() {
        let (entities, refs) = parse_single_file("", "test.py", "python").unwrap();
        assert!(entities.is_empty());
        assert!(refs.is_empty());
    }

    #[test]
    fn parse_single_file_javascript_jsdoc_attaches_to_function() {
        let source = "/** Compute the answer. */\nfunction compute() { return 42; }\n";
        let (entities, _refs) = parse_single_file(source, "lib.js", "javascript").unwrap();
        let f = entities.iter().find(|e| e.name == "compute").unwrap();
        assert_eq!(f.doc.as_deref(), Some("Compute the answer."));
    }

    #[test]
    fn parse_single_file_javascript_jsdoc_attaches_to_const() {
        let source = "/** API endpoint URL. */\nexport const API_URL = \"https://x\";\n";
        let (entities, _refs) = parse_single_file(source, "lib.js", "javascript").unwrap();
        let c = entities.iter().find(|e| e.name == "API_URL").unwrap();
        assert_eq!(c.doc.as_deref(), Some("API endpoint URL."));
    }

    #[test]
    fn parse_single_file_typescript_jsdoc_attaches_to_function() {
        let source = "/** Greets the user. */\nexport function greet(name: string): string { return name; }\n";
        let (entities, _refs) = parse_single_file(source, "lib.ts", "typescript").unwrap();
        let f = entities.iter().find(|e| e.name == "greet").unwrap();
        assert_eq!(f.doc.as_deref(), Some("Greets the user."));
    }

    #[test]
    fn parse_single_file_typescript_jsdoc_attaches_to_interface() {
        let source = "/** A user record. */\nexport interface User { name: string; }\n";
        let (entities, _refs) = parse_single_file(source, "lib.ts", "typescript").unwrap();
        let i = entities.iter().find(|e| e.name == "User").unwrap();
        assert_eq!(i.doc.as_deref(), Some("A user record."));
    }

    #[test]
    fn parse_single_file_java_javadoc_attaches_to_class() {
        let source = "/** A point in 2D space. */\npublic class Point { public int x; }\n";
        let (entities, _refs) = parse_single_file(source, "Point.java", "java").unwrap();
        let c = entities.iter().find(|e| e.name == "Point").unwrap();
        assert_eq!(c.doc.as_deref(), Some("A point in 2D space."));
    }

    #[test]
    fn parse_single_file_java_javadoc_attaches_to_method() {
        let source = "public class Foo {\n    /** Doubles its input. */\n    public int twice(int x) { return x * 2; }\n}\n";
        let (entities, _refs) = parse_single_file(source, "Foo.java", "java").unwrap();
        let m = entities.iter().find(|e| e.name.ends_with("twice")).unwrap();
        assert_eq!(m.doc.as_deref(), Some("Doubles its input."));
    }

    #[test]
    fn parse_single_file_csharp_xmldoc_attaches_to_method() {
        let source = "public class Foo {\n    /// <summary>Doubles its input.</summary>\n    public int Twice(int x) { return x * 2; }\n}\n";
        let (entities, _refs) = parse_single_file(source, "Foo.cs", "csharp").unwrap();
        let m = entities.iter().find(|e| e.name.ends_with("Twice")).unwrap();
        assert!(
            m.doc.as_deref().unwrap_or("").contains("Doubles its input"),
            "missing doc body: {:?}",
            m.doc
        );
    }

    #[test]
    fn parse_single_file_cpp_doxygen_attaches_to_function() {
        // C++ uses Doxygen — /** is the canonical form.
        let source = "/** Returns the answer. */\nint answer() { return 42; }\n";
        let (entities, _refs) = parse_single_file(source, "lib.cpp", "cpp").unwrap();
        let f = entities.iter().find(|e| e.name == "answer").unwrap();
        assert_eq!(f.doc.as_deref(), Some("Returns the answer."));
    }

    #[test]
    fn parse_single_file_cpp_triple_slash_attaches_to_function() {
        let source = "/// Returns the answer.\nint answer() { return 42; }\n";
        let (entities, _refs) = parse_single_file(source, "lib.cpp", "cpp").unwrap();
        let f = entities.iter().find(|e| e.name == "answer").unwrap();
        assert_eq!(f.doc.as_deref(), Some("Returns the answer."));
    }

    #[test]
    fn parse_single_file_go_function_picks_up_preceding_godoc_comment() {
        // godoc convention: a comment line immediately preceding the
        // declaration with no blank line in between is the doc.
        let source = "package main\n\n// Compute returns the answer.\nfunc Compute() int {\n    return 42\n}\n";
        let (entities, _refs) = parse_single_file(source, "main.go", "go").unwrap();
        let f = entities.iter().find(|e| e.name == "Compute").unwrap();
        assert_eq!(f.doc.as_deref(), Some("Compute returns the answer."));
    }

    #[test]
    fn parse_single_file_go_function_blank_line_severs_doc() {
        // Comment + blank line + func is NOT a godoc — the blank line
        // disassociates them. Sigil should respect that.
        let source = "package main\n\n// detached comment\n\nfunc Bar() {}\n";
        let (entities, _refs) = parse_single_file(source, "main.go", "go").unwrap();
        let f = entities.iter().find(|e| e.name == "Bar").unwrap();
        assert!(
            f.doc.is_none(),
            "blank line must sever doc-comment association, got {:?}",
            f.doc
        );
    }

    #[test]
    fn parse_single_file_rust_function_picks_up_preceding_triple_slash() {
        let source = "/// Compute the answer.\npub fn compute() -> i32 { 42 }\n";
        let (entities, _refs) = parse_single_file(source, "lib.rs", "rust").unwrap();
        let f = entities.iter().find(|e| e.name == "compute").unwrap();
        assert_eq!(f.doc.as_deref(), Some("Compute the answer."));
    }

    #[test]
    fn parse_single_file_rust_function_collapses_multiline_doc() {
        let source = "/// Line one.\n/// Line two.\npub fn x() {}\n";
        let (entities, _refs) = parse_single_file(source, "lib.rs", "rust").unwrap();
        let f = entities.iter().find(|e| e.name == "x").unwrap();
        // Multiple /// lines join with newlines preserved as paragraph break.
        assert!(
            f.doc.as_deref().unwrap_or("").contains("Line one"),
            "missing line one: {:?}",
            f.doc
        );
        assert!(
            f.doc.as_deref().unwrap_or("").contains("Line two"),
            "missing line two: {:?}",
            f.doc
        );
    }

    #[test]
    fn parse_single_file_python_function_doc_carried_into_entity() {
        // The aider example from issue #12: docstring is the single best
        // description of intent and should reach `code.context` consumers.
        let source = "def tags_cache_error(self, original_error=None):\n    \"\"\"Handle SQLite errors by trying to recreate cache, falling back to dict if needed\"\"\"\n    pass\n";
        let (entities, _refs) = parse_single_file(source, "repomap.py", "python").unwrap();
        let f = entities
            .iter()
            .find(|e| e.name == "tags_cache_error")
            .unwrap();
        assert_eq!(
            f.doc.as_deref(),
            Some("Handle SQLite errors by trying to recreate cache, falling back to dict if needed")
        );
    }

    #[test]
    fn parse_single_file_python_method_doc_carried_into_entity() {
        // Issue #12 headline example — RepoMap.tags_cache_error in aider.
        let source = "class RepoMap:\n    def tags_cache_error(self, original_error=None):\n        \"\"\"Handle SQLite errors by trying to recreate cache, falling back to dict if needed\"\"\"\n        pass\n";
        let (entities, _refs) = parse_single_file(source, "repomap.py", "python").unwrap();
        let m = entities
            .iter()
            .find(|e| e.name == "RepoMap.tags_cache_error")
            .expect("method not extracted");
        assert_eq!(
            m.doc.as_deref(),
            Some("Handle SQLite errors by trying to recreate cache, falling back to dict if needed")
        );
    }

    #[test]
    fn parse_single_file_python_class_doc_carried_into_entity() {
        let source = "class RepoMap:\n    \"\"\"Builds a code map for an LLM agent.\"\"\"\n    pass\n";
        let (entities, _refs) = parse_single_file(source, "repomap.py", "python").unwrap();
        let c = entities.iter().find(|e| e.name == "RepoMap").unwrap();
        assert_eq!(c.doc.as_deref(), Some("Builds a code map for an LLM agent."));
    }

    #[test]
    fn parse_single_file_python_function_without_doc_has_none() {
        let source = "def foo():\n    pass\n";
        let (entities, _refs) = parse_single_file(source, "x.py", "python").unwrap();
        let f = entities.iter().find(|e| e.name == "foo").unwrap();
        assert!(f.doc.is_none(), "no docstring → no doc field");
    }

    #[test]
    fn parse_single_file_python_module_constant_carries_value_as_sig() {
        let source = "RETRY_TIMEOUT = 60\n";
        let (entities, _refs) = parse_single_file(source, "config.py", "python").unwrap();
        let c = entities
            .iter()
            .find(|e| e.name == "RETRY_TIMEOUT")
            .expect("RETRY_TIMEOUT entity not emitted");
        assert_eq!(c.kind, "constant");
        assert_eq!(c.sig.as_deref(), Some("60"));
    }

    #[test]
    fn parse_single_file_python_module_variable_carries_value_as_sig() {
        let source = "debug_mode = True\n";
        let (entities, _refs) = parse_single_file(source, "config.py", "python").unwrap();
        let v = entities
            .iter()
            .find(|e| e.name == "debug_mode")
            .expect("debug_mode entity not emitted");
        assert_eq!(v.kind, "variable");
        assert_eq!(v.sig.as_deref(), Some("True"));
    }

    #[test]
    fn parse_single_file_python_string_constant_keeps_quotes_in_sig() {
        // Issue example: ANTHROPIC_BETA_HEADER = "prompt-caching-2024-07-31,…"
        let source = "API_VERSION = \"v1.2.3\"\n";
        let (entities, _refs) = parse_single_file(source, "client.py", "python").unwrap();
        let c = entities.iter().find(|e| e.name == "API_VERSION").unwrap();
        assert_eq!(c.kind, "constant");
        // Quotes preserved verbatim — consumers shouldn't have to guess how
        // a value was spelled.
        assert_eq!(c.sig.as_deref(), Some("\"v1.2.3\""));
    }

    #[test]
    fn parse_single_file_rust_const_carries_value_as_sig() {
        let source = "pub const MAX_RETRIES: usize = 5;\n";
        let (entities, _refs) = parse_single_file(source, "lib.rs", "rust").unwrap();
        let c = entities
            .iter()
            .find(|e| e.name == "MAX_RETRIES")
            .expect("MAX_RETRIES not emitted");
        assert_eq!(c.kind, "constant");
        assert_eq!(c.sig.as_deref(), Some("5"));
    }

    #[test]
    fn parse_single_file_rust_static_carries_value_as_sig() {
        let source = "static GREETING: &str = \"hello\";\n";
        let (entities, _refs) = parse_single_file(source, "lib.rs", "rust").unwrap();
        let c = entities
            .iter()
            .find(|e| e.name == "GREETING")
            .expect("GREETING not emitted");
        assert_eq!(c.kind, "constant");
        assert_eq!(c.sig.as_deref(), Some("\"hello\""));
    }

    #[test]
    fn parse_single_file_go_const_carries_value_as_sig() {
        let source = "package main\n\nconst MaxRetries = 5\n";
        let (entities, _refs) = parse_single_file(source, "main.go", "go").unwrap();
        let c = entities
            .iter()
            .find(|e| e.name == "MaxRetries")
            .expect("MaxRetries not emitted");
        assert_eq!(c.kind, "constant");
        assert_eq!(c.sig.as_deref(), Some("5"));
    }

    #[test]
    fn parse_single_file_go_const_block_carries_each_value() {
        let source = "package main\n\nconst (\n    MaxA = 1\n    MaxB = 2\n)\n";
        let (entities, _refs) = parse_single_file(source, "main.go", "go").unwrap();
        let a = entities.iter().find(|e| e.name == "MaxA").unwrap();
        let b = entities.iter().find(|e| e.name == "MaxB").unwrap();
        assert_eq!(a.sig.as_deref(), Some("1"));
        assert_eq!(b.sig.as_deref(), Some("2"));
    }

    #[test]
    fn parse_single_file_go_var_carries_value_as_sig() {
        let source = "package main\n\nvar Workers = 15\n";
        let (entities, _refs) = parse_single_file(source, "main.go", "go").unwrap();
        let v = entities.iter().find(|e| e.name == "Workers").unwrap();
        assert_eq!(v.kind, "variable");
        assert_eq!(v.sig.as_deref(), Some("15"));
    }

    #[test]
    fn parse_single_file_typescript_const_carries_value_as_sig() {
        let source = "export const FOO = 42;\n";
        let (entities, _refs) = parse_single_file(source, "lib.ts", "typescript").unwrap();
        let c = entities.iter().find(|e| e.name == "FOO").expect("FOO not emitted");
        assert_eq!(c.kind, "constant");
        assert_eq!(c.sig.as_deref(), Some("42"));
    }

    #[test]
    fn parse_single_file_javascript_const_carries_value_as_sig() {
        let source = "const API_URL = \"https://api.example.com\";\n";
        let (entities, _refs) = parse_single_file(source, "client.js", "javascript").unwrap();
        let c = entities.iter().find(|e| e.name == "API_URL").unwrap();
        assert_eq!(c.kind, "constant");
        assert_eq!(c.sig.as_deref(), Some("\"https://api.example.com\""));
    }

    #[test]
    fn parse_single_file_typescript_arrow_function_keeps_function_kind() {
        // Regression guard — `const handler = () => …` should still be kind=function,
        // and we don't want the new sig wiring to clobber the function signature path.
        let source = "const handler = (req: Request) => { return req.url; };\n";
        let (entities, _refs) = parse_single_file(source, "h.ts", "typescript").unwrap();
        let h = entities.iter().find(|e| e.name == "handler").unwrap();
        assert_eq!(h.kind, "function");
    }

    #[test]
    fn parse_single_file_java_static_final_carries_value_as_sig() {
        let source = "class Foo {\n    public static final int MAX_SIZE = 100;\n}\n";
        let (entities, _refs) = parse_single_file(source, "Foo.java", "java").unwrap();
        let c = entities
            .iter()
            .find(|e| e.name.ends_with("MAX_SIZE"))
            .expect("MAX_SIZE not emitted");
        assert_eq!(c.kind, "constant");
        assert_eq!(c.sig.as_deref(), Some("100"));
    }

    #[test]
    fn parse_single_file_csharp_const_carries_value_as_sig() {
        let source = "class Foo {\n    public const int MAX_SIZE = 100;\n}\n";
        let (entities, _refs) = parse_single_file(source, "Foo.cs", "csharp").unwrap();
        let c = entities
            .iter()
            .find(|e| e.name.ends_with("MAX_SIZE"))
            .expect("MAX_SIZE not emitted");
        assert_eq!(c.kind, "constant");
        assert_eq!(c.sig.as_deref(), Some("100"));
    }

    #[test]
    fn parse_single_file_cpp_constexpr_classified_as_constant_with_sig() {
        let source = "constexpr int MAX_RETRIES = 5;\n";
        let (entities, _refs) = parse_single_file(source, "lib.cpp", "cpp").unwrap();
        let c = entities
            .iter()
            .find(|e| e.name == "MAX_RETRIES")
            .expect("MAX_RETRIES not emitted");
        assert_eq!(c.kind, "constant");
        assert_eq!(c.sig.as_deref(), Some("5"));
    }

    #[test]
    fn parse_single_file_cpp_define_carries_value_as_sig() {
        let source = "#define MAX_RETRIES 5\n";
        let (entities, _refs) = parse_single_file(source, "lib.cpp", "cpp").unwrap();
        let c = entities
            .iter()
            .find(|e| e.name == "MAX_RETRIES")
            .expect("MAX_RETRIES not emitted");
        assert_eq!(c.kind, "constant");
        assert_eq!(c.sig.as_deref(), Some("5"));
    }

    #[test]
    fn parse_single_file_python_long_constant_sig_truncates_with_ellipsis() {
        // A 300-char string literal — well above the 256-char cap.
        let payload = "x".repeat(300);
        let source = format!("BIG_BLOB = \"{payload}\"\n");
        let (entities, _refs) = parse_single_file(&source, "data.py", "python").unwrap();
        let c = entities.iter().find(|e| e.name == "BIG_BLOB").unwrap();
        let sig = c.sig.as_deref().unwrap();
        assert!(
            sig.chars().count() <= 257, // 256 + the trailing '…'
            "sig should be truncated, got {} chars",
            sig.chars().count()
        );
        assert!(
            sig.ends_with('…'),
            "truncated sig must end with the ellipsis sentinel, got {sig:?}"
        );
    }

    #[test]
    fn parse_single_file_python_class_level_constant_has_parent_and_sig() {
        let source = "class Config:\n    CACHE_VERSION = 3\n";
        let (entities, _refs) = parse_single_file(source, "config.py", "python").unwrap();
        let c = entities
            .iter()
            .find(|e| e.name.ends_with("CACHE_VERSION"))
            .expect("class-level CACHE_VERSION not emitted");
        assert_eq!(c.kind, "constant");
        assert_eq!(c.parent.as_deref(), Some("Config"));
        assert_eq!(c.sig.as_deref(), Some("3"));
    }
}
