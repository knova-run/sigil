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
        resolve_tier3(&all_entities, &mut all_refs);
        resolve_tier2b_imported_fallback(&all_entities, &mut all_refs);
        resolve_member_call(&all_entities, &mut all_refs);
        let extra = resolve_barrel_follow(&all_entities, &all_refs);
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
fn resolve_tier2b_imported_fallback(entities: &[Entity], refs: &mut [Reference]) {
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
            let Some(target) = resolve_module_path(&r.file, normalized, &file_set) else { continue };
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
fn resolve_barrel_follow(entities: &[Entity], refs: &[Reference]) -> Vec<Reference> {
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
            let Some(resolved) = resolve_module_path(file, src, &file_set) else {
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
        let Some(resolved_barrel) = resolve_module_path(&r.file, import_src, &file_set) else {
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
        let edge_name = if rest.is_empty() {
            format!("{origin_module}/{local_name}")
        } else {
            format!("{origin_module}/{rest}")
        };
        additions.push(Reference {
            file: r.file.clone(),
            caller: r.caller.clone(),
            name: edge_name,
            ref_kind: "call".to_string(),
            line: r.line,
            confidence: Some(0.7),
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
) -> Option<String> {
    fn parent_dir(file: &str) -> &str {
        match file.rfind('/') {
            Some(i) => &file[..i],
            None => "",
        }
    }
    let lang = language_from_file(from_file);

    if matches!(lang, Some("javascript") | Some("typescript"))
        && (module.starts_with("./") || module.starts_with("../"))
    {
        let base = parent_dir(from_file);
        let joined = if base.is_empty() {
            module.trim_start_matches("./").to_string()
        } else {
            format!("{}/{}", base, module)
        };
        let normalized = normalize_path(&joined);
        for ext in &["ts", "tsx", "js", "jsx", "mjs", "cjs"] {
            let candidate = format!("{normalized}.{ext}");
            if let Some(hit) = files.get(candidate.as_str()) {
                return Some((*hit).to_string());
            }
        }
        for ext in &["ts", "tsx", "js", "jsx", "mjs", "cjs"] {
            let candidate = format!("{normalized}/index.{ext}");
            if let Some(hit) = files.get(candidate.as_str()) {
                return Some((*hit).to_string());
            }
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
