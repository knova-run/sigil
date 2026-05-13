//! Framework-aware dead-code detection with confidence tiers.
//!
//! Walks the `.sigil/` index (entities + refs) and surfaces functions,
//! classes, and methods that have no incoming references. Two big sources
//! of noise in naive dead-code detection are (a) HTTP route handlers
//! that frameworks dispatch via decorators / registration rather than
//! direct calls, and (b) plugin / handler / factory exports that
//! external code reaches by dynamic lookup. This module excludes both
//! and emits a confidence score so CI can shed the most-likely-noise
//! tier behind `--safe-only`.
//!
//! The frame-of-reference is the existing `.sigil/` index — we don't
//! re-parse source files here. Framework exclusion is done via the same
//! regex patterns that `src/contracts.rs` uses for HTTP route detection
//! (extended with chi/gin/echo for Go and NestJS for TS/JS); see
//! `framework_route_patterns()` for the registry.
//!
//! Confidence tiers:
//!   - 1.00 — file with zero incoming graph edges AND not a framework entry point
//!   - 0.85 — exported symbol with zero call sites AND not matching dynamic-name patterns
//!   - 0.70 — internal (private) helper with zero call sites
//!   - <0.70 — surfaced only with `--include-low-confidence`
//!
//! `--safe-only` filters to confidence ≥ 0.85 (the CI-safe tier — file-
//! level dead files + exported-orphan symbols; drops the 0.70
//! internal-helper tier, which is higher false-positive rate).

use anyhow::Result;
use regex::Regex;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use crate::entity::Entity;
use crate::query;
use crate::query::index::Index;

/// One dead-code candidate. JSON output is backward-compatible: the new
/// fields all use `skip_serializing_if = "Option::is_none"` (or bool
/// `is_false` for `recent_activity`), so consumers that key off the
/// original fields keep working.
#[derive(Debug, Serialize, PartialEq)]
pub struct DeadCodeCandidate {
    /// What kind of dead code: "file" or "symbol".
    pub kind: String,
    pub file: String,
    /// None for file-scope candidates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Entity kind (function / class / method / ...). None for file-scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_start: Option<u32>,
    /// Confidence score: 1.0 (most confident) down to 0.0.
    pub confidence: f64,
    /// The trailing suffix (Plugin / Handler / ...) that matched the
    /// dynamic-name list, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dynamic_name_match: Option<String>,
    /// True if the file's last commit is within `--activity-window-days`.
    #[serde(skip_serializing_if = "is_false")]
    pub recent_activity: bool,
    /// Per-file primary author from git history (highest commit-count
    /// author email over the last `--activity-window-days * N` commits;
    /// see `crate::ownership::mine`). None when the file has no git
    /// history in this repo. Mirrors repowise's `primary_owner` field
    /// on dead-code findings, useful for `@-mention the right person
    /// before deleting their code` flows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_owner: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Per-language framework route patterns. The regex must match a line
/// in a source file of `language` and indicates that the surrounding
/// symbol is a framework-registered entry point — i.e. it has incoming
/// edges that the static call-graph doesn't capture.
///
/// `tag` is a short stable identifier (`flask_route`, `fastapi_route`,
/// `django_views`, ...) used internally and by tests to attribute a
/// match to a specific framework rule.
pub struct FrameworkPattern {
    pub language: &'static str,
    pub re: &'static Regex,
    pub tag: &'static str,
}

/// All framework route patterns, lazily compiled.
pub fn framework_route_patterns() -> &'static [FrameworkPattern] {
    static PATS: OnceLock<Vec<FrameworkPattern>> = OnceLock::new();
    PATS.get_or_init(build_patterns).as_slice()
}

fn build_patterns() -> Vec<FrameworkPattern> {
    fn re(s: &str) -> &'static Regex {
        let b = Box::new(Regex::new(s).unwrap());
        Box::leak(b)
    }
    vec![
        // ── Python ──────────────────────────────────────────────────
        // Flask: @app.route("/foo") or @app.route("/foo", methods=["GET"])
        FrameworkPattern {
            language: "python",
            re: re(r#"@\s*[A-Za-z_][A-Za-z0-9_]*\.route\(\s*['"][^'"]+['"]"#),
            tag: "flask_route",
        },
        // FastAPI: @app.get("/foo") / @router.post("/foo") etc.
        // Same shape as src/contracts.rs::fastapi_re; tagged here so the
        // dead-code module owns its naming.
        FrameworkPattern {
            language: "python",
            re: re(r#"@\s*[A-Za-z_][A-Za-z0-9_]*\.(get|post|put|delete|patch|options|head)\(\s*['"][^'"]+['"]"#),
            tag: "fastapi_route",
        },
        // Django routing is detected by filename convention rather than
        // a regex over file contents — `urls.py` always carries
        // urlpatterns and `views.py` always carries view functions, both
        // by Django convention. A bare `\bpath\(...\)` regex misfires on
        // any non-Django code that calls a custom `path()` helper, so
        // pattern-based detection here would over-exclude. See
        // `framework_filename_match` for the Django filename rule.

        // ── Go ──────────────────────────────────────────────────────
        // net/http: mux.HandleFunc("/path", handler) — also matches
        // http.HandleFunc(...).
        FrameworkPattern {
            language: "go",
            re: re(r#"\b[A-Za-z_][A-Za-z0-9_]*\.HandleFunc\(\s*['"`][^'"`]+['"`]"#),
            tag: "go_net_http",
        },
        // chi: r.Get/Post/Put/Delete/Patch — Title-case verbs.
        FrameworkPattern {
            language: "go",
            re: re(r#"\b[A-Za-z_][A-Za-z0-9_]*\.(Get|Post|Put|Delete|Patch|Options|Head)\(\s*['"`][^'"`]+['"`]"#),
            tag: "go_chi",
        },
        // gin / echo: r.GET/POST/PUT/DELETE/PATCH — all-caps verbs.
        FrameworkPattern {
            language: "go",
            re: re(r#"\b[A-Za-z_][A-Za-z0-9_]*\.(GET|POST|PUT|DELETE|PATCH|OPTIONS|HEAD)\(\s*['"`][^'"`]+['"`]"#),
            tag: "go_gin_echo",
        },

        // ── JS / TS ─────────────────────────────────────────────────
        // Express verbs — same shape as src/contracts.rs::express_re,
        // re-tagged for dead-code attribution.
        FrameworkPattern {
            language: "javascript",
            re: re(r#"\b[A-Za-z_][A-Za-z0-9_]*\.(get|post|put|delete|patch|options|head|all)\(\s*['"`][^'"`]+['"`]"#),
            tag: "express_route",
        },
        FrameworkPattern {
            language: "typescript",
            re: re(r#"\b[A-Za-z_][A-Za-z0-9_]*\.(get|post|put|delete|patch|options|head|all)\(\s*['"`][^'"`]+['"`]"#),
            tag: "express_route",
        },
        // NestJS decorators on TS/JS classes: @Controller, @Get, @Post...
        FrameworkPattern {
            language: "typescript",
            re: re(r#"@(Controller|Get|Post|Put|Delete|Patch|Options|Head|All|Module|Injectable)\b"#),
            tag: "nestjs_decorator",
        },
        FrameworkPattern {
            language: "javascript",
            re: re(r#"@(Controller|Get|Post|Put|Delete|Patch|Options|Head|All|Module|Injectable)\b"#),
            tag: "nestjs_decorator",
        },

        // ── Kotlin ──────────────────────────────────────────────────
        // Ktor: `routing { get("/path") { ... } }` — DSL builder with
        // bare verb function calls that take a path-string argument.
        // Anchored on the verb being preceded by whitespace or `{`,
        // and a string literal in the first parameter slot, so it
        // doesn't fire on every `get(...)` method call.
        FrameworkPattern {
            language: "kotlin",
            re: re(r#"(?m)(?:^|\s|\{)(get|post|put|delete|patch|options|head)\(\s*"[^"]+""#),
            tag: "ktor_route",
        },
        // Spring MVC / Spring Boot: @RestController / @Controller class
        // annotation, and @GetMapping / @PostMapping / @RequestMapping
        // method annotations. Any of these on a file is enough to mark
        // it as a Spring entry point.
        FrameworkPattern {
            language: "kotlin",
            re: re(r#"@(RestController|Controller|GetMapping|PostMapping|PutMapping|DeleteMapping|PatchMapping|RequestMapping)\b"#),
            tag: "spring_annotation",
        },

        // ── Swift ───────────────────────────────────────────────────
        // Vapor: `app.get("/path") { req in ... }` — same shape as
        // Express but with Swift trailing closures. Backticks aren't
        // valid Swift string delimiters so the alternation drops them.
        FrameworkPattern {
            language: "swift",
            re: re(r#"\b[A-Za-z_][A-Za-z0-9_]*\.(get|post|put|delete|patch|options|head)\(\s*"[^"]+""#),
            tag: "vapor_route",
        },

        // ── Scala ───────────────────────────────────────────────────
        // Akka HTTP / pekko-http: `path("...") { get { complete(...) } }`.
        // The `path("...")` directive is the unambiguous tell — bare
        // method-named verbs would over-match. We deliberately don't
        // match unqualified `get { ... }` directives because plain
        // builder DSLs use the same shape outside HTTP.
        FrameworkPattern {
            language: "scala",
            re: re(r#"\bpath\(\s*"[^"]+"\s*\)\s*\{"#),
            tag: "akka_http_route",
        },
        // Play Framework: `Action { request => ... }` with the bare
        // builder constructor and request callback. Tighter than the
        // generic `Action` token alone.
        FrameworkPattern {
            language: "scala",
            re: re(r#"\bAction(?:\.async)?\s*(?:\([^)]*\)\s*)?\{"#),
            tag: "play_action",
        },

        // ── PHP ─────────────────────────────────────────────────────
        // Laravel: `Route::get('/path', ...)` / `Route::post(...)` etc.
        // Matches the canonical static-method form in routes/web.php.
        FrameworkPattern {
            language: "php",
            re: re(r#"\bRoute::(get|post|put|delete|patch|options|any|match|resource|view|redirect)\(\s*['"][^'"]+['"]"#),
            tag: "laravel_route",
        },
        // Symfony: `#[Route('/path', ...)]` PHP 8 attribute on a
        // controller class or method.
        FrameworkPattern {
            language: "php",
            re: re(r#"#\[\s*Route\s*\(\s*['"][^'"]+['"]"#),
            tag: "symfony_route",
        },
    ]
}

/// Dynamic-name suffixes — any exported symbol whose final identifier
/// matches `<X>(Plugin|Handler|Adapter|Middleware|Provider|Factory|Service)$`
/// is treated as reachable via dynamic registration / lookup and downgraded.
///
/// Returns the matched suffix string (e.g. `"Handler"`) so callers can
/// surface the reason in JSON output.
pub fn dynamic_name_match(name: &str) -> Option<&'static str> {
    // Operate on the trailing identifier so qualified names like
    // `module::MyHandler` still match.
    let tail = name.rsplit("::").next().unwrap_or(name);
    let tail = tail.rsplit('.').next().unwrap_or(tail);
    static SUFFIXES: &[&str] = &[
        "Plugin", "Handler", "Adapter", "Middleware", "Provider", "Factory", "Service",
    ];
    for s in SUFFIXES {
        if tail.ends_with(s) && tail.len() > s.len() {
            return Some(*s);
        }
    }
    None
}

/// Detect language from a file extension. Mirrors the small whitelist
/// the patterns above target; unknown extensions return None.
fn lang_for_ext(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "py" => "python",
        "go" => "go",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "ts" | "tsx" => "typescript",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "scala" | "sc" => "scala",
        "php" | "phtml" | "phps" => "php",
        _ => return None,
    })
}

fn file_ext(file: &str) -> Option<&str> {
    file.rsplit_once('.').map(|(_, e)| e)
}

/// Scan a file and return `Some(tag)` for the first framework pattern
/// that matches anywhere in the file, or None if no pattern matches.
/// Errors reading the file map to None (the file just won't be marked
/// as a framework entry point — safer than panicking on permission
/// errors).
///
/// Filename-based rules (e.g. Django `urls.py` / `views.py`) are checked
/// before file content; they're cheaper and avoid the false-positive
/// regex problem of pattern-based detection.
pub fn first_framework_match(file_path: &Path, language: &str) -> Option<&'static str> {
    if let Some(tag) = framework_filename_match(file_path, language) {
        return Some(tag);
    }
    let text = std::fs::read_to_string(file_path).ok()?;
    for p in framework_route_patterns() {
        if p.language == language && p.re.is_match(&text) {
            return Some(p.tag);
        }
    }
    None
}

/// Filename-convention framework detection. Used for cases where the
/// file's role is unambiguous from its name alone:
///
/// - Django: `urls.py` (route table) and `views.py` (view functions)
///   both qualify as framework entry points. Pattern-based detection
///   over file contents would over-exclude legitimate non-Django code
///   that happens to call a custom `path(...)` helper.
fn framework_filename_match(file_path: &Path, language: &str) -> Option<&'static str> {
    if language != "python" {
        return None;
    }
    let name = file_path.file_name().and_then(|n| n.to_str())?;
    match name {
        "urls.py" => Some("django_urls"),
        "views.py" => Some("django_views"),
        _ => None,
    }
}

/// In-text framework match without I/O — used by callers that already
/// have the file bytes (tests, fixtures).
pub fn first_framework_match_in_text(text: &str, language: &str) -> Option<&'static str> {
    for p in framework_route_patterns() {
        if p.language == language && p.re.is_match(text) {
            return Some(p.tag);
        }
    }
    None
}

/// Configuration for `find_dead_code`.
pub struct DeadCodeConfig {
    /// Restrict to confidence ≥ 0.85 (the CI-safe tier: dead files +
    /// exported-orphan symbols; excludes the 0.70 internal-helper tier
    /// because its false-positive rate is too high for CI gates).
    pub safe_only: bool,
    /// Include candidates with confidence < 0.70 (off by default).
    pub include_low_confidence: bool,
    /// Activity window for `recent_activity`, in days. Default 30.
    pub activity_window_days: u64,
    /// User-supplied regex patterns; any candidate whose `name` matches
    /// is dropped from output entirely. Use this for project-specific
    /// naming conventions that the built-in dynamic-name suffix list
    /// (Plugin / Handler / Adapter / ...) doesn't cover.
    pub exclude_patterns: Vec<Regex>,
}

impl Default for DeadCodeConfig {
    fn default() -> Self {
        Self {
            safe_only: false,
            include_low_confidence: false,
            activity_window_days: 30,
            exclude_patterns: Vec::new(),
        }
    }
}

/// Walk the `.sigil/` index under `root` and return dead-code candidates.
///
/// Lifts entities + refs via `query::load`. Three passes:
///   1. **Reference pre-pass** — record which files have at least one
///      entity (any kind) referenced from outside. Used by the file
///      sweep below.
///   2. **Symbol sweep** — exported (and optionally internal) callable
///      entities with zero callers. Excluded if the name matches a
///      dynamic-name suffix or a user-supplied `--exclude-pattern`.
///   3. **File sweep** — files with zero entries in the pre-pass set
///      and not flagged as a framework entry-point file (Django
///      `urls.py` / `views.py`, Flask / FastAPI / Express / NestJS /
///      Go chi/gin/echo route registrations).
pub fn find_dead_code(root: &Path, cfg: &DeadCodeConfig) -> Result<Vec<DeadCodeCandidate>> {
    let idx = query::load(root)?;
    let mut out = find_dead_code_in_index(root, &idx, cfg);
    // Join per-file primary owner from git history. Mining is cheap on
    // small repos and capped at 500 commits — same default as
    // `sigil ownership`. Silently tolerate non-git repos / missing
    // history.
    if let Ok(rows) = crate::ownership::mine(root, 500) {
        let owners: HashMap<String, String> = rows
            .into_iter()
            .map(|r| (r.file, r.primary_owner))
            .collect();
        for c in &mut out {
            if let Some(o) = owners.get(&c.file) {
                c.primary_owner = Some(o.clone());
            }
        }
    }
    Ok(out)
}

/// Pure version of `find_dead_code` — takes an already-loaded Index.
/// Used by tests and by callers that want to avoid re-loading the index.
pub fn find_dead_code_in_index(
    root: &Path,
    idx: &Index,
    cfg: &DeadCodeConfig,
) -> Vec<DeadCodeCandidate> {
    let mut out = Vec::new();

    // Recent-activity cache: file → bool. Cheap to compute once per file.
    let mut activity_cache: HashMap<String, bool> = HashMap::new();
    let mut activity_for = |file: &str| -> bool {
        if let Some(&v) = activity_cache.get(file) {
            return v;
        }
        let v = file_recent_activity(root, file, cfg.activity_window_days);
        activity_cache.insert(file.to_string(), v);
        v
    };

    // Framework-entry-point cache: file → Option<tag>.
    let mut fw_cache: HashMap<String, Option<&'static str>> = HashMap::new();
    let mut fw_for = |file: &str| -> Option<&'static str> {
        if let Some(&v) = fw_cache.get(file) {
            return v;
        }
        let v = match file_ext(file).and_then(lang_for_ext) {
            Some(lang) => first_framework_match(&root.join(file), lang),
            None => None,
        };
        fw_cache.insert(file.to_string(), v);
        v
    };

    // ── Reference pre-pass ──────────────────────────────────────────
    // Track which files contain at least one entity that something else
    // references — independent of entity kind, so a constants-only module
    // imported by another file isn't later flagged as a dead file just
    // because the symbol sweep skips non-callable kinds.
    let mut files_with_referenced_symbols: HashSet<String> = HashSet::new();
    for entity in &idx.entities {
        let leaf_used = idx.refs_to(&entity.name).next().is_some();
        let qualified_used = entity
            .qualified_name
            .as_deref()
            .map(|q| idx.refs_to(q).next().is_some())
            .unwrap_or(false);
        if leaf_used || qualified_used {
            files_with_referenced_symbols.insert(entity.file.clone());
        }
    }

    // ── Symbol sweep ────────────────────────────────────────────────
    // Emit per-entity candidates for callable kinds only. The file
    // pre-pass above is used by the file sweep below regardless of kind.
    for entity in &idx.entities {
        if !is_callable_kind(&entity.kind) {
            continue;
        }
        // P5.15 external sentinels + sigil's native non-source-file
        // parsers (Markdown/JSON/YAML/TOML) shouldn't surface here.
        if is_non_source_file(&entity.file) {
            continue;
        }
        // Refs are indexed by both fully-qualified name and trailing
        // segment, so a plain `refs_to(name)` here matches both forms.
        let has_callers = idx.refs_to(&entity.name).next().is_some();
        if has_callers {
            continue;
        }
        // Also check the qualified name when present — Rust method refs
        // can land under `Struct::method` rather than the bare leaf.
        if let Some(q) = &entity.qualified_name {
            if idx.refs_to(q).next().is_some() {
                continue;
            }
        }

        // Exclude tests entirely — test code calling itself is "dead"
        // by the call graph but obviously not dead. Aligns with the
        // exclude-tests posture other agent-facing commands take.
        if crate::entity::is_test_path(&entity.file) {
            continue;
        }

        // User-supplied regex exclusions — drop the candidate entirely.
        if cfg
            .exclude_patterns
            .iter()
            .any(|re| re.is_match(&entity.name))
        {
            continue;
        }

        let dynamic = dynamic_name_match(&entity.name);
        let exported = is_exported(entity);

        // Framework attribution: only flag if the candidate is itself
        // a framework entry point (i.e. its file contains a route
        // registration). Otherwise the file-sweep will catch the file
        // and we don't double-report.
        let fw = fw_for(&entity.file);

        let confidence = classify_symbol_confidence(exported, dynamic.is_some(), fw.is_some());

        if fw.is_some() {
            // Symbol lives in a framework entry-point file — treat as
            // reachable; skip emission. Conservative bias.
            continue;
        }

        if !cfg.include_low_confidence && confidence < 0.70 {
            continue;
        }
        if cfg.safe_only && confidence < SAFE_ONLY_THRESHOLD {
            continue;
        }

        out.push(DeadCodeCandidate {
            kind: "symbol".to_string(),
            file: entity.file.clone(),
            name: Some(entity.name.clone()),
            entity_kind: Some(entity.kind.clone()),
            line_start: Some(entity.line_start),
            confidence,
            dynamic_name_match: dynamic.map(|s| s.to_string()),
            recent_activity: activity_for(&entity.file),
            primary_owner: None,
        });
    }

    // ── File sweep ──────────────────────────────────────────────────
    // A file is dead if (a) none of its symbols have callers, AND
    // (b) it is not a framework entry-point file.
    let mut files_seen: HashSet<String> = HashSet::new();
    for entity in &idx.entities {
        if !files_seen.insert(entity.file.clone()) {
            continue;
        }
        if crate::entity::is_test_path(&entity.file) {
            continue;
        }
        if is_non_source_file(&entity.file) {
            continue;
        }
        if files_with_referenced_symbols.contains(&entity.file) {
            continue;
        }
        if let Some(tag) = fw_for(&entity.file) {
            // Framework entry-point file — explicitly NOT dead.
            // (Don't emit; that's the whole point of framework
            // exclusion. The tag is available via the public
            // `first_framework_match` API for callers that want it.)
            let _ = tag;
            continue;
        }

        // Top-tier file-level dead code: zero incoming edges anywhere
        // in the file and not framework-registered.
        let confidence = 1.00;

        if cfg.safe_only && confidence < SAFE_ONLY_THRESHOLD {
            continue;
        }

        out.push(DeadCodeCandidate {
            kind: "file".to_string(),
            file: entity.file.clone(),
            name: None,
            entity_kind: None,
            line_start: None,
            confidence,
            dynamic_name_match: None,
            recent_activity: activity_for(&entity.file),
            primary_owner: None,
        });
    }

    // CLAUDE.md convention: sort by (file, line_start) for deterministic
    // output. File-scope candidates (`line_start = None`) sort before any
    // line-numbered symbol candidates on the same file because Rust's
    // `Option::None < Some(_)` ordering gives that for free.
    out.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| a.line_start.cmp(&b.line_start))
    });
    out
}

/// Entity kinds we consider callable for dead-code purposes. Constants /
/// variables / imports are excluded — they're already lightly indexed
/// and most consumers don't want them flagged.
fn is_callable_kind(kind: &str) -> bool {
    matches!(kind, "function" | "method" | "class" | "struct" | "interface" | "enum")
}

/// Files we never report as dead — these aren't executable code even
/// when sigil's native JSON/YAML/TOML/Markdown parsers emit entities
/// for their keys/headings. Source code is the dead-code target; docs
/// and config rot is a separate problem (handled by linters / git
/// hygiene, not by a call-graph dead-code report).
fn is_non_source_file(file: &str) -> bool {
    // The P5.15 synthetic external entities have a `<external>` file
    // marker that isn't a real path. Drop those first.
    if file == "<external>" || file.starts_with("external:") {
        return true;
    }
    let lower = file.to_ascii_lowercase();
    for ext in &[
        ".md", ".markdown", ".rst", ".txt",
        ".yml", ".yaml", ".toml", ".json", ".jsonc",
        ".ini", ".cfg", ".conf",
        ".csv", ".tsv", ".xml",
    ] {
        if lower.ends_with(ext) {
            return true;
        }
    }
    false
}

/// Confidence floor enforced by `--safe-only`. Set just above the
/// internal-helper tier (0.70) so the flag is strictly more restrictive
/// than the default — admits `unused_export` (0.85) and `dead_file`
/// (1.00), drops `internal_helper` (0.70) and `dynamic_name_match`
/// (0.50). Matches the docstring's "for CI shipping" intent: CI auto-
/// delete should only run on the highest-confidence findings.
const SAFE_ONLY_THRESHOLD: f64 = 0.85;

/// Whether an entity is exported (public visibility). The parser writes
/// visibility as None when private (default for many languages), or
/// "public" / similar when explicit. We treat None as "private" for the
/// confidence tier — matches the on-disk JSONL semantics in `entity.rs`.
fn is_exported(e: &Entity) -> bool {
    match e.visibility.as_deref() {
        Some("public") | Some("export") | Some("pub") => true,
        // Heuristic fallbacks for languages where the parser may not
        // emit a visibility field but naming is the signal:
        //   - Go: leading uppercase identifier = exported.
        //   - Python: leading underscore = private.
        None => is_exported_by_convention(&e.file, &e.name),
        _ => false,
    }
}

fn is_exported_by_convention(file: &str, name: &str) -> bool {
    let tail = name.rsplit("::").next().unwrap_or(name);
    let tail = tail.rsplit('.').next().unwrap_or(tail);
    let first = tail.chars().next();
    let ext = file_ext(file).unwrap_or("");
    match ext {
        // Go: capital-first identifiers are exported.
        "go" => first.map(|c| c.is_ascii_uppercase()).unwrap_or(false),
        // Python: underscore-prefix is private; everything else is public.
        "py" => first.map(|c| c != '_').unwrap_or(false),
        // JS/TS: hard to tell statically without parsing import/export
        // statements; default to true so we don't suppress real dead code.
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => true,
        // Rust: without an explicit `pub` we treat as private.
        "rs" => false,
        _ => true,
    }
}

/// Confidence classifier per the issue spec.
fn classify_symbol_confidence(exported: bool, dynamic_name: bool, framework: bool) -> f64 {
    if framework {
        // Framework-attributed symbols never make it past the sweep,
        // but if a caller reaches this fn directly they get 0.0.
        return 0.0;
    }
    if dynamic_name {
        // Dynamic-name match → low-confidence tier regardless of
        // visibility. A private `_serviceImpl` is just as plugin-shaped
        // as an exported one and shouldn't leak through `--safe-only`.
        return 0.50;
    }
    if exported {
        return 0.85;
    }
    // Internal helper with no callers.
    0.70
}

/// Read the unix-epoch timestamp of the most recent commit touching
/// `file` and return true if it is within `window_days` of "now". A git
/// failure (not a repo, file untracked) → false: we can't claim "recent
/// activity" without evidence.
fn file_recent_activity(root: &Path, file: &str, window_days: u64) -> bool {
    let out = match Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["log", "-1", "--format=%at", "--"])
        .arg(file)
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return false,
    };
    let s = String::from_utf8_lossy(&out.stdout);
    let ts: u64 = match s.trim().parse() {
        Ok(t) => t,
        Err(_) => return false,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if now == 0 {
        return false;
    }
    let cutoff = now.saturating_sub(window_days * 86_400);
    ts >= cutoff
}

/// Test-only re-export of the symbol-confidence classifier so the
/// integration test crate (which links the library) can pin tier
/// boundaries without us exposing every internal helper.
#[doc(hidden)]
pub fn _classify_for_test(exported: bool, dynamic_name: bool, framework: bool) -> f64 {
    classify_symbol_confidence(exported, dynamic_name, framework)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_name_matches_known_suffixes() {
        assert_eq!(dynamic_name_match("AuthHandler"), Some("Handler"));
        assert_eq!(dynamic_name_match("LoggingMiddleware"), Some("Middleware"));
        assert_eq!(dynamic_name_match("module::UserService"), Some("Service"));
        assert_eq!(dynamic_name_match("module.AdminPlugin"), Some("Plugin"));
        assert_eq!(dynamic_name_match("UserAdapter"), Some("Adapter"));
        assert_eq!(dynamic_name_match("SessionFactory"), Some("Factory"));
        assert_eq!(dynamic_name_match("ConfigProvider"), Some("Provider"));
    }

    #[test]
    fn dynamic_name_does_not_match_bare_suffix() {
        // "Service" by itself isn't a suffix-of-a-larger-name; the
        // tail.len() > suffix.len() guard rejects it.
        assert_eq!(dynamic_name_match("Service"), None);
        assert_eq!(dynamic_name_match("doSomething"), None);
    }

    #[test]
    fn confidence_tiers_match_spec() {
        // Exported + no dynamic + no framework → 0.85
        assert!((classify_symbol_confidence(true, false, false) - 0.85).abs() < 1e-9);
        // Internal (not exported) → 0.70
        assert!((classify_symbol_confidence(false, false, false) - 0.70).abs() < 1e-9);
        // Exported + dynamic-name → 0.50 (low-confidence tier)
        assert!((classify_symbol_confidence(true, true, false) - 0.50).abs() < 1e-9);
        // Private + dynamic-name → 0.50 too — dynamic-name is the
        // signal regardless of visibility, so a private `_serviceImpl`
        // also drops below the safe-only threshold instead of leaking
        // through at 0.70.
        assert!(
            (classify_symbol_confidence(false, true, false) - 0.50).abs() < 1e-9,
            "private dynamic-name match must be 0.50 (low-confidence), got {}",
            classify_symbol_confidence(false, true, false),
        );
        // Framework match → 0.0
        assert!(classify_symbol_confidence(true, false, true).abs() < 1e-9);
    }

    #[test]
    fn flask_route_matches_python_decorator() {
        let text = r#"
@app.route("/health")
def health():
    return "ok"
"#;
        assert_eq!(first_framework_match_in_text(text, "python"), Some("flask_route"));
    }

    #[test]
    fn fastapi_route_matches_python_decorator() {
        let text = r#"
@router.get("/users")
async def list_users():
    return []
"#;
        assert_eq!(first_framework_match_in_text(text, "python"), Some("fastapi_route"));
    }

    #[test]
    fn go_chi_matches() {
        let text = r#"r.Get("/users", listUsers)"#;
        assert_eq!(first_framework_match_in_text(text, "go"), Some("go_chi"));
    }

    #[test]
    fn go_gin_matches() {
        let text = r#"r.GET("/users", listUsers)"#;
        assert_eq!(first_framework_match_in_text(text, "go"), Some("go_gin_echo"));
    }

    #[test]
    fn nestjs_decorator_matches_ts() {
        let text = r#"
@Controller("users")
export class UsersController {
  @Get()
  list() {}
}
"#;
        assert_eq!(first_framework_match_in_text(text, "typescript"), Some("nestjs_decorator"));
    }
}
