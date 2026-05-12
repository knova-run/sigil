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
    // Proximity fallback for extractors that don't set TextEntry.parent to
    // the documented symbol's name. Builds a sorted list of docstrings by
    // end-line per file; the entity loop below looks up the docstring
    // whose end-line is closest-before the symbol's start-line (within a
    // small gap). This wires KDoc / Scaladoc / Swift `///` / PHPDoc to
    // their target declarations without needing a per-extractor refactor.
    let mut docs_by_file_end_line: std::collections::HashMap<&str, Vec<(u32, &str)>> =
        std::collections::HashMap::new();
    for t in &texts {
        if t.kind == "docstring" {
            docs_by_file_end_line
                .entry(t.file.as_str())
                .or_default()
                .push((t.line[1], t.text.as_str()));
        }
    }
    for v in docs_by_file_end_line.values_mut() {
        v.sort_by_key(|&(end, _)| end);
    }

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
            .copied()
            .or_else(|| nearest_doc_before(&docs_by_file_end_line, &sym.file, sym.line[0]))
            .and_then(crate::entity::truncate_doc);

        let parent = sym.parent.clone();
        let qualified_name = crate::entity::compose_qualified_name(parent.as_deref(), &sym.name);
        // Translate parser-side heritage tuples to the on-disk shape.
        let heritage: Vec<crate::entity::HeritageEdge> = sym
            .heritage
            .iter()
            .map(|(kind, target)| crate::entity::HeritageEdge {
                kind: kind.clone(),
                target: target.clone(),
            })
            .collect();
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
            heritage,
        });
    }

    for refentry in &references {
        refs.push(Reference {
            file: file_path.to_string(),
            caller: refentry.caller.clone(),
            name: refentry.name.clone(),
            ref_kind: refentry.kind.clone(),
            line: refentry.line[0],
            confidence: refentry.confidence,
            callee_id: None,
        });
    }

    Ok((entities, refs))
}

pub fn build_index(
    root: &Path,
    files: Option<&[PathBuf]>,
    full: bool,
    include_refs: bool,
    tier3: bool,
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

    // Tier-3 call resolution: runs before the final sort so the resolver
    // sees stable input order. Mutates `all_refs` in place — may demote
    // false tier-1 edges (bare-name 1.0 with no actual same-file def) and
    // promote unique cross-file matches to confidence 0.5. Also appends
    // barrel-follow edges for JS/TS + Python re-exports at confidence 0.7.
    if tier3 && include_refs {
        let tsconfig = load_tsconfig_paths(root);
        let go_modules = load_go_modules(root);
        let php_psr4 = load_php_psr4(root);
        let cargo_workspace = load_cargo_workspace(root);
        resolve_tier3(&all_entities, &mut all_refs);
        resolve_tier2b_imported_fallback(&all_entities, &mut all_refs, tsconfig.as_ref());
        resolve_member_call(&all_entities, &mut all_refs);
        let extra = resolve_barrel_follow(&all_entities, &all_refs, tsconfig.as_ref());
        all_refs.extend(extra);
        let extra = resolve_go_module_imports(&all_entities, &all_refs, &go_modules);
        all_refs.extend(extra);
        let extra = resolve_php_psr4_imports(&all_entities, &all_refs, &php_psr4);
        all_refs.extend(extra);
        let extra = resolve_cargo_workspace_imports(&all_entities, &all_refs, &cargo_workspace);
        all_refs.extend(extra);
        let extra = resolve_rails_autoload(&all_entities, &all_refs);
        all_refs.extend(extra);
        let swift_spm = load_swift_spm(root);
        let extra = resolve_swift_spm_imports(&all_entities, &all_refs, &swift_spm);
        all_refs.extend(extra);
        let extra = resolve_jvm_fqn_imports(&all_entities, &all_refs);
        all_refs.extend(extra);
        let compile_commands = load_compile_commands(root);
        let extra = resolve_cpp_includes(&all_entities, &all_refs, &compile_commands);
        all_refs.extend(extra);
        let dotnet = load_dotnet_index(root);
        let extra = resolve_csharp_usings(&all_entities, &all_refs, &dotnet);
        all_refs.extend(extra);
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
/// Look up a docstring that ends just before `symbol_line_start` on the
/// same file. The doc must end within `MAX_GAP` lines of the symbol —
/// allows for one blank separator (`/** */`-then-blank-then-`fn`) but
/// rejects unrelated docstrings further upstream.
///
/// `docs_by_file` maps `file → sorted [(end_line, text)]` (sort by
/// end_line ascending).
fn nearest_doc_before<'a>(
    docs_by_file: &'a std::collections::HashMap<&str, Vec<(u32, &'a str)>>,
    file: &str,
    symbol_line_start: u32,
) -> Option<&'a str> {
    const MAX_GAP: u32 = 2;
    if symbol_line_start == 0 {
        return None;
    }
    let docs = docs_by_file.get(file)?;
    // Largest end_line strictly less than symbol_line_start.
    let pos = docs.partition_point(|&(end, _)| end < symbol_line_start);
    if pos == 0 {
        return None;
    }
    let (end_line, text) = docs[pos - 1];
    if symbol_line_start - end_line <= MAX_GAP {
        Some(text)
    } else {
        None
    }
}

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

/// Entity kinds that participate in tier-3 call resolution. These are the
/// kinds you can actually `call(...)`; data types, modules, imports and the
/// like are excluded so a `Foo` data class doesn't shadow a `Foo()` function
/// in another file.
const CALLABLE_KINDS: &[&str] = &["function", "fn", "method", "constructor"];

fn is_callable_kind(kind: &str) -> bool {
    CALLABLE_KINDS.contains(&kind)
}

/// Map a file path (by extension) to the language tag the parsers use.
/// `None` for files we don't parse as code (json/yaml/toml/markdown).
fn language_from_file(file: &str) -> Option<&'static str> {
    let ext = file.rsplit('.').next().unwrap_or("");
    match ext {
        "py" => Some("python"),
        "rs" => Some("rust"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "ts" | "tsx" => Some("typescript"),
        "go" => Some("go"),
        "rb" => Some("ruby"),
        "java" => Some("java"),
        "c" | "h" => Some("c"),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => Some("cpp"),
        "cs" => Some("csharp"),
        "swift" => Some("swift"),
        "kt" | "kts" => Some("kotlin"),
        "scala" | "sc" => Some("scala"),
        "php" => Some("php"),
        _ => None,
    }
}

/// Tier-3 call resolver — runs after all files are parsed and before the
/// final sort. Mirrors repowise's `CallResolver._resolve_free_call` (issue
/// #15 followup) but only the global-unique fallback piece — barrel
/// re-export following is a separate pass (JS/TS + Python only) layered on
/// top of this.
///
/// Behaviour:
///
///   * Demote false tier-1: a bare-identifier call tagged 1.0 by the
///     parser whose name does NOT match any same-file callable definition
///     is reset to None (parsers issue 1.0 optimistically for any bare
///     identifier; this pass is the first place we can verify same-file
///     match).
///   * Promote to tier-3 (0.5): an unresolved bare-name call (None
///     confidence after the demote) whose name appears as a callable
///     definition exactly once across the whole index — and only in
///     files of the same language as the caller — gets confidence 0.5.
///   * Tier-2 edges (0.8) and member/scoped calls (`.`/`::`/`/` in the
///     name) are left untouched — those resolve through file-local imports
///     and are owned by the per-parser resolvers.
fn resolve_tier3(entities: &[Entity], refs: &mut [Reference]) {
    use std::collections::HashMap;

    // (file, name) → exists — callable defs in that file
    let mut same_file: std::collections::HashSet<(&str, &str)> =
        std::collections::HashSet::new();
    // name → Vec<&file> — every callable def of that name
    let mut globals: HashMap<&str, Vec<&str>> = HashMap::new();

    for e in entities {
        if !is_callable_kind(&e.kind) {
            continue;
        }
        same_file.insert((e.file.as_str(), e.name.as_str()));
        globals.entry(e.name.as_str()).or_default().push(e.file.as_str());
    }

    for r in refs.iter_mut() {
        if r.ref_kind != "call" {
            continue;
        }
        // Only bare names participate. Member/scoped/resolved-form names
        // (containing `.`, `::`, or `/`) belong to tier-2 or are already
        // unresolved attribute chains.
        if r.name.contains('.') || r.name.contains("::") || r.name.contains('/') {
            continue;
        }
        // Tier-2 (or any other non-None, non-tier-1 confidence) → leave alone.
        if !matches!(r.confidence, None | Some(1.0)) {
            continue;
        }
        // Verified tier-1: same-file def exists, leave at 1.0.
        if same_file.contains(&(r.file.as_str(), r.name.as_str())) {
            continue;
        }
        // Tier-1 was optimistic — demote.
        if r.confidence == Some(1.0) {
            r.confidence = None;
        }
        // Tier-3 global-unique check (language-gated).
        let caller_lang = language_from_file(&r.file);
        let Some(defs) = globals.get(r.name.as_str()) else { continue };
        let same_lang_defs: Vec<&&str> = defs
            .iter()
            .filter(|f| language_from_file(f) == caller_lang)
            .collect();
        if same_lang_defs.len() == 1 {
            r.confidence = Some(0.5);
        }
    }
}

/// P0.3 — tier-2b fallback (imported-file scan).
///
/// Ports repowise's tier-2b free-call branch (call_resolver.py:228-234).
/// When a bare-name call has no same-file def and no specific import alias
/// — and `resolve_tier3`'s global-unique check failed (0 or >1 matches) —
/// scan the caller's `import` entities. If exactly one of the resolved
/// imported files defines the called name as a callable, bind at 0.85.
///
/// Closes the gap for `from utils import *` and other unbound-name
/// resolutions where global ambiguity is broken by the caller's imports.
fn resolve_tier2b_imported_fallback(
    entities: &[Entity],
    refs: &mut [Reference],
    tsconfig: Option<&TsconfigPaths>,
) {
    use std::collections::{HashMap, HashSet};

    let file_set: HashSet<&str> = entities.iter().map(|e| e.file.as_str()).collect();

    // file → set of locally-defined callable names.
    let mut callables_by_file: HashMap<&str, HashSet<&str>> = HashMap::new();
    // file → list of import module specifiers.
    let mut imports_by_file: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in entities {
        if is_callable_kind(&e.kind) {
            callables_by_file
                .entry(e.file.as_str())
                .or_default()
                .insert(e.name.as_str());
        }
        if e.kind == "import" {
            imports_by_file
                .entry(e.file.as_str())
                .or_default()
                .push(e.name.as_str());
        }
    }

    for r in refs.iter_mut() {
        if r.ref_kind != "call" {
            continue;
        }
        // Bare names only.
        if r.name.contains('.') || r.name.contains("::") || r.name.contains('/') {
            continue;
        }
        // Don't downgrade higher-confidence bindings; >0.85 already wins.
        if matches!(r.confidence, Some(c) if c > 0.85) {
            continue;
        }
        let Some(imports) = imports_by_file.get(r.file.as_str()) else { continue };
        // Distinct imported files that contain the called name.
        let mut hit_files: HashSet<String> = HashSet::new();
        for module in imports {
            // Strip Python star-import suffix `.*` to recover the module
            // specifier (`utils.*` → `utils`). For `import utils` the
            // specifier is already `utils` — no-op.
            let normalized = module.trim_end_matches(".*");
            let Some(target) = resolve_module_path(&r.file, normalized, &file_set, tsconfig) else { continue };
            if let Some(callables) = callables_by_file.get(target.as_str()) {
                if callables.contains(r.name.as_str()) {
                    hit_files.insert(target);
                }
            }
        }
        if hit_files.len() == 1 {
            r.confidence = Some(0.85);
        }
    }
}

/// Member-call resolution — ports repowise's `_resolve_member_call`
/// (call_resolver.py:247-313). Handles two strategies today:
///
///   * **Strategy 3 — `self`/`this` (0.95):** receiver is `self` (Python/Ruby)
///     or `this` (Java/Kotlin/JS/TS/C#/Swift). The binding is unambiguously
///     the caller's own class. Look up the method on that class.
///   * **Strategy 2 — known-class receiver (0.93 same-file):** receiver is
///     the bare name of a class entity in the same file. The binding is the
///     static/class method by that name.
///
/// Imported-class variant of Strategy 2 (0.88) is a follow-on once the
/// caller-side import table is exposed here. Strategy 1 (module-alias
/// receiver) is owned by per-parser tier-2 resolvers today.
///
/// Built on shared (file, class, method) and (file, class) indices over
/// `kind=="method"` / `kind=="class"` entities. Runs after `resolve_tier3`,
/// which skips names containing `.` so member calls are untouched.
fn resolve_member_call(entities: &[Entity], refs: &mut [Reference]) {
    use std::collections::{HashMap, HashSet};

    // (file, class_name, method_leaf) — every method definition.
    let mut methods: HashSet<(&str, &str, &str)> = HashSet::new();
    // (file, class_name) — every class definition.
    let mut classes: HashSet<(&str, &str)> = HashSet::new();
    // class_name → list of files defining `class class_name` with a method
    // of name X (filled by walking entities once). Used for Strategy-2
    // imported-class lookup (global-unique check). Key is (class, method).
    let mut global_class_methods: HashMap<(&str, &str), Vec<&str>> = HashMap::new();
    for e in entities {
        if e.kind == "class" {
            classes.insert((e.file.as_str(), e.name.as_str()));
            continue;
        }
        if e.kind != "method" {
            continue;
        }
        let Some(parent) = e.parent.as_deref() else { continue };
        let leaf = e
            .name
            .strip_prefix(&format!("{parent}."))
            .or_else(|| e.name.strip_prefix(&format!("{parent}::")))
            .unwrap_or(e.name.as_str());
        methods.insert((e.file.as_str(), parent, leaf));
        global_class_methods
            .entry((parent, leaf))
            .or_default()
            .push(e.file.as_str());
    }

    for r in refs.iter_mut() {
        if r.ref_kind != "call" {
            continue;
        }
        // Split `receiver.leaf`. One-level chains only.
        let Some((head, leaf)) = r.name.rsplit_once('.') else { continue };
        if head.contains('.') || leaf.is_empty() {
            continue;
        }
        // Don't downgrade higher-confidence bindings.
        if matches!(r.confidence, Some(c) if c >= 0.95) {
            continue;
        }

        // Strategy 3: self/this receiver — binding is the caller's class.
        if head == "self" || head == "this" {
            if r.confidence.is_some() {
                continue;
            }
            let Some(caller) = r.caller.as_deref() else { continue };
            // The caller field shape varies by parser:
            //   * Python emits `caller = "Foo.a"` (parent.leaf form)
            //   * Rust/PHP emit `caller = "Foo::a"` (parent::leaf form)
            //   * Java emits `caller = "Foo"` (the class itself, no method leaf)
            // Strip method-leaf suffix when a separator is present; otherwise
            // the caller string is already the parent class.
            let parent_class = caller
                .rsplit_once("::")
                .or_else(|| caller.rsplit_once('.'))
                .map(|(p, _)| p)
                .unwrap_or(caller);
            if methods.contains(&(r.file.as_str(), parent_class, leaf)) {
                r.confidence = Some(0.95);
            }
            continue;
        }

        // Strategy 2 (same-file): receiver is a class defined in this file.
        if classes.contains(&(r.file.as_str(), head))
            && methods.contains(&(r.file.as_str(), head, leaf))
        {
            if r.confidence.is_none() {
                r.confidence = Some(0.93);
            }
            continue;
        }

        // Strategy 2 (imported): receiver names a class whose definition
        // lives in a different file, and that class has the called method.
        // We approximate "imported into caller's file" with a same-language
        // global-unique check — if exactly one file in the index defines
        // `class head` with method `leaf` (same language as the caller),
        // the binding is unambiguous. Upgrades tier-2's 0.8 to 0.88.
        let caller_lang = language_from_file(&r.file);
        if let Some(files) = global_class_methods.get(&(head, leaf)) {
            let same_lang: Vec<&&str> = files
                .iter()
                .filter(|f| language_from_file(f) == caller_lang)
                .collect();
            if same_lang.len() == 1 {
                match r.confidence {
                    None => r.confidence = Some(0.88),
                    Some(c) if c <= 0.85 => r.confidence = Some(0.88),
                    _ => {}
                }
            }
        }
    }
}

/// Barrel-follow tier-3 pass. Detects files that re-export a name without
/// defining it locally (JS/TS `export { x } from "./y"`, Python
/// `__init__.py` doing `from .pkg import x`) and emits an additional edge
/// at confidence 0.7 pointing at the underlying file, for every tier-2
/// edge that lands on a barrel.
///
/// Mirrors repowise's `_follow_barrel_exports` heuristic
/// (call_resolver.py:98-110) — one-hop: if the chain is barrel→barrel→def
/// we only follow the first hop, which covers the common case.
///
/// Scope: JS/TS and Python only — these are the ecosystems where
/// re-exports through `index.{ts,js}` / `__init__.py` are idiomatic. Other
/// languages would need manifest-aware resolvers (Cargo.toml, go.mod,
/// composer.json, etc.) to map a textual import path to a file; that work
/// is a separate follow-up.
fn resolve_barrel_follow(
    entities: &[Entity],
    refs: &[Reference],
    tsconfig: Option<&TsconfigPaths>,
) -> Vec<Reference> {
    use std::collections::{HashMap, HashSet};

    let file_set: HashSet<&str> = entities.iter().map(|e| e.file.as_str()).collect();

    // Per-file map of locally-defined callable names.
    let mut local_defs: HashMap<&str, HashSet<&str>> = HashMap::new();
    for e in entities {
        if is_callable_kind(&e.kind) {
            local_defs.entry(e.file.as_str()).or_default().insert(e.name.as_str());
        }
    }

    // Per-file map of "imported name → source module string" — only the
    // imports we know about (Entity kind=="import"). The alias when present
    // gives the local binding name; otherwise the trailing segment of the
    // import's name.
    let mut file_imports: HashMap<&str, Vec<(String, String)>> = HashMap::new();
    for e in entities {
        if e.kind != "import" {
            continue;
        }
        // The Entity layer doesn't carry the SymbolEntry's alias, so we
        // back-derive the local binding from the import name's shape:
        // JS/TS + Python imports use `.` as the joiner, so the local name
        // is the trailing `.`-segment and the source is everything before
        // it. Wildcard imports (`pkg.*`) are skipped — they introduce no
        // single local binding.
        let trimmed = e.name.trim_end_matches(".*");
        let (source, local_name) = match trimmed.rsplit_once('.') {
            Some((src, leaf)) => (src.to_string(), leaf.to_string()),
            None => (trimmed.to_string(), trimmed.to_string()),
        };
        if source.is_empty() || local_name.is_empty() || local_name == "*" {
            continue;
        }
        file_imports
            .entry(e.file.as_str())
            .or_default()
            .push((local_name, source));
    }

    // Resolve textual import sources (`./utils`, `.pkg`) to real file paths,
    // and build the barrel map: (barrel_file, local_name) → underlying_file.
    // A file is a barrel for `name` if it imports `name` from somewhere
    // AND does not locally define `name`.
    let mut barrel_origins: HashMap<(&str, String), String> = HashMap::new();
    for (file, imps) in &file_imports {
        let defs = local_defs.get(file);
        for (local_name, src) in imps {
            let already_defined = defs.map(|d| d.contains(local_name.as_str())).unwrap_or(false);
            if already_defined {
                continue;
            }
            let Some(resolved) = resolve_module_path(file, src, &file_set, tsconfig) else {
                continue;
            };
            barrel_origins.insert((*file, local_name.clone()), resolved);
        }
    }
    if barrel_origins.is_empty() {
        return Vec::new();
    }

    // Walk tier-2 resolved-form refs (`<src>.<member>/<rest>`) and check
    // whether `<src>` resolves to a barrel for `<member>`. If so, emit an
    // additional edge against the underlying file.
    let mut additions: Vec<Reference> = Vec::new();
    for r in refs {
        if r.ref_kind != "call" {
            continue;
        }
        if r.confidence != Some(0.8) {
            continue;
        }
        // The resolved-form is `<import-path>.<local_name>/<rest>` per the
        // tier-2 convention. Split on the LAST `/` — the import path itself
        // can contain `/` (relative JS/TS module specifiers like `./a/b`).
        let Some(slash_at) = r.name.rfind('/') else { continue };
        let lhs = &r.name[..slash_at];
        let rest = &r.name[slash_at + 1..];
        let Some(dot_at) = lhs.rfind('.') else { continue };
        let import_src = &lhs[..dot_at];
        let local_name = &lhs[dot_at + 1..];
        let Some(resolved_barrel) = resolve_module_path(&r.file, import_src, &file_set, tsconfig) else {
            continue;
        };
        let key: (&str, String) = (resolved_barrel.as_str(), local_name.to_string());
        // We need a `&str` for the file slot but resolved_barrel is owned
        // — index into the file_set's stored borrow for the same path.
        let Some(file_borrow) = file_set.get(resolved_barrel.as_str()) else { continue };
        let key: (&str, String) = (*file_borrow, key.1);
        let Some(origin_module) = barrel_origins.get(&key) else { continue };
        // The barrel's underlying file is the import source it pointed at.
        // Emit an edge whose name is `<origin_file>/<rest>` so consumers can
        // distinguish this edge from the original tier-2 form.
        let leaf = if rest.is_empty() { local_name } else { rest };
        let edge_name = format!("{origin_module}/{leaf}");
        let callee_id = format!("{origin_module}::{leaf}");
        additions.push(Reference {
            file: r.file.clone(),
            caller: r.caller.clone(),
            name: edge_name,
            ref_kind: "call".to_string(),
            line: r.line,
            confidence: Some(0.7),
            callee_id: Some(callee_id),
        });
    }
    additions
}

/// Parsed `tsconfig.json` path mappings. Built once per index from
/// `compilerOptions.paths`. Patterns with a trailing `*` map a prefix to
/// a target prefix (e.g. `"@/*": ["src/*"]` → ("@/", "src/")). Patterns
/// without `*` are not currently supported (rare; exact-match aliases
/// are a follow-up).
#[derive(Debug, Clone, Default)]
pub struct TsconfigPaths {
    /// (prefix, target_prefix) pairs, sorted by prefix length descending so
    /// the longest match wins on a leftmost-prefix scan.
    mappings: Vec<(String, String)>,
}

/// Read `tsconfig.json` at index root and extract the `paths` mapping.
/// Tolerates a missing or malformed tsconfig by returning None — the
/// resolver falls back to relative probing in that case.
pub fn load_tsconfig_paths(root: &Path) -> Option<TsconfigPaths> {
    let content = std::fs::read_to_string(root.join("tsconfig.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    let paths = v.get("compilerOptions")?.get("paths")?.as_object()?;
    let mut mappings: Vec<(String, String)> = Vec::new();
    for (pattern, targets) in paths {
        let Some(targets) = targets.as_array() else { continue };
        let Some(target) = targets.first().and_then(|t| t.as_str()) else { continue };
        if let (Some(p), Some(t)) = (pattern.strip_suffix('*'), target.strip_suffix('*')) {
            let target_prefix = t.trim_start_matches("./").to_string();
            mappings.push((p.to_string(), target_prefix));
        }
    }
    if mappings.is_empty() {
        return None;
    }
    mappings.sort_by_key(|(p, _)| std::cmp::Reverse(p.len()));
    Some(TsconfigPaths { mappings })
}

/// Rewrite a JS/TS import specifier using tsconfig path mappings. Returns
/// None when no pattern matches; otherwise the rewritten path (relative to
/// the index root, ready to probe in `files`).
fn apply_tsconfig_paths(module: &str, ts: &TsconfigPaths) -> Option<String> {
    for (prefix, target) in &ts.mappings {
        if let Some(rest) = module.strip_prefix(prefix) {
            return Some(format!("{target}{rest}"));
        }
    }
    None
}

/// Parsed `go.mod` files — one entry per workspace module declaration.
/// Maps the canonical module path (e.g. `github.com/acme/myproj`) to the
/// filesystem prefix where its packages live (relative to the index root).
/// A repo with a single `go.mod` at the root has `fs_prefix == ""`;
/// nested modules at `services/api/go.mod` get `fs_prefix == "services/api"`.
#[derive(Debug, Clone, Default)]
pub struct GoModules {
    /// (canonical_path, fs_prefix) pairs, sorted by canonical-path length
    /// descending so the longest match wins for a given import.
    modules: Vec<(String, String)>,
}

/// Walk the index root for `go.mod` files, parse each `module <path>` line,
/// and return the resulting prefix map. `vendor/` directories are skipped
/// to avoid resolving back into vendored dependencies. Limited to a depth
/// of 4 to bound the walk on large monorepos.
pub fn load_go_modules(root: &Path) -> GoModules {
    fn walk(dir: &Path, root: &Path, depth: usize, modules: &mut Vec<(String, String)>) {
        if depth > 4 {
            return;
        }
        let Ok(read) = std::fs::read_dir(dir) else { return };
        for entry in read.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name_lossy = name.to_string_lossy();
            if path.is_dir() {
                if name_lossy == "vendor"
                    || name_lossy == "node_modules"
                    || name_lossy == ".git"
                    || name_lossy == ".sigil"
                    || name_lossy.starts_with('.')
                {
                    continue;
                }
                walk(&path, root, depth + 1, modules);
            } else if name_lossy == "go.mod" {
                let Ok(content) = std::fs::read_to_string(&path) else { continue };
                for line in content.lines() {
                    let line = line.trim();
                    if let Some(rest) = line.strip_prefix("module ") {
                        let canonical = rest.trim().trim_matches('"').to_string();
                        let fs_prefix = dir
                            .strip_prefix(root)
                            .ok()
                            .map(|p| p.to_string_lossy().replace('\\', "/"))
                            .unwrap_or_default();
                        modules.push((canonical, fs_prefix));
                        break;
                    }
                }
            }
        }
    }
    let mut modules = Vec::new();
    walk(root, root, 0, &mut modules);
    modules.sort_by_key(|(c, _)| std::cmp::Reverse(c.len()));
    GoModules { modules }
}

/// Resolve a Go canonical import path to a workspace-relative directory.
/// Returns None when no module prefix matches (the import is from outside
/// the workspace — likely a third-party dependency).
fn resolve_go_import(canonical: &str, go: &GoModules) -> Option<String> {
    for (prefix, fs_prefix) in &go.modules {
        let Some(rest) = canonical.strip_prefix(prefix.as_str()) else { continue };
        let rest = rest.trim_start_matches('/');
        let path = match (fs_prefix.as_str(), rest) {
            ("", "") => return Some(String::new()),
            ("", r) => r.to_string(),
            (p, "") => p.to_string(),
            (p, r) => format!("{p}/{r}"),
        };
        return Some(path);
    }
    None
}

/// Tier-3 file-resolution pass for Go. For each tier-2 0.8 call edge whose
/// name has form `<canonical-path>/<func>` and whose canonical path matches
/// a known workspace `go.mod`, locate the `.go` file in the corresponding
/// package directory that defines `<func>` and emit an additional edge at
/// confidence 0.7 pointing at that file. Mirrors the barrel-follow shape:
/// `name = "<file>/<func>"`.
fn resolve_go_module_imports(
    entities: &[Entity],
    refs: &[Reference],
    go: &GoModules,
) -> Vec<Reference> {
    use std::collections::HashMap;
    if go.modules.is_empty() {
        return Vec::new();
    }
    // pkg_dir → (name → file_path) for every Go callable.
    let mut by_pkg: HashMap<String, HashMap<String, String>> = HashMap::new();
    for e in entities {
        if !e.file.ends_with(".go") {
            continue;
        }
        if !is_callable_kind(&e.kind) {
            continue;
        }
        let dir = match e.file.rfind('/') {
            Some(i) => e.file[..i].to_string(),
            None => String::new(),
        };
        by_pkg
            .entry(dir)
            .or_default()
            .insert(e.name.clone(), e.file.clone());
    }

    let mut additions: Vec<Reference> = Vec::new();
    for r in refs {
        if r.ref_kind != "call" {
            continue;
        }
        if r.confidence != Some(0.8) {
            continue;
        }
        if !r.file.ends_with(".go") {
            continue;
        }
        // Tier-2 Go path form: `<canonical>/<func>`. Split on the last `/`.
        let Some(slash_at) = r.name.rfind('/') else { continue };
        let canonical = &r.name[..slash_at];
        let func = &r.name[slash_at + 1..];
        if func.is_empty() || func.contains('.') || func.contains('/') {
            continue;
        }
        let Some(pkg_dir) = resolve_go_import(canonical, go) else { continue };
        let Some(callables) = by_pkg.get(&pkg_dir) else { continue };
        let Some(file_path) = callables.get(func) else { continue };
        additions.push(Reference {
            file: r.file.clone(),
            caller: r.caller.clone(),
            name: format!("{file_path}/{func}"),
            ref_kind: "call".to_string(),
            line: r.line,
            confidence: Some(0.7),
            callee_id: Some(format!("{file_path}::{func}")),
        });
    }
    additions
}

/// Parsed `composer.json` `autoload.psr-4` (and `autoload-dev.psr-4`)
/// mappings — namespace prefix → filesystem directory (relative to the
/// index root). Sorted by namespace length descending so the longest
/// match wins on a prefix scan.
#[derive(Debug, Clone, Default)]
pub struct PhpPsr4 {
    /// (namespace_prefix_with_trailing_backslash, fs_dir_with_trailing_slash)
    /// e.g. ("App\\", "src/") for `"App\\": "src/"`.
    mappings: Vec<(String, String)>,
}

/// Read `composer.json` at index root and extract PSR-4 mappings from
/// both `autoload.psr-4` and `autoload-dev.psr-4`. Tolerates missing or
/// malformed files by returning an empty mapping.
pub fn load_php_psr4(root: &Path) -> PhpPsr4 {
    let Ok(content) = std::fs::read_to_string(root.join("composer.json")) else {
        return PhpPsr4::default();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) else {
        return PhpPsr4::default();
    };
    let mut mappings: Vec<(String, String)> = Vec::new();
    for section in &["autoload", "autoload-dev"] {
        let Some(psr4) = v.get(section).and_then(|s| s.get("psr-4")).and_then(|p| p.as_object())
        else {
            continue;
        };
        for (namespace, target) in psr4 {
            let Some(dir) = target.as_str() else { continue };
            // Normalize: namespace must end in `\\`, dir in `/`.
            let ns = if namespace.ends_with('\\') {
                namespace.clone()
            } else {
                format!("{namespace}\\")
            };
            let fs = dir.trim_start_matches("./").to_string();
            let fs = if fs.ends_with('/') || fs.is_empty() {
                fs
            } else {
                format!("{fs}/")
            };
            mappings.push((ns, fs));
        }
    }
    mappings.sort_by_key(|(n, _)| std::cmp::Reverse(n.len()));
    PhpPsr4 { mappings }
}

/// Apply the longest-prefix PSR-4 mapping to a fully-qualified namespace
/// path (e.g. `App\Service`). Returns the filesystem directory (with a
/// trailing `/`) under the index root, or None when no prefix matches.
fn apply_php_psr4(namespace: &str, psr4: &PhpPsr4) -> Option<String> {
    let ns_with_trail = format!("{namespace}\\");
    for (prefix, fs_dir) in &psr4.mappings {
        if let Some(rest) = ns_with_trail.strip_prefix(prefix.as_str()) {
            let suffix = rest.trim_end_matches('\\').replace('\\', "/");
            let out = match (fs_dir.as_str(), suffix.as_str()) {
                (fs, "") => fs.to_string(),
                (fs, s) => format!("{fs}{s}/"),
            };
            return Some(out);
        }
    }
    None
}

/// Tier-3 file-resolution pass for PHP. For each tier-2 0.8 call edge in
/// a `.php` file whose name has form `<canonical-namespace-path>/<rest>`,
/// strip the trailing function name from the canonical, PSR-4 the
/// remaining namespace to a directory, scan files in that directory for
/// a callable matching the function name, and emit a 0.7 edge with
/// `<file>/<rest_or_func>`.
///
/// Today this covers PHP free-function imports (`use function Foo\bar`)
/// — the case where the parser actually emits a call ref. Static-method
/// and instantiation refs are a parser-side follow-up.
fn resolve_php_psr4_imports(
    entities: &[Entity],
    refs: &[Reference],
    psr4: &PhpPsr4,
) -> Vec<Reference> {
    use std::collections::HashMap;
    if psr4.mappings.is_empty() {
        return Vec::new();
    }
    // dir → (callable_name → file_path) for every PHP callable.
    let mut by_dir: HashMap<String, HashMap<String, String>> = HashMap::new();
    for e in entities {
        if !e.file.ends_with(".php") {
            continue;
        }
        if !is_callable_kind(&e.kind) {
            continue;
        }
        let dir = match e.file.rfind('/') {
            Some(i) => format!("{}/", &e.file[..i]),
            None => String::new(),
        };
        by_dir
            .entry(dir)
            .or_default()
            .insert(e.name.clone(), e.file.clone());
    }

    let mut additions: Vec<Reference> = Vec::new();
    for r in refs {
        if r.ref_kind != "call" {
            continue;
        }
        if r.confidence != Some(0.8) {
            continue;
        }
        if !r.file.ends_with(".php") {
            continue;
        }
        // Tier-2 PHP form: `<canonical>/<rest>`. Split on last `/`.
        let Some(slash_at) = r.name.rfind('/') else { continue };
        let canonical = &r.name[..slash_at];
        let rest = &r.name[slash_at + 1..];
        // Canonical uses `\` separators. Split off the trailing leaf as
        // the function name when `rest` is empty (function-import form).
        let (namespace, func) = if rest.is_empty() {
            let Some(bs_at) = canonical.rfind('\\') else { continue };
            (&canonical[..bs_at], &canonical[bs_at + 1..])
        } else {
            (canonical, rest)
        };
        if func.is_empty() || func.contains('\\') || func.contains('/') {
            continue;
        }
        let Some(fs_dir) = apply_php_psr4(namespace, psr4) else { continue };
        let Some(callables) = by_dir.get(&fs_dir) else { continue };
        let Some(file_path) = callables.get(func) else { continue };
        additions.push(Reference {
            file: r.file.clone(),
            caller: r.caller.clone(),
            name: format!("{file_path}/{func}"),
            ref_kind: "call".to_string(),
            line: r.line,
            confidence: Some(0.7),
            callee_id: Some(format!("{file_path}::{func}")),
        });
    }
    additions
}

/// Parsed Cargo workspace map — `crate_name` (both hyphen and underscore
/// forms) → workspace-relative directory of the member crate. Rust source
/// uses underscored crate names (`use myapp_core`) while Cargo.toml's
/// `name` and the on-disk directory typically use hyphens (`myapp-core`).
/// We register both spellings up front so a lookup just hashes once.
#[derive(Debug, Clone, Default)]
pub struct CargoWorkspace {
    crates: std::collections::HashMap<String, String>,
}

/// Read the workspace `Cargo.toml` at the index root, expand `members`
/// globs, and parse each member's `[package] name`. Registers both
/// hyphenated and underscored variants of the crate name (mirrors what
/// rustc does — `use my_crate` for a `name = "my-crate"`). Tolerates a
/// missing or non-workspace Cargo.toml by returning an empty map.
pub fn load_cargo_workspace(root: &Path) -> CargoWorkspace {
    let Ok(content) = std::fs::read_to_string(root.join("Cargo.toml")) else {
        return CargoWorkspace::default();
    };
    let Ok(v) = toml::from_str::<toml::Value>(&content) else {
        return CargoWorkspace::default();
    };
    let Some(workspace) = v.get("workspace") else {
        return CargoWorkspace::default();
    };
    let members: Vec<String> = workspace
        .get("members")
        .and_then(|m| m.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let mut crates: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for member_pattern in &members {
        // Resolve glob patterns (`crates/*`) and bare paths uniformly.
        let candidates = expand_cargo_member_glob(root, member_pattern);
        for member_dir in candidates {
            let manifest = member_dir.join("Cargo.toml");
            let Ok(text) = std::fs::read_to_string(&manifest) else { continue };
            let Ok(mv) = toml::from_str::<toml::Value>(&text) else { continue };
            let Some(name) = mv.get("package").and_then(|p| p.get("name")).and_then(|n| n.as_str())
            else { continue };
            let rel = member_dir
                .strip_prefix(root)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            crates.insert(name.to_string(), rel.clone());
            // Rust source allows both hyphen → underscore aliasing.
            let underscored = name.replace('-', "_");
            if underscored != name {
                crates.insert(underscored, rel);
            }
        }
    }
    CargoWorkspace { crates }
}

/// Expand a single `[workspace] members` entry — either an exact path or a
/// trailing `*` glob (`crates/*`). Returns the directory paths that exist
/// and contain a `Cargo.toml`.
fn expand_cargo_member_glob(root: &Path, pattern: &str) -> Vec<std::path::PathBuf> {
    let pattern = pattern.trim_end_matches('/');
    if let Some(prefix) = pattern.strip_suffix("/*") {
        let base = root.join(prefix);
        let Ok(read) = std::fs::read_dir(&base) else { return Vec::new() };
        let mut out = Vec::new();
        for entry in read.flatten() {
            let p = entry.path();
            if p.is_dir() && p.join("Cargo.toml").is_file() {
                out.push(p);
            }
        }
        out
    } else {
        let p = root.join(pattern);
        if p.join("Cargo.toml").is_file() {
            vec![p]
        } else {
            Vec::new()
        }
    }
}

/// Tier-3 file-resolution pass for Rust Cargo workspaces. For each tier-2
/// 0.8 Rust call edge of form `<crate>::<path>/<rest>`, look up the
/// workspace crate, scan files under its directory for a callable matching
/// the trailing name, and emit a 0.7 edge pointing at the actual `.rs`
/// file. Mirrors the Go / PHP resolution pattern.
fn resolve_cargo_workspace_imports(
    entities: &[Entity],
    refs: &[Reference],
    cargo: &CargoWorkspace,
) -> Vec<Reference> {
    use std::collections::HashMap;
    if cargo.crates.is_empty() {
        return Vec::new();
    }
    // dir_prefix → (name → file_path) for every Rust callable, keyed by
    // the workspace-relative directory the file sits under (recursive —
    // we accept any file beneath the crate root).
    let mut callables: HashMap<String, HashMap<String, String>> = HashMap::new();
    for e in entities {
        if !e.file.ends_with(".rs") {
            continue;
        }
        if !is_callable_kind(&e.kind) {
            continue;
        }
        callables
            .entry(e.file.clone())
            .or_default()
            .insert(e.name.clone(), e.file.clone());
    }

    let mut additions: Vec<Reference> = Vec::new();
    for r in refs {
        if r.ref_kind != "call" {
            continue;
        }
        if r.confidence != Some(0.8) {
            continue;
        }
        if !r.file.ends_with(".rs") {
            continue;
        }
        // Tier-2 Rust form: `<crate>::<path>/<rest>` (or `<crate>::<func>/`
        // for a bare-name call after a `use` import).
        let Some(slash_at) = r.name.rfind('/') else { continue };
        let lhs = &r.name[..slash_at];
        let rest = &r.name[slash_at + 1..];
        let Some(cc_at) = lhs.find("::") else { continue };
        let crate_name = &lhs[..cc_at];
        let trailing = &lhs[cc_at + 2..];
        let func: &str = if rest.is_empty() {
            // `<crate>::<func>/` — func is the last `::` segment.
            match trailing.rfind("::") {
                Some(pos) => &trailing[pos + 2..],
                None => trailing,
            }
        } else {
            rest
        };
        if func.is_empty() || func.contains('/') || func.contains(':') {
            continue;
        }
        let Some(crate_dir) = cargo.crates.get(crate_name) else { continue };
        // Scan all callables under the crate's directory. The intermediate
        // path between crate name and func is advisory only — Rust's
        // module tree makes strict enforcement noisy without a full
        // mod-graph walk.
        let mut hits: Vec<String> = Vec::new();
        for (file, names) in callables.iter() {
            if !file.starts_with(crate_dir.as_str()) {
                continue;
            }
            if names.contains_key(func) {
                hits.push(file.clone());
            }
        }
        if hits.len() != 1 {
            continue;
        }
        let target = hits[0].clone();
        additions.push(Reference {
            file: r.file.clone(),
            caller: r.caller.clone(),
            name: format!("{target}/{func}"),
            ref_kind: "call".to_string(),
            line: r.line,
            confidence: Some(0.7),
            callee_id: Some(format!("{target}::{func}")),
        });
    }
    additions
}

/// Repo-scoped .NET project index. Mirrors repowise's `dotnet/` package:
/// every `.csproj` is parsed for its references and implicit-usings flag,
/// every `.sln` is regex-scanned to surface orphan csprojs, every `.cs`
/// file is regex-scanned for its `namespace` declarations and `global
/// using` directives, and per-project global+implicit usings are merged.
///
/// Used by `resolve_csharp_usings` to disambiguate `Class.Method` calls
/// where multiple namespaces define the same class name — the caller's
/// `using` list (plus its project's globals) picks the right namespace.
#[derive(Debug, Clone, Default)]
pub struct DotNetIndex {
    /// fully-qualified namespace → list of repo-relative .cs files declaring it.
    namespace_map: std::collections::HashMap<String, Vec<String>>,
    /// repo-relative .cs file → enclosing csproj's repo-relative path.
    file_to_project: std::collections::HashMap<String, String>,
    /// repo-relative csproj path → set of referenced csproj paths.
    project_refs: std::collections::HashMap<String, std::collections::HashSet<String>>,
    /// repo-relative csproj path → set of NuGet package ids.
    package_refs: std::collections::HashMap<String, std::collections::HashSet<String>>,
    /// repo-relative csproj path → set of implicit+global+`<Using/>` namespaces.
    project_globals: std::collections::HashMap<String, std::collections::HashSet<String>>,
}

/// Build the .NET index. Tolerates a repo without `.csproj`/`.sln` —
/// returns an index with just the namespace map (sufficient for the
/// loose-cs-files case).
pub fn load_dotnet_index(root: &Path) -> DotNetIndex {
    use std::collections::HashSet;

    // Default ImplicitUsings set (dotnet/sdk repo,
    // Microsoft.NET.Sdk.ImplicitNamespaceImports.props).
    const DEFAULT_IMPLICIT_USINGS: &[&str] = &[
        "System",
        "System.Collections.Generic",
        "System.IO",
        "System.Linq",
        "System.Net.Http",
        "System.Threading",
        "System.Threading.Tasks",
    ];
    const WEB_IMPLICIT_USINGS: &[&str] = &[
        "System.Net.Http.Json",
        "Microsoft.AspNetCore.Builder",
        "Microsoft.AspNetCore.Hosting",
        "Microsoft.AspNetCore.Http",
        "Microsoft.AspNetCore.Routing",
        "Microsoft.Extensions.Configuration",
        "Microsoft.Extensions.DependencyInjection",
        "Microsoft.Extensions.Hosting",
        "Microsoft.Extensions.Logging",
    ];

    fn rel_posix(root: &Path, p: &Path) -> Option<String> {
        p.strip_prefix(root)
            .ok()
            .map(|x| x.to_string_lossy().replace('\\', "/"))
    }

    // Walk repo for .csproj, .sln, .cs (skip bin/obj/.vs/packages/etc.).
    fn walk(
        dir: &Path,
        root: &Path,
        depth: usize,
        cs: &mut Vec<std::path::PathBuf>,
        csproj: &mut Vec<std::path::PathBuf>,
        sln: &mut Vec<std::path::PathBuf>,
    ) {
        if depth > 12 {
            return;
        }
        let Ok(read) = std::fs::read_dir(dir) else { return };
        for entry in read.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name_lossy = name.to_string_lossy();
            if path.is_dir() {
                if matches!(
                    name_lossy.as_ref(),
                    "bin" | "obj" | ".vs" | "node_modules" | ".git" | ".sigil" | "packages" | "TestResults"
                ) || name_lossy.starts_with('.')
                {
                    continue;
                }
                walk(&path, root, depth + 1, cs, csproj, sln);
            } else if name_lossy.ends_with(".cs") {
                cs.push(path);
            } else if name_lossy.ends_with(".csproj") {
                csproj.push(path);
            } else if name_lossy.ends_with(".sln") {
                sln.push(path);
            }
        }
    }
    let mut cs_files = Vec::new();
    let mut csproj_files = Vec::new();
    let mut sln_files = Vec::new();
    walk(root, root, 0, &mut cs_files, &mut csproj_files, &mut sln_files);

    let mut idx = DotNetIndex::default();

    // ---- 1. Parse each .csproj ----
    // Tracks project_dir → csproj relative path for the .cs → project step.
    let mut project_dirs: Vec<(String, std::path::PathBuf)> = Vec::new();
    for path in &csproj_files {
        let Some(rel) = rel_posix(root, path) else { continue };
        let Ok(content) = std::fs::read_to_string(path) else { continue };
        let (refs, pkgs, usings, implicit) = parse_csproj_xml(&content, path);
        idx.project_refs.insert(rel.clone(), refs);
        idx.package_refs.insert(rel.clone(), pkgs);
        // Implicit / project usings: compute the union here so the resolver
        // doesn't have to re-derive per call. Web SDK heuristic: any
        // AspNetCore package reference flags the expanded implicit set.
        let mut globals: HashSet<String> = HashSet::new();
        if implicit {
            for ns in DEFAULT_IMPLICIT_USINGS {
                globals.insert((*ns).to_string());
            }
            if idx.package_refs[&rel]
                .iter()
                .any(|p| p.starts_with("Microsoft.AspNetCore"))
            {
                for ns in WEB_IMPLICIT_USINGS {
                    globals.insert((*ns).to_string());
                }
            }
        }
        for u in usings {
            globals.insert(u);
        }
        idx.project_globals.insert(rel.clone(), globals);
        if let Some(parent) = path.parent() {
            project_dirs.push((rel, parent.to_path_buf()));
        }
    }

    // ---- 2. Walk .sln files to catch orphan csprojs ----
    // (Minimal: just surface csproj refs we missed. Full project parse for
    // orphans is a small extension if needed.)
    for path in &sln_files {
        let Ok(content) = std::fs::read_to_string(path) else { continue };
        let sln_dir = path.parent().unwrap_or(root);
        for entry_path in parse_sln_csproj_paths(&content, sln_dir) {
            let Some(rel) = rel_posix(root, &entry_path) else { continue };
            if !idx.project_refs.contains_key(&rel) {
                let Ok(content) = std::fs::read_to_string(&entry_path) else { continue };
                let (refs, pkgs, usings, implicit) = parse_csproj_xml(&content, &entry_path);
                idx.project_refs.insert(rel.clone(), refs);
                idx.package_refs.insert(rel.clone(), pkgs);
                let mut globals: HashSet<String> = HashSet::new();
                if implicit {
                    for ns in DEFAULT_IMPLICIT_USINGS {
                        globals.insert((*ns).to_string());
                    }
                }
                for u in usings {
                    globals.insert(u);
                }
                idx.project_globals.insert(rel.clone(), globals);
                if let Some(parent) = entry_path.parent() {
                    project_dirs.push((rel, parent.to_path_buf()));
                }
            }
        }
    }

    // ---- 3. Build namespace map + file_to_project + global-using scan ----
    // Longest project-dir prefix wins for file → project assignment.
    project_dirs.sort_by_key(|(_, p)| std::cmp::Reverse(p.to_string_lossy().len()));
    for cs_path in &cs_files {
        let Some(cs_rel) = rel_posix(root, cs_path) else { continue };
        let Ok(text) = std::fs::read_to_string(cs_path) else { continue };

        for ns in scan_csharp_namespaces(&text) {
            idx.namespace_map.entry(ns).or_default().push(cs_rel.clone());
        }
        // file → enclosing project (longest matching csproj dir).
        for (proj_rel, proj_dir) in &project_dirs {
            if cs_path.starts_with(proj_dir) {
                idx.file_to_project.insert(cs_rel.clone(), proj_rel.clone());
                break;
            }
        }
        // global using directives — fold into the file's project globals.
        let globals = scan_csharp_global_usings(&text);
        if !globals.is_empty() {
            if let Some(proj_rel) = idx.file_to_project.get(&cs_rel).cloned() {
                let set = idx.project_globals.entry(proj_rel).or_default();
                for g in globals {
                    set.insert(g);
                }
            }
        }
    }

    idx
}

/// Parse a `.csproj` (or `Directory.Build.props`) for the fields we need.
/// Returns (project_references_abs, package_refs, project_usings,
/// implicit_usings_enabled). Tolerates SDK-style and legacy XML by
/// stripping the XML namespace prefix from element local-names.
fn parse_csproj_xml(
    content: &str,
    csproj_path: &Path,
) -> (
    std::collections::HashSet<String>,
    std::collections::HashSet<String>,
    std::collections::HashSet<String>,
    bool,
) {
    use std::collections::HashSet;
    let mut project_refs: HashSet<String> = HashSet::new();
    let mut package_refs: HashSet<String> = HashSet::new();
    let mut project_usings: HashSet<String> = HashSet::new();
    let mut implicit_usings = false;
    let csproj_dir = csproj_path.parent();

    // Hand-rolled regex-style scan (no XML parser dep). We just need
    // element local-names + their `Include=` attribute / inner text.
    // Strips XML namespace prefix automatically (we never look at it).
    fn find_attr(tag: &str, attr: &str) -> Option<String> {
        // Returns the value of `attr="..."` inside the tag text.
        let needle = format!("{attr}=\"");
        let start = tag.find(&needle)? + needle.len();
        let end = tag[start..].find('"')? + start;
        Some(tag[start..end].to_string())
    }

    // Iterate over all `<TagName ...>` matches; for self-closing tags we
    // only need the attributes; for `<ImplicitUsings>true</ImplicitUsings>`
    // we read the inner text.
    let bytes = content.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        // Find the closing `>`.
        let Some(end) = content[i..].find('>') else { break };
        let tag_text = &content[i + 1..i + end]; // without the `<` and `>`.
        i += end + 1;
        if tag_text.starts_with('/') || tag_text.starts_with('?') || tag_text.starts_with('!') {
            continue;
        }
        // Extract local-name (strip namespace prefix and any attribute tail).
        let local: String = tag_text
            .split_whitespace()
            .next()
            .unwrap_or("")
            .split(':')
            .next_back()
            .unwrap_or("")
            .trim_end_matches('/')
            .to_string();

        match local.as_str() {
            "ImplicitUsings" => {
                // Read inner text up to closing tag.
                if let Some(close) = content[i..].find("</") {
                    let inner = content[i..i + close].trim().to_lowercase();
                    if matches!(inner.as_str(), "true" | "enable" | "1") {
                        implicit_usings = true;
                    }
                }
            }
            "ProjectReference" => {
                if let Some(include) = find_attr(tag_text, "Include") {
                    let rel = include.replace('\\', "/");
                    if let Some(dir) = csproj_dir {
                        let resolved = dir.join(&rel);
                        let posix = normalize_path(&resolved.to_string_lossy().replace('\\', "/"));
                        project_refs.insert(posix);
                    }
                }
            }
            "PackageReference" => {
                if let Some(pkg) = find_attr(tag_text, "Include") {
                    package_refs.insert(pkg);
                }
            }
            "Using" => {
                if let Some(ns) = find_attr(tag_text, "Include") {
                    project_usings.insert(ns);
                }
            }
            _ => {}
        }
    }

    (project_refs, package_refs, project_usings, implicit_usings)
}

/// Parse a `.sln` line-oriented format for the absolute csproj paths it
/// declares. Skips solution-folder type GUID `2150E333-...-46DE8`.
fn parse_sln_csproj_paths(content: &str, sln_dir: &Path) -> Vec<std::path::PathBuf> {
    const FOLDER_GUID: &str = "2150E333-8FDC-42A3-9474-1A3956D46DE8";
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if !line.starts_with("Project(") {
            continue;
        }
        // Project("{TYPE}") = "Name", "rel\path.csproj", "{GUID}"
        let Some(type_start) = line.find("(\"{") else { continue };
        let Some(type_end) = line[type_start + 3..].find("}\"") else { continue };
        let type_guid = &line[type_start + 3..type_start + 3 + type_end];
        if type_guid.eq_ignore_ascii_case(FOLDER_GUID) {
            continue;
        }
        // Find the second quoted string — the relative path.
        let mut quotes: Vec<usize> = Vec::new();
        for (idx, ch) in line.char_indices() {
            if ch == '"' {
                quotes.push(idx);
            }
        }
        if quotes.len() < 6 {
            continue;
        }
        // Quote layout: [0..1]=type GUID, [2..3]=Name, [4..5]=relative
        // csproj path. We want the path.
        let rel = &line[quotes[4] + 1..quotes[5]];
        if !rel.to_ascii_lowercase().ends_with(".csproj") {
            continue;
        }
        let rel_normalised = rel.replace('\\', "/");
        out.push(sln_dir.join(rel_normalised));
    }
    out
}

/// Extract every namespace declaration from a .cs file. Covers both
/// block-form `namespace Foo {` and file-scoped C# 10+ `namespace Foo;`.
fn scan_csharp_namespaces(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("namespace ") else { continue };
        let mut name_end = 0;
        for (i, ch) in rest.char_indices() {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' {
                name_end = i + ch.len_utf8();
            } else {
                break;
            }
        }
        if name_end == 0 {
            continue;
        }
        let name = &rest[..name_end];
        let tail = rest[name_end..].trim_start();
        if tail.starts_with('{') || tail.starts_with(';') {
            out.push(name.to_string());
        }
    }
    out
}

/// Extract every `global using <Foo.Bar>;` namespace target.
fn scan_csharp_global_usings(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let rest = match trimmed
            .strip_prefix("global using static ")
            .or_else(|| trimmed.strip_prefix("global using "))
        {
            Some(r) => r,
            None => continue,
        };
        // Optional `Alias = ` prefix.
        let payload = if let Some(eq) = rest.find('=') {
            rest[eq + 1..].trim_start()
        } else {
            rest
        };
        let mut end = 0;
        for (i, ch) in payload.char_indices() {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' {
                end = i + ch.len_utf8();
            } else {
                break;
            }
        }
        if end > 0 {
            out.push(payload[..end].to_string());
        }
    }
    out
}

/// Tier-3 file-resolution pass for C# `Class.Method` calls. For each .cs
/// call ref of form `<Class>.<Member>` (the C# parser's tier-2 form), find
/// candidate classes via the namespace map, intersected with the caller's
/// `using` directives + their project's implicit + global usings. Rank:
/// same-project → referenced-project → anywhere. Emits a 0.7 edge naming
/// the chosen file.
fn resolve_csharp_usings(
    entities: &[Entity],
    refs: &[Reference],
    idx: &DotNetIndex,
) -> Vec<Reference> {
    use std::collections::{HashMap, HashSet};
    if idx.namespace_map.is_empty() {
        return Vec::new();
    }

    // (file, class_name) → set of method names defined on that class.
    let mut class_methods: HashMap<(&str, &str), HashSet<&str>> = HashMap::new();
    // file → set of class names defined locally.
    let mut classes_in_file: HashMap<&str, HashSet<&str>> = HashMap::new();
    for e in entities {
        if !e.file.ends_with(".cs") {
            continue;
        }
        if e.kind == "class" {
            classes_in_file.entry(e.file.as_str()).or_default().insert(e.name.as_str());
        }
        if e.kind == "method" {
            if let Some(parent) = e.parent.as_deref() {
                // Method leaf: strip `Parent.` or `Parent::` prefix.
                let leaf = e
                    .name
                    .strip_prefix(&format!("{parent}."))
                    .or_else(|| e.name.strip_prefix(&format!("{parent}::")))
                    .unwrap_or(e.name.as_str());
                // Also strip any namespace prefix from parent
                // (parent might be `ProjA.Foo.Helper` — extract `Helper`).
                let class_leaf = match parent.rsplit_once('.') {
                    Some((_, last)) => last,
                    None => parent,
                };
                class_methods
                    .entry((e.file.as_str(), class_leaf))
                    .or_default()
                    .insert(leaf);
            }
        }
    }
    // file → list of using namespaces (sigil's C# parser emits import
    // entities for `using X.Y;` with name = "X.Y").
    let mut usings_by_file: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in entities {
        if e.kind != "import" {
            continue;
        }
        if !e.file.ends_with(".cs") {
            continue;
        }
        usings_by_file
            .entry(e.file.as_str())
            .or_default()
            .push(e.name.as_str());
    }

    let mut additions: Vec<Reference> = Vec::new();
    for r in refs {
        if r.ref_kind != "call" {
            continue;
        }
        if !r.file.ends_with(".cs") {
            continue;
        }
        if matches!(r.confidence, Some(c) if c >= 0.95) {
            continue;
        }
        // `Class.Method` split (one-level chain).
        let Some((class_name, method_leaf)) = r.name.rsplit_once('.') else { continue };
        if class_name.contains('.') || class_name.is_empty() || method_leaf.is_empty() {
            continue;
        }

        // Caller's namespaces to consult: file's `using` directives +
        // its project's implicit + global usings.
        let mut consult: Vec<&str> = Vec::new();
        if let Some(usings) = usings_by_file.get(r.file.as_str()) {
            consult.extend(usings.iter().copied());
        }
        let caller_project = idx.file_to_project.get(r.file.as_str());
        if let Some(proj) = caller_project {
            if let Some(globals) = idx.project_globals.get(proj) {
                consult.extend(globals.iter().map(String::as_str));
            }
        }
        if consult.is_empty() {
            continue;
        }

        // For each namespace, find files declaring it that contain
        // (class_name, method_leaf).
        let mut hits: Vec<String> = Vec::new();
        for ns in &consult {
            let Some(files) = idx.namespace_map.get(*ns) else { continue };
            for file in files {
                let f: &str = file.as_str();
                let has_class = classes_in_file
                    .get(f)
                    .map(|s| s.contains(class_name))
                    .unwrap_or(false);
                let has_method = class_methods
                    .get(&(f, class_name))
                    .map(|s| s.contains(method_leaf))
                    .unwrap_or(false);
                if has_class && has_method {
                    hits.push(file.clone());
                }
            }
        }
        hits.sort();
        hits.dedup();
        if hits.is_empty() {
            continue;
        }

        // Rank: same-project → referenced-project → anywhere.
        let chosen: String = if let Some(caller_proj) = caller_project {
            let same: Vec<&String> = hits
                .iter()
                .filter(|f| idx.file_to_project.get(*f) == Some(caller_proj))
                .collect();
            if !same.is_empty() {
                same[0].clone()
            } else {
                let refs_set = idx.project_refs.get(caller_proj);
                let referenced: Vec<&String> = hits
                    .iter()
                    .filter(|f| {
                        if let Some(rs) = refs_set {
                            idx.file_to_project
                                .get(*f)
                                .map(|p| rs.contains(p))
                                .unwrap_or(false)
                        } else {
                            false
                        }
                    })
                    .collect();
                if !referenced.is_empty() {
                    referenced[0].clone()
                } else {
                    hits[0].clone()
                }
            }
        } else {
            // No project info → just take the first hit (or skip when
            // multiple are equally plausible).
            if hits.len() > 1 {
                continue;
            }
            hits[0].clone()
        };

        additions.push(Reference {
            file: r.file.clone(),
            caller: r.caller.clone(),
            name: format!("{chosen}/{method_leaf}"),
            ref_kind: "call".to_string(),
            line: r.line,
            confidence: Some(0.7),
            callee_id: Some(format!("{chosen}::{class_name}::{method_leaf}")),
        });
    }
    additions
}

/// Parsed `compile_commands.json` — per-source-file include directories
/// extracted from `-I`, `-isystem`, `-iquote` flags in either the
/// `arguments` array or the `command` string. Paths are repo-relative.
/// Mirrors repowise's `context.py:load_compile_commands` +
/// `extract_include_dirs`.
#[derive(Debug, Clone, Default)]
pub struct CompileCommands {
    /// source_file (repo-relative posix) → ordered list of include dirs
    /// (repo-relative posix) for that translation unit.
    per_file: std::collections::HashMap<String, Vec<String>>,
}

/// Load `compile_commands.json` from the index root or `build/`. Returns
/// an empty map when absent or unparseable.
pub fn load_compile_commands(root: &Path) -> CompileCommands {
    for candidate in &[
        root.join("compile_commands.json"),
        root.join("build").join("compile_commands.json"),
    ] {
        let Ok(text) = std::fs::read_to_string(candidate) else { continue };
        let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&text) else { continue };
        let mut per_file: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for entry in arr {
            let Some(file) = entry.get("file").and_then(|v| v.as_str()) else { continue };
            let cmd_dir = entry
                .get("directory")
                .and_then(|v| v.as_str())
                .unwrap_or(".");
            // Tokenize: prefer `arguments` array; else split `command` on whitespace.
            let tokens: Vec<String> = if let Some(args) = entry.get("arguments").and_then(|v| v.as_array()) {
                args.iter().filter_map(|v| v.as_str().map(String::from)).collect()
            } else if let Some(cmd) = entry.get("command").and_then(|v| v.as_str()) {
                cmd.split_whitespace().map(String::from).collect()
            } else {
                Vec::new()
            };
            // Extract include dirs from -I / -isystem / -iquote.
            let mut includes: Vec<String> = Vec::new();
            let mut i = 0;
            while i < tokens.len() {
                let tok = tokens[i].as_str();
                let (dir, advance) = if tok == "-I" || tok == "-isystem" || tok == "-iquote" {
                    if i + 1 < tokens.len() { (Some(tokens[i + 1].clone()), 2) } else { (None, 1) }
                } else if let Some(rest) = tok.strip_prefix("-I") {
                    (Some(rest.to_string()), 1)
                } else if let Some(rest) = tok.strip_prefix("-isystem") {
                    (Some(rest.to_string()), 1)
                } else {
                    (None, 1)
                };
                if let Some(d) = dir {
                    // Resolve relative to cmd_dir, then make root-relative.
                    let absolute = if std::path::Path::new(&d).is_absolute() {
                        std::path::PathBuf::from(&d)
                    } else {
                        std::path::PathBuf::from(cmd_dir).join(&d)
                    };
                    // Canonicalise lazily — we only need posix-relative paths
                    // resolvable against the index root.
                    let posix = absolute.to_string_lossy().replace('\\', "/");
                    let normalized = normalize_path(&posix);
                    let rel = normalized
                        .strip_prefix("./")
                        .unwrap_or(&normalized)
                        .to_string();
                    includes.push(rel);
                }
                i += advance;
            }
            // Normalize the file key — make it repo-relative posix.
            let file_key = std::path::Path::new(file)
                .to_string_lossy()
                .replace('\\', "/");
            let file_key = file_key.strip_prefix("./").unwrap_or(&file_key).to_string();
            per_file.insert(file_key, includes);
        }
        if !per_file.is_empty() {
            return CompileCommands { per_file };
        }
    }
    CompileCommands::default()
}

/// Tier-3 file-resolution pass for C/C++ `#include` directives. For each
/// `#include "foo.h"` from a `.c`/`.cpp`/`.cc`/`.cxx`/etc. source file,
/// resolve to a real file path using:
///   1. compile_commands.json `-I/-isystem/-iquote` dirs for that file,
///   2. relative to the importer's directory,
/// and emit a 0.7 edge `<file>/<func>` for every call ref in the importer
/// whose callee is defined in the resolved header. Mirrors repowise's
/// `cpp.py` 3-step ladder (we omit the stem-match fallback — it
/// over-binds in practice when sources have header/impl pairs).
fn resolve_cpp_includes(
    entities: &[Entity],
    refs: &[Reference],
    cc: &CompileCommands,
) -> Vec<Reference> {
    use std::collections::{HashMap, HashSet};

    fn is_c_or_cpp_path(file: &str) -> bool {
        for ext in &[".c", ".cc", ".cpp", ".cxx", ".h", ".hh", ".hpp", ".hxx"] {
            if file.ends_with(ext) {
                return true;
            }
        }
        false
    }
    fn parent_dir(file: &str) -> &str {
        match file.rfind('/') {
            Some(i) => &file[..i],
            None => "",
        }
    }

    // file → set of locally-defined callable names.
    let mut callables: HashMap<&str, HashSet<&str>> = HashMap::new();
    // Set of every indexed file path (for existence checks).
    let mut file_set: HashSet<&str> = HashSet::new();
    for e in entities {
        file_set.insert(e.file.as_str());
        if !is_c_or_cpp_path(&e.file) {
            continue;
        }
        if !is_callable_kind(&e.kind) {
            continue;
        }
        callables
            .entry(e.file.as_str())
            .or_default()
            .insert(e.name.as_str());
    }
    // file → list of (raw_include_spec) — the bare `helper.h` form sigil
    // emits as the import entity name.
    let mut imports_by_file: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in entities {
        if e.kind != "import" {
            continue;
        }
        if !is_c_or_cpp_path(&e.file) {
            continue;
        }
        imports_by_file
            .entry(e.file.as_str())
            .or_default()
            .push(e.name.as_str());
    }

    let mut additions: Vec<Reference> = Vec::new();
    for r in refs {
        if r.ref_kind != "call" {
            continue;
        }
        if !is_c_or_cpp_path(&r.file) {
            continue;
        }
        if r.name.contains('.') || r.name.contains('/') || r.name.contains(':') {
            continue;
        }
        let Some(imports) = imports_by_file.get(r.file.as_str()) else { continue };

        // Resolve each include to a real file via compile_commands then
        // importer-relative probing. Keep the unique-resolution invariant:
        // we only emit when exactly one resolved header defines the name.
        let mut hit_files: HashSet<String> = HashSet::new();
        let include_dirs = cc.per_file.get(r.file.as_str()).cloned().unwrap_or_default();
        let importer_dir = parent_dir(&r.file).to_string();

        for spec in imports {
            // 1. compile_commands include dirs.
            for inc in &include_dirs {
                let candidate = if inc.is_empty() {
                    spec.to_string()
                } else {
                    format!("{inc}/{spec}")
                };
                let normalized = normalize_path(&candidate);
                if let Some(file) = file_set.get(normalized.as_str()) {
                    if let Some(names) = callables.get(file) {
                        if names.contains(r.name.as_str()) {
                            hit_files.insert((*file).to_string());
                        }
                    }
                }
            }
            // 2. Relative to importer's directory.
            let rel_candidate = if importer_dir.is_empty() {
                spec.to_string()
            } else {
                format!("{importer_dir}/{spec}")
            };
            let normalized = normalize_path(&rel_candidate);
            if let Some(file) = file_set.get(normalized.as_str()) {
                if let Some(names) = callables.get(file) {
                    if names.contains(r.name.as_str()) {
                        hit_files.insert((*file).to_string());
                    }
                }
            }
        }

        if hit_files.len() != 1 {
            continue;
        }
        let file = hit_files.into_iter().next().unwrap();
        let callee_name = r.name.clone();
        additions.push(Reference {
            file: r.file.clone(),
            caller: r.caller.clone(),
            name: format!("{file}/{callee_name}"),
            ref_kind: "call".to_string(),
            line: r.line,
            confidence: Some(0.7),
            callee_id: Some(format!("{file}::{callee_name}")),
        });
    }
    additions
}

/// Tier-3 file-resolution pass for JVM-family FQN imports (Kotlin, Scala).
/// Both languages put source under `<root>/<pkg-as-dirs>/<File>.<ext>` in
/// standard build-tool layouts (Gradle `src/main/kotlin`, sbt
/// `src/main/scala`). FQ imports encode the disambiguation: scan files
/// under `*/<pkg>/` for the callable.
///
/// Kotlin imports come in two shapes:
///   * `com.example.helper` — top-level function/property.
///   * `com.example.MyClass.method` — class member.
///
/// Scala imports always carry the class as the second-to-last segment
/// (`com.example.Helper.helper` for `object Helper { def helper }`).
///
/// We treat both shapes uniformly: the *last* segment is the call leaf;
/// everything before is the package-as-directories. For Scala this means
/// scanning `*/com/example/Helper/` (wrong) — so we additionally try
/// stripping the last two segments when the last segment doesn't match
/// the call leaf. Path-based rather than settings.gradle/build.sbt-based;
/// non-standard `srcDirs(...)` layouts are a follow-up.
fn resolve_jvm_fqn_imports(entities: &[Entity], refs: &[Reference]) -> Vec<Reference> {
    use std::collections::{HashMap, HashSet};

    fn is_jvm_path(file: &str) -> bool {
        file.ends_with(".kt") || file.ends_with(".kts") || file.ends_with(".scala")
    }

    // file → set of callable names defined locally (incl. methods stored
    // under their `Class.method` qualified form).
    let mut callables: HashMap<&str, HashSet<String>> = HashMap::new();
    for e in entities {
        if !is_jvm_path(&e.file) {
            continue;
        }
        if !is_callable_kind(&e.kind) && e.kind != "class" {
            continue;
        }
        let set = callables.entry(e.file.as_str()).or_default();
        set.insert(e.name.clone());
        // For methods stored as `Class.method`, also index the bare leaf
        // — Scala/Kotlin tier-2 emits the call as the bare leaf name.
        if let Some(parent) = e.parent.as_deref() {
            let leaf = e
                .name
                .strip_prefix(&format!("{parent}."))
                .or_else(|| e.name.strip_prefix(&format!("{parent}::")))
                .unwrap_or(e.name.as_str());
            set.insert(leaf.to_string());
        }
    }
    // file → list of FQN imports.
    let mut imports_by_file: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in entities {
        if e.kind != "import" {
            continue;
        }
        if !is_jvm_path(&e.file) {
            continue;
        }
        imports_by_file
            .entry(e.file.as_str())
            .or_default()
            .push(e.name.as_str());
    }

    let mut additions: Vec<Reference> = Vec::new();
    for r in refs {
        if r.ref_kind != "call" {
            continue;
        }
        if !is_jvm_path(&r.file) {
            continue;
        }
        if r.name.contains('.') || r.name.contains('/') || r.name.contains(':') {
            continue;
        }
        let Some(imports) = imports_by_file.get(r.file.as_str()) else { continue };
        let mut hit_files: HashSet<&str> = HashSet::new();
        for import in imports {
            // The import's last segment is the leaf — must match the
            // call name. Star imports skipped.
            let Some(dot_at) = import.rfind('.') else { continue };
            let leaf = &import[dot_at + 1..];
            if leaf == "*" || leaf != r.name {
                continue;
            }
            // Try the package-as-dirs form. Two interpretations:
            //   (a) everything before the leaf  (Kotlin top-level)
            //   (b) everything before the parent-class.leaf (Scala member,
            //       where the second-to-last segment is the Object/Class)
            // Try (a) first; if it yields no matches, fall back to (b).
            let needle_a = format!("/{}/", import[..dot_at].replace('.', "/"));
            let mut local_hits: HashSet<&str> = HashSet::new();
            for (file, names) in callables.iter() {
                if file.contains(needle_a.as_str()) && names.contains(r.name.as_str()) {
                    local_hits.insert(*file);
                }
            }
            if local_hits.is_empty() {
                if let Some(second_dot) = import[..dot_at].rfind('.') {
                    let needle_b = format!("/{}/", import[..second_dot].replace('.', "/"));
                    for (file, names) in callables.iter() {
                        if file.contains(needle_b.as_str()) && names.contains(r.name.as_str()) {
                            local_hits.insert(*file);
                        }
                    }
                }
            }
            hit_files.extend(local_hits);
        }
        if hit_files.len() != 1 {
            continue;
        }
        let file = hit_files.into_iter().next().unwrap();
        let callee_name = r.name.clone();
        additions.push(Reference {
            file: r.file.clone(),
            caller: r.caller.clone(),
            name: format!("{file}/{callee_name}"),
            ref_kind: "call".to_string(),
            line: r.line,
            confidence: Some(0.7),
            callee_id: Some(format!("{file}::{callee_name}")),
        });
    }
    additions
}

/// Parsed Swift Package Manager target map. Each `Package.swift` declares
/// `.target(name:)` / `.executableTarget(name:)` / `.testTarget(name:)`
/// entries. Without an explicit `path:`, sources live under
/// `Sources/<TargetName>/` (or `Tests/<TargetName>/` for testTargets).
/// Mirrors repowise's `swift_spm.py` regex approach — no Swift parser
/// dependency needed.
#[derive(Debug, Clone, Default)]
pub struct SwiftSpm {
    /// target_name → directory (relative to the index root).
    targets: std::collections::HashMap<String, String>,
}

/// Walk the index root for `Package.swift` files, regex-extract their
/// targets, and merge into one map. The directory each target sits under
/// is the Package.swift's parent dir joined with the target's `path:`
/// (or the `Sources/<name>` / `Tests/<name>` default).
pub fn load_swift_spm(root: &Path) -> SwiftSpm {
    fn walk(dir: &Path, root: &Path, depth: usize, out: &mut std::collections::HashMap<String, String>) {
        if depth > 4 {
            return;
        }
        let Ok(read) = std::fs::read_dir(dir) else { return };
        for entry in read.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name_lossy = name.to_string_lossy();
            if path.is_dir() {
                if name_lossy.starts_with('.')
                    || name_lossy == "node_modules"
                    || name_lossy == ".sigil"
                    || name_lossy == ".build"
                {
                    continue;
                }
                walk(&path, root, depth + 1, out);
            } else if name_lossy == "Package.swift" {
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                let pkg_dir = path
                    .parent()
                    .and_then(|p| p.strip_prefix(root).ok())
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                for (target_name, target_path) in parse_package_swift(&text) {
                    let full = if pkg_dir.is_empty() {
                        target_path
                    } else {
                        format!("{pkg_dir}/{target_path}")
                    };
                    out.entry(target_name).or_insert(full);
                }
            }
        }
    }
    let mut targets = std::collections::HashMap::new();
    walk(root, root, 0, &mut targets);
    SwiftSpm { targets }
}

/// Extract `(target_name, source_dir)` pairs from Package.swift text via
/// regex. Recognises `.target`, `.executableTarget`, `.testTarget`,
/// `.systemLibrary`, `.binaryTarget`, `.plugin`. `path:`-less targets
/// default to `Sources/<name>` (test targets default to `Tests/<name>`).
fn parse_package_swift(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    // Iterate over each `.<kind>(` occurrence and capture the matched
    // body up to the matching `)`. Simple paren-balance, no full Swift
    // parse — covers the 95% case repowise targets.
    let kinds = [
        "target",
        "executableTarget",
        "testTarget",
        "systemLibrary",
        "binaryTarget",
        "plugin",
    ];
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '.' {
            i += 1;
            continue;
        }
        let mut matched: Option<&str> = None;
        for kind in kinds {
            let end = i + 1 + kind.len();
            if end <= chars.len()
                && chars[i + 1..end].iter().collect::<String>() == kind
                && end < chars.len()
                && (chars[end] == '(' || chars[end].is_whitespace())
            {
                matched = Some(kind);
                break;
            }
        }
        let Some(kind) = matched else {
            i += 1;
            continue;
        };
        // Find the opening `(`.
        let mut p = i + 1 + kind.len();
        while p < chars.len() && chars[p].is_whitespace() {
            p += 1;
        }
        if p >= chars.len() || chars[p] != '(' {
            i += 1;
            continue;
        }
        // Capture body to matching `)`, respecting one level of nesting
        // (e.g. dependencies: [.product(...)]).
        let body_start = p + 1;
        let mut depth = 1;
        let mut q = body_start;
        while q < chars.len() && depth > 0 {
            match chars[q] {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            q += 1;
        }
        if depth != 0 {
            break;
        }
        let body: String = chars[body_start..q - 1].iter().collect();
        i = q;
        // Extract `name: "..."` and optional `path: "..."` from body.
        let Some(name) = scan_swift_string_arg(&body, "name") else { continue };
        let path = scan_swift_string_arg(&body, "path");
        let target_dir = match path {
            Some(p) => p.trim_start_matches("./").to_string(),
            None => {
                let base = if kind == "testTarget" { "Tests" } else { "Sources" };
                format!("{base}/{name}")
            }
        };
        out.push((name, target_dir));
    }
    out
}

/// Find `<key>:\s*"..."` inside a Swift call body. Returns the unquoted
/// string. Returns None if the key isn't present.
fn scan_swift_string_arg(body: &str, key: &str) -> Option<String> {
    let mut idx = 0;
    while idx < body.len() {
        let rest = &body[idx..];
        let Some(found) = rest.find(key) else { return None };
        let after = idx + found + key.len();
        // Must be immediately followed by optional whitespace + `:`.
        let mut p = after;
        let bytes = body.as_bytes();
        while p < bytes.len() && bytes[p].is_ascii_whitespace() {
            p += 1;
        }
        if p >= bytes.len() || bytes[p] != b':' {
            idx = after;
            continue;
        }
        p += 1;
        while p < bytes.len() && bytes[p].is_ascii_whitespace() {
            p += 1;
        }
        if p >= bytes.len() || bytes[p] != b'"' {
            idx = after;
            continue;
        }
        p += 1;
        let value_start = p;
        while p < bytes.len() && bytes[p] != b'"' {
            p += 1;
        }
        if p > bytes.len() {
            return None;
        }
        return Some(body[value_start..p].to_string());
    }
    None
}

/// Tier-3 file-resolution pass for Swift SPM. For each `.swift` call ref
/// whose name has no receiver (bare call) and whose caller's file imports
/// an SPM target, scan files under that target's source dir for a
/// callable matching the name. Emits a 0.7 file-resolved edge when
/// exactly one target's directory yields a hit.
fn resolve_swift_spm_imports(
    entities: &[Entity],
    refs: &[Reference],
    spm: &SwiftSpm,
) -> Vec<Reference> {
    use std::collections::{HashMap, HashSet};
    if spm.targets.is_empty() {
        return Vec::new();
    }
    // dir-prefix → (name → file_path) for every Swift callable.
    let mut by_dir: HashMap<String, HashMap<String, String>> = HashMap::new();
    for e in entities {
        if !e.file.ends_with(".swift") {
            continue;
        }
        if !is_callable_kind(&e.kind) {
            continue;
        }
        by_dir
            .entry(e.file.clone())
            .or_default()
            .insert(e.name.clone(), e.file.clone());
    }
    // file → list of imported target names.
    let mut imports_by_file: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in entities {
        if e.kind != "import" {
            continue;
        }
        if !e.file.ends_with(".swift") {
            continue;
        }
        imports_by_file
            .entry(e.file.as_str())
            .or_default()
            .push(e.name.as_str());
    }

    let mut additions: Vec<Reference> = Vec::new();
    for r in refs {
        if r.ref_kind != "call" {
            continue;
        }
        if !r.file.ends_with(".swift") {
            continue;
        }
        // Bare names only (member/scoped calls are owned elsewhere).
        if r.name.contains('.') || r.name.contains('/') || r.name.contains(':') {
            continue;
        }
        let Some(imports) = imports_by_file.get(r.file.as_str()) else { continue };
        let mut hit_files: HashSet<String> = HashSet::new();
        for module in imports {
            let Some(target_dir) = spm.targets.get(*module) else { continue };
            let target_prefix = format!("{}/", target_dir.trim_end_matches('/'));
            for (file, names) in by_dir.iter() {
                if !file.starts_with(target_prefix.as_str()) {
                    continue;
                }
                if names.contains_key(r.name.as_str()) {
                    hit_files.insert(file.clone());
                }
            }
        }
        if hit_files.len() != 1 {
            continue;
        }
        let file = hit_files.into_iter().next().unwrap();
        let callee_name = r.name.clone();
        additions.push(Reference {
            file: r.file.clone(),
            caller: r.caller.clone(),
            name: format!("{file}/{callee_name}"),
            ref_kind: "call".to_string(),
            line: r.line,
            confidence: Some(0.7),
            callee_id: Some(format!("{file}::{callee_name}")),
        });
    }
    additions
}

/// Convert a CamelCase identifier to Rails-convention snake_case
/// (`UserMailer` → `user_mailer`, `APIController` → `api_controller`).
/// Treats consecutive uppercase runs as a single acronym to match
/// ActiveSupport's `underscore` behavior.
fn camel_to_snake(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + 4);
    for (i, &ch) in chars.iter().enumerate() {
        if ch.is_ascii_uppercase() {
            let prev_lower = i > 0 && chars[i - 1].is_ascii_lowercase();
            let next_lower = i + 1 < chars.len() && chars[i + 1].is_ascii_lowercase();
            // Insert `_` between a lower→upper boundary, or before the
            // last upper of an acronym followed by a lowercase letter
            // (e.g. `APIController` → `api_controller`).
            if i > 0 && (prev_lower || next_lower) {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// Tier-3 file-resolution pass for Ruby on Rails autoload conventions.
/// For each `.rb` call ref of form `ClassName.method`, scan the index for
/// a file whose path matches the Rails convention
/// (`<dir>/<snake_case>.rb` for `dir` in `app/**`, `lib/**`) AND defines
/// `ClassName` with the called method. Emits a 0.7 file-resolved edge.
///
/// This pass is additive to Strategy 2 (P0.2): when global-unique already
/// fires (one class globally), Strategy 2 binds at 0.88; this pass layers
/// the file pointer. When global is ambiguous, the Rails convention
/// disambiguates and this pass is the only one that fires.
fn resolve_rails_autoload(entities: &[Entity], refs: &[Reference]) -> Vec<Reference> {
    use std::collections::HashMap;

    // class_name → list of (file, has_method_X)? Build a class→file map
    // first, then on the per-ref pass we filter by method presence.
    let mut classes_by_name: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut methods: std::collections::HashSet<(&str, &str, &str)> =
        std::collections::HashSet::new();
    for e in entities {
        if !e.file.ends_with(".rb") {
            continue;
        }
        if e.kind == "class" {
            classes_by_name.entry(e.name.as_str()).or_default().push(e.file.as_str());
        } else if e.kind == "method" {
            let Some(parent) = e.parent.as_deref() else { continue };
            let leaf = e
                .name
                .strip_prefix(&format!("{parent}."))
                .or_else(|| e.name.strip_prefix(&format!("{parent}::")))
                .unwrap_or(e.name.as_str());
            methods.insert((e.file.as_str(), parent, leaf));
        }
    }

    fn is_rails_path(file: &str) -> bool {
        file.starts_with("app/") || file.starts_with("lib/")
    }
    fn matches_rails_convention(file: &str, class_name: &str) -> bool {
        let snake = camel_to_snake(class_name);
        // The file's stem (filename without extension) must equal the
        // class's snake_case form. Subdirectory structure is loose —
        // Rails autoload only enforces the leaf.
        let stem = match file.rfind('/') {
            Some(i) => &file[i + 1..],
            None => file,
        };
        stem == format!("{snake}.rb")
    }

    let mut additions: Vec<Reference> = Vec::new();
    for r in refs {
        if r.ref_kind != "call" {
            continue;
        }
        if !r.file.ends_with(".rb") {
            continue;
        }
        let Some((head, leaf)) = r.name.rsplit_once('.') else { continue };
        if head.contains('.') || leaf.is_empty() {
            continue;
        }
        // Find Rails-convention files defining `head` with method `leaf`.
        let Some(class_files) = classes_by_name.get(head) else { continue };
        let candidates: Vec<&str> = class_files
            .iter()
            .copied()
            .filter(|f| is_rails_path(f))
            .filter(|f| matches_rails_convention(f, head))
            .filter(|f| methods.contains(&(*f, head, leaf)))
            .collect();
        // Rails default autoload privileges `app/` over `lib/` — when
        // both exist, `app/` wins. Within a tier the match must still
        // be unique.
        let app_hits: Vec<&str> = candidates.iter().copied().filter(|f| f.starts_with("app/")).collect();
        let hits: Vec<&str> = if !app_hits.is_empty() { app_hits } else { candidates };
        let mut hits = hits;
        hits.sort();
        hits.dedup();
        if hits.len() != 1 {
            continue;
        }
        let target = hits[0];
        additions.push(Reference {
            file: r.file.clone(),
            caller: r.caller.clone(),
            name: format!("{target}/{leaf}"),
            ref_kind: "call".to_string(),
            line: r.line,
            confidence: Some(0.7),
            callee_id: Some(format!("{target}::{head}::{leaf}")),
        });
    }
    additions
}

/// Resolve a textual module specifier (`./utils`, `../pkg/helpers`,
/// `pkg.helpers`) to a real file path that exists in the index. JS/TS and
/// Python only — other languages need manifest-aware resolvers.
///
/// Strategies tried (first hit wins):
///
///   * JS/TS relative (`./X`, `../X`): probe `X.ts`, `X.tsx`, `X.js`,
///     `X.jsx`, `X.mjs`, `X.cjs`, then `X/index.{ts,tsx,js,jsx,mjs,cjs}`
///   * Python dotted (`pkg.helpers`, `.pkg.helpers`): probe
///     `pkg/helpers.py`, `pkg/helpers/__init__.py` (and resolve leading
///     `.` relative to the importing file's directory)
fn resolve_module_path(
    from_file: &str,
    module: &str,
    files: &std::collections::HashSet<&str>,
    tsconfig: Option<&TsconfigPaths>,
) -> Option<String> {
    fn parent_dir(file: &str) -> &str {
        match file.rfind('/') {
            Some(i) => &file[..i],
            None => "",
        }
    }
    fn probe_ts_extensions(stem: &str, files: &std::collections::HashSet<&str>) -> Option<String> {
        for ext in &["ts", "tsx", "js", "jsx", "mjs", "cjs"] {
            let candidate = format!("{stem}.{ext}");
            if let Some(hit) = files.get(candidate.as_str()) {
                return Some((*hit).to_string());
            }
        }
        for ext in &["ts", "tsx", "js", "jsx", "mjs", "cjs"] {
            let candidate = format!("{stem}/index.{ext}");
            if let Some(hit) = files.get(candidate.as_str()) {
                return Some((*hit).to_string());
            }
        }
        None
    }
    let lang = language_from_file(from_file);

    if matches!(lang, Some("javascript") | Some("typescript")) {
        // tsconfig.json `paths` rewrite — checked first so aliased imports
        // like `@/utils` get a chance before relative-probing rejects them.
        // The rewrite yields a path relative to the index root; no further
        // `parent_dir` resolution needed.
        if let Some(ts) = tsconfig {
            if let Some(rewritten) = apply_tsconfig_paths(module, ts) {
                let normalized = normalize_path(&rewritten);
                if let Some(hit) = probe_ts_extensions(&normalized, files) {
                    return Some(hit);
                }
            }
        }
        if module.starts_with("./") || module.starts_with("../") {
            let base = parent_dir(from_file);
            let joined = if base.is_empty() {
                module.trim_start_matches("./").to_string()
            } else {
                format!("{}/{}", base, module)
            };
            let normalized = normalize_path(&joined);
            return probe_ts_extensions(&normalized, files);
        }
        return None;
    }

    if lang == Some("python") {
        // Strip leading dots for relative imports (`.pkg.helpers`).
        let (level, rest) = {
            let mut level = 0;
            for ch in module.chars() {
                if ch == '.' {
                    level += 1;
                } else {
                    break;
                }
            }
            (level, &module[level..])
        };
        let base = if level == 0 {
            String::new()
        } else {
            // Walk up `level-1` directories from the file's parent (level=1
            // means same dir, level=2 means one up, etc.).
            let mut dir = parent_dir(from_file).to_string();
            for _ in 1..level {
                if let Some(i) = dir.rfind('/') {
                    dir.truncate(i);
                } else {
                    dir.clear();
                }
            }
            dir
        };
        let dotted = rest.replace('.', "/");
        let path = if base.is_empty() {
            dotted
        } else if dotted.is_empty() {
            base
        } else {
            format!("{base}/{dotted}")
        };
        let candidate = format!("{path}.py");
        if let Some(hit) = files.get(candidate.as_str()) {
            return Some((*hit).to_string());
        }
        let candidate = format!("{path}/__init__.py");
        if let Some(hit) = files.get(candidate.as_str()) {
            return Some((*hit).to_string());
        }
    }
    None
}

/// Canonicalise a path string by collapsing `.` / `..` segments. Operates
/// on `/`-separated paths (the form sigil stores filenames in).
fn normalize_path(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out.join("/")
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
    fn camel_to_snake_rails_conventions() {
        assert_eq!(camel_to_snake("UserMailer"), "user_mailer");
        assert_eq!(camel_to_snake("User"), "user");
        assert_eq!(camel_to_snake("HTTPController"), "http_controller");
        assert_eq!(camel_to_snake("APIBase"), "api_base");
        assert_eq!(camel_to_snake("V2Api"), "v2_api");
        // Single-token leaf — no insertion needed.
        assert_eq!(camel_to_snake("Foo"), "foo");
    }

    #[test]
    fn scan_csharp_namespaces_handles_both_forms() {
        // Block-form
        assert_eq!(
            scan_csharp_namespaces("namespace Foo.Bar {\n}\n"),
            vec!["Foo.Bar".to_string()],
        );
        // File-scoped (C# 10+)
        assert_eq!(
            scan_csharp_namespaces("namespace Foo.Bar;\nclass X {}\n"),
            vec!["Foo.Bar".to_string()],
        );
        // Multiple namespaces in one file (rare but legal)
        let multi = "namespace A {\n}\nnamespace B.C {\n}\n";
        assert_eq!(
            scan_csharp_namespaces(multi),
            vec!["A".to_string(), "B.C".to_string()],
        );
        // Not a namespace declaration
        assert!(scan_csharp_namespaces("using System;").is_empty());
    }

    #[test]
    fn scan_csharp_global_usings_handles_alias_and_static() {
        assert_eq!(
            scan_csharp_global_usings("global using Foo.Bar;\n"),
            vec!["Foo.Bar".to_string()],
        );
        assert_eq!(
            scan_csharp_global_usings("global using static Foo.Bar;\n"),
            vec!["Foo.Bar".to_string()],
        );
        // Alias form
        assert_eq!(
            scan_csharp_global_usings("global using Alias = Foo.Bar;\n"),
            vec!["Foo.Bar".to_string()],
        );
        // Plain `using` (not global) is ignored.
        assert!(scan_csharp_global_usings("using Foo.Bar;\n").is_empty());
    }

    #[test]
    fn parse_sln_csproj_paths_skips_solution_folders() {
        // Solution-folder type GUID — must be skipped.
        let sln = r#"
            Project("{2150E333-8FDC-42A3-9474-1A3956D46DE8}") = "Folder", "Folder", "{00000000-0000-0000-0000-000000000001}"
            EndProject
            Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "App", "App\App.csproj", "{00000000-0000-0000-0000-000000000002}"
            EndProject
        "#;
        let paths = parse_sln_csproj_paths(sln, std::path::Path::new("/repo"));
        assert_eq!(paths.len(), 1);
        assert!(paths[0].to_string_lossy().ends_with("App/App.csproj"));
    }

    #[test]
    fn parse_package_swift_handles_common_target_shapes() {
        let text = r#"
            let package = Package(
                name: "MyLib",
                targets: [
                    .target(name: "Bare"),
                    .target(name: "WithPath", path: "Custom/Dir"),
                    .executableTarget(name: "App", dependencies: ["Bare"]),
                    .testTarget(name: "BareTests"),
                ]
            )
        "#;
        let pairs = parse_package_swift(text);
        let map: std::collections::HashMap<_, _> = pairs.into_iter().collect();
        assert_eq!(map.get("Bare").map(String::as_str), Some("Sources/Bare"));
        assert_eq!(map.get("WithPath").map(String::as_str), Some("Custom/Dir"));
        assert_eq!(map.get("App").map(String::as_str), Some("Sources/App"));
        assert_eq!(map.get("BareTests").map(String::as_str), Some("Tests/BareTests"));
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
    fn parse_single_file_kotlin_doc_attaches_to_next_decl() {
        // KDoc `/** ... */` immediately preceding a declaration should
        // attach as the decl's `doc`. The new-language extractors emit
        // doc-comment TextEntry rows with parent = enclosing scope, so
        // attachment relies on a proximity-based fallback in index.rs.
        let source = "/**\n * Greets the caller.\n */\nfun greet(name: String): String = \"hi $name\"\n";
        let (entities, _) = parse_single_file(source, "t.kt", "kotlin").unwrap();
        let g = entities.iter().find(|e| e.name == "greet").expect("greet");
        assert!(
            g.doc.as_deref().map(|d| d.contains("Greets the caller")).unwrap_or(false),
            "Kotlin KDoc must attach to `greet`, got doc={:?}",
            g.doc,
        );
    }

    #[test]
    fn parse_single_file_swift_doc_attaches_to_next_decl() {
        let source = "/// Greets the caller.\nfunc greet(_ name: String) -> String { return \"hi \\(name)\" }\n";
        let (entities, _) = parse_single_file(source, "t.swift", "swift").unwrap();
        let g = entities.iter().find(|e| e.name == "greet").expect("greet");
        assert!(
            g.doc.as_deref().map(|d| d.contains("Greets the caller")).unwrap_or(false),
            "Swift /// doc must attach to `greet`, got doc={:?}",
            g.doc,
        );
    }

    #[test]
    fn parse_single_file_scala_doc_attaches_to_next_decl() {
        let source = "/**\n * Greets the caller.\n */\ndef greet(name: String): String = s\"hi $name\"\n";
        let (entities, _) = parse_single_file(source, "t.scala", "scala").unwrap();
        let g = entities.iter().find(|e| e.name == "greet").expect("greet");
        assert!(
            g.doc.as_deref().map(|d| d.contains("Greets the caller")).unwrap_or(false),
            "Scaladoc /** */ must attach to `greet`, got doc={:?}",
            g.doc,
        );
    }

    #[test]
    fn parse_single_file_php_doc_attaches_to_next_decl() {
        let source = "<?php\n/**\n * Greets the caller.\n */\nfunction greet(string $name): string { return \"hi $name\"; }\n";
        let (entities, _) = parse_single_file(source, "t.php", "php").unwrap();
        let g = entities.iter().find(|e| e.name == "greet").expect("greet");
        assert!(
            g.doc.as_deref().map(|d| d.contains("Greets the caller")).unwrap_or(false),
            "PHPDoc /** */ must attach to `greet`, got doc={:?}",
            g.doc,
        );
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
