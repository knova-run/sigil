//! `sigil context <symbol>` — the minimum-viable-context bundle.
//!
//! Collapses the agent loop "read 6 files to understand one function before
//! editing" into a single structured call.
//!
//! Output shape (see `Context` struct):
//!   - the resolved entity (file, line range, signature, visibility)
//!   - direct callers (enclosing symbol + file:line)
//!   - direct callees (with ref_kind so the agent sees the relationship)
//!   - related types used in the symbol's body (ref_kind == type_annotation)
//!   - blast-radius summary
//!
//! All three renderers share the same `Context` data model — the difference
//! is packing and format. `Agent` is compact, short-keyed JSON for LLM
//! ingestion; `Markdown` is human-readable; `Full` is the unabridged JSON.

use std::cmp::Reverse;
use std::collections::HashSet;

use serde::Serialize;

use crate::entity::{BlastRadius, Entity, Reference};
use crate::query::index::Index;

/// Config knobs for a single `sigil context` invocation.
#[derive(Debug, Clone)]
pub struct ContextOptions {
    /// Rough output token cap. 0 = unlimited.
    pub budget: usize,
    /// How many callers / callees / related types to include.
    pub depth: usize,
    pub format: ContextFormat,
    /// When true, filter candidates whose file looks like test code and
    /// also drop test-file callers from the output. Default off — opt-in.
    pub exclude_tests: bool,
    /// Include the symbol's source body (lines `line_start..=line_end`)
    /// inline in the bundle. Off by default — bodies are large and not
    /// every caller wants them. Evals show agents typically follow up a
    /// `sigil context` with a `read_file` on the same line range anyway,
    /// so bundling saves a round-trip when the caller opts in.
    pub with_body: bool,
    /// Root directory used to resolve the entity's `file` path when
    /// reading its body. Defaults to the current directory.
    pub project_root: std::path::PathBuf,
}

impl Default for ContextOptions {
    fn default() -> Self {
        Self {
            budget: 1500,
            depth: 10,
            format: ContextFormat::Markdown,
            exclude_tests: false,
            with_body: false,
            project_root: std::path::PathBuf::from("."),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextFormat {
    /// Compact JSON with short keys — designed for LLM token budgets.
    Agent,
    /// Human-readable markdown.
    Markdown,
    /// Full structured JSON — stable field names, safe to deserialize.
    Full,
}

impl ContextFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "agent" => Some(Self::Agent),
            "markdown" | "md" => Some(Self::Markdown),
            "json" | "full" => Some(Self::Full),
            _ => None,
        }
    }
}

/// A resolved entity — enough to locate it in the codebase and understand
/// its shape without reading the file.
#[derive(Debug, Clone, Serialize)]
pub struct SymbolRef {
    pub file: String,
    pub name: String,
    pub kind: String,
    pub line_start: u32,
    pub line_end: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blast_radius: Option<BlastRadius>,
    /// Author-provided description of the entity, when available — see
    /// `Entity::doc`. Surfaced in `code.context` markdown as a `## Doc`
    /// section so an LLM consumer doesn't need a follow-up file read to
    /// learn what this entity is for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

impl SymbolRef {
    fn from_entity(e: &Entity) -> Self {
        Self {
            file: e.file.clone(),
            name: e.name.clone(),
            kind: e.kind.clone(),
            line_start: e.line_start,
            line_end: e.line_end,
            parent: e.parent.clone(),
            sig: e.sig.clone(),
            visibility: e.visibility.clone(),
            blast_radius: e.blast_radius,
            doc: e.doc.clone(),
        }
    }
}

/// One edge in the context graph — caller or callee.
#[derive(Debug, Clone, Serialize)]
pub struct Edge {
    pub file: String,
    pub line: u32,
    pub symbol: String,
    /// `ref_kind` from the Reference row (call, import, type_annotation,
    /// instantiation, …). Surface it so the agent doesn't have to guess
    /// whether a row is a function call or a type usage.
    pub kind: String,
    /// Enclosing symbol where the reference appears, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Context {
    pub query: String,
    /// The entity the context was built for.
    pub chosen: SymbolRef,
    /// Source body of the chosen entity, when `--with-body` was set and
    /// the file could be read. Contains the raw lines `line_start..=line_end`
    /// (1-indexed, inclusive), joined with `\n`. None otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// When `query` resolves to multiple entities, the others are surfaced
    /// so the caller can disambiguate on the next invocation.
    pub alternatives: Vec<SymbolRef>,
    pub callers: Vec<Edge>,
    pub callees: Vec<Edge>,
    pub related_types: Vec<Edge>,
    /// When `chosen` is a class with heritage edges (extend / implement /
    /// trait_impl), the resolved parent class entities. Lets an agent
    /// see `class Flask(App):` → `App` lives at `src/flask/sansio/app.py`
    /// without a separate `sigil heritage` call. Empty for entities
    /// with no resolvable heritage edges.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parents: Vec<SymbolRef>,
    /// When `chosen` is a method (has a parent class), other classes in
    /// the codebase that define a method with the same tail segment —
    /// the inheritance / polymorphism delta. Empty for non-method
    /// symbols. Capped at 5 to avoid blowing the budget; the count in
    /// `skipped_overrides` tracks truncation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<SymbolRef>,
    pub skipped_callers: usize,
    pub skipped_callees: usize,
    pub skipped_types: usize,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub skipped_overrides: usize,
    /// When the chosen entity was found by walking the parent class's
    /// heritage chain (e.g. `Flask::testing` → `App::testing`), this
    /// is `Some("heritage")`. Standard direct resolutions leave it None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_via: Option<String>,
    /// Bare name of the ancestor class on which the chosen entity was
    /// found, when `resolved_via = "heritage"`. Lets the agent see
    /// where the inherited member actually lives without a second call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ancestor: Option<String>,
    pub estimated_tokens: usize,
}

fn is_zero_usize(n: &usize) -> bool {
    *n == 0
}

/// Parse a query string into (optional file filter, optional parent filter, name).
///
/// Accepted forms:
///   - `foo`                      — bare name
///   - `Foo::bar`                 — `bar` with parent `Foo`
///   - `src/x.rs::bar`            — `bar` in `src/x.rs`
///   - `src/x.rs::Foo::bar`       — `bar` with parent `Foo` in `src/x.rs`
fn split_query(query: &str) -> (Option<&str>, Option<&str>, &str) {
    let parts: Vec<&str> = query.split("::").collect();
    match parts.len() {
        1 => (None, None, parts[0]),
        2 => {
            // Either file::name (first part looks like a path) or parent::name.
            let a = parts[0];
            let b = parts[1];
            if a.contains('/') || a.contains('.') {
                (Some(a), None, b)
            } else {
                (None, Some(a), b)
            }
        }
        _ => {
            // 3+ parts: last = name, second-last = parent, everything before = file.
            let name = parts[parts.len() - 1];
            let parent = parts[parts.len() - 2];
            let file = parts[..parts.len() - 2].join("::");
            // Leak to match the &str return. Acceptable — resolve is called once
            // per CLI invocation. Using a heap-allocated String + lifetime gymnastics
            // would clutter call sites for no practical win.
            let file_static: &'static str = Box::leak(file.into_boxed_str());
            (Some(file_static), Some(parent), name)
        }
    }
}

/// Find every entity in `idx` that matches the query. Sort by impact so
/// ambiguous names pick up the load-bearing definition first.
pub fn resolve<'a>(idx: &'a Index, query: &str) -> Vec<&'a Entity> {
    let (file_hint, parent_hint, name) = split_query(query);

    let mut matches: Vec<&Entity> = idx
        .entities_by_name(name)
        .filter(|e| match file_hint {
            Some(f) => e.file == f || e.file.ends_with(f),
            None => true,
        })
        .filter(|e| match parent_hint {
            Some(p) => e.parent.as_deref() == Some(p),
            None => true,
        })
        // Don't resolve to imports — `sigil context use foo::bar` is never
        // what the caller wants; they want the defining entity.
        .filter(|e| e.kind != "import")
        .collect();

    // Rank by blast direct_files desc (load-bearing definition first), then
    // by line_start ascending for stable output on ties.
    matches.sort_by_key(|e| {
        (
            Reverse(e.blast_radius.as_ref().map(|b| b.direct_files).unwrap_or(0)),
            e.line_start,
        )
    });

    matches
}

/// No-match response payload for `sigil context <q>` when no entity
/// resolves. Emitted as JSON on stdout under `--format agent` and
/// `--format json` so script consumers can branch on a parseable shape
/// instead of regexing stderr. Short keys mirror the `Agent` view.
#[derive(Debug, Clone, Serialize)]
pub struct NoMatch {
    pub q: String,
    pub resolved: bool,
    pub reason: String,
    pub candidates: Vec<Candidate>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub f: String,
    pub n: String,
    pub k: String,
    pub l: u32,
}

/// Build the `{q, resolved: false, reason, candidates}` payload for
/// `sigil context` queries that fail to resolve. Candidates come from
/// `Index::search(Scope::All, limit=10)` — a substring scan over symbol
/// names and file paths.
pub fn build_no_match(idx: &Index, q: &str) -> NoMatch {
    let hits = idx.search(q, crate::query::index::Scope::All, None, None, 10);
    let mut candidates: Vec<Candidate> = hits
        .into_iter()
        .map(|h| match h {
            crate::query::index::SearchHit::Symbol(e) => Candidate {
                f: e.file.clone(),
                n: e.name.clone(),
                k: e.kind.clone(),
                l: e.line_start,
            },
            crate::query::index::SearchHit::File(fh) => Candidate {
                f: fh.path.clone(),
                n: std::path::Path::new(&fh.path)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| fh.path.clone()),
                k: "file".to_string(),
                l: 0,
            },
        })
        .collect();
    if candidates.is_empty() {
        for name in crate::query::suggest_similar(idx, q, 10) {
            if let Some(e) = idx.entities_by_name(&name).next() {
                candidates.push(Candidate {
                    f: e.file.clone(),
                    n: e.name.clone(),
                    k: e.kind.clone(),
                    l: e.line_start,
                });
            }
        }
    }
    NoMatch {
        q: q.to_string(),
        resolved: false,
        reason: format!("no entity matches `{}`", q),
        candidates,
    }
}

/// Per-file digest returned by [`build_file_context`] when the query
/// matches a file path in the index. Mirrors the per-symbol `Context`
/// but aggregated over a whole file: top-level outline entities plus
/// (later) aggregated cross-file refs.
#[derive(Debug, Clone, Serialize)]
pub struct FileContext {
    pub q: String,
    pub file: String,
    pub entities: Vec<FileEntity>,
    /// External callers (refs from other files) of any entity defined
    /// in this file. `None` when not yet computed; empty `Vec` when
    /// computed but no callers exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_callers: Option<Vec<Edge>>,
    /// Outbound refs from inside this file to symbols defined
    /// elsewhere — the file's external surface area in the other
    /// direction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_callees: Option<Vec<Edge>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileEntity {
    pub name: String,
    pub kind: String,
    pub line_start: u32,
    pub line_end: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

/// Build a per-file digest for `query` when it matches a file in the
/// index. Returns `None` when no entity has `file == query`. The
/// entity list is filtered to top-level outline shape (classes,
/// structs, top-level functions, …) so the agent gets a file shape
/// without methods cluttering the view.
pub fn build_file_context(idx: &Index, query: &str) -> Option<FileContext> {
    if idx.entities_by_file(query).next().is_none() {
        return None;
    }
    let mut entities: Vec<FileEntity> = idx
        .entities_by_file(query)
        .filter(|e| crate::query::is_top_level_outline(e))
        .map(|e| FileEntity {
            name: e.name.clone(),
            kind: e.kind.clone(),
            line_start: e.line_start,
            line_end: e.line_end,
            visibility: e.visibility.clone(),
            doc: e.doc.clone(),
        })
        .collect();
    entities.sort_by_key(|e| e.line_start);

    // Aggregate external callers: every ref whose target is an entity
    // defined in this file, *from* a different file. Dedup by (file, line).
    let target_names: std::collections::HashSet<&str> = idx
        .entities_by_file(query)
        .map(|e| e.name.as_str())
        .collect();
    let mut seen: HashSet<(String, u32)> = HashSet::new();
    let top_callers: Vec<Edge> = target_names
        .iter()
        .flat_map(|name| idx.refs_to(name))
        .filter(|r| r.file != query)
        .filter(|r| seen.insert((r.file.clone(), r.line)))
        .map(caller_edge)
        .collect();

    // Aggregate outbound callees: refs whose caller is inside this
    // file but whose target resolves outside this file. Refs without a
    // resolvable target (no defining entity) are kept too — they are
    // the file's external symbol references.
    let in_file_names: std::collections::HashSet<&str> = idx
        .entities_by_file(query)
        .map(|e| e.name.as_str())
        .collect();
    let mut seen: HashSet<(String, u32, String)> = HashSet::new();
    let top_callees: Vec<Edge> = idx
        .entities_by_file(query)
        .flat_map(|e| idx.refs_from(&e.name))
        .filter(|r| r.file == query)
        .filter(|r| !in_file_names.contains(r.name.as_str()))
        .filter(|r| seen.insert((r.file.clone(), r.line, r.name.clone())))
        .map(callee_edge)
        .collect();

    Some(FileContext {
        q: query.to_string(),
        file: query.to_string(),
        entities,
        top_callers: Some(top_callers),
        top_callees: Some(top_callees),
    })
}

/// Output of [`render_no_match`]: which stream the caller should print
/// to. Keeps the CLI thin and lets unit tests assert routing without
/// capturing process streams.
#[derive(Debug)]
pub enum NoMatchOutput {
    Stdout(String),
    Stderr(String),
}

pub fn render_no_match(nm: &NoMatch, format: ContextFormat, pretty: bool) -> NoMatchOutput {
    match format {
        ContextFormat::Agent => NoMatchOutput::Stdout(
            serde_json::to_string(nm).expect("NoMatch serializes infallibly"),
        ),
        ContextFormat::Full => {
            let json = if pretty {
                serde_json::to_string_pretty(nm)
            } else {
                serde_json::to_string(nm)
            }
            .expect("NoMatch serializes infallibly");
            NoMatchOutput::Stdout(json)
        }
        ContextFormat::Markdown => NoMatchOutput::Stderr(format!(
            "no entity matches `{}`\nhint: try `sigil search {}` to find similar symbols",
            nm.q, nm.q
        )),
    }
}

/// Primary entry point. Pure over `Index`.
pub fn build_context(idx: &Index, query: &str, opts: &ContextOptions) -> Option<Context> {
    let resolved = resolve(idx, query);
    let resolved: Vec<_> = if opts.exclude_tests {
        resolved
            .into_iter()
            .filter(|e| !crate::entity::is_test_path(&e.file))
            .collect()
    } else {
        resolved
    };

    // Heritage-aware fallback: when a `Parent::name` query fails direct
    // resolution, try walking the parent class's `extend`/`implement`/
    // `trait_impl` chain looking for a class that defines `name`. This
    // lets `Flask::testing` succeed even when `testing` lives on `App`
    // (Flask's superclass) — the issue #38 Option-2 path.
    let mut resolved_via_heritage: Option<String> = None;
    let resolved_vec: Vec<&Entity> = if resolved.is_empty() {
        match resolve_via_heritage(idx, query, opts) {
            Some((entity, ancestor)) => {
                resolved_via_heritage = Some(ancestor);
                vec![entity]
            }
            None => return None,
        }
    } else {
        resolved
    };

    let chosen_entity = resolved_vec.first()?;
    let chosen = SymbolRef::from_entity(chosen_entity);
    let alternatives: Vec<SymbolRef> = resolved_vec
        .iter()
        .skip(1)
        .take(4) // cap alt list — more than 4 is rarely helpful, often noise
        .map(|e| SymbolRef::from_entity(e))
        .collect();

    let depth = opts.depth.max(1);

    // Callers — refs whose target is this name. Dedup by (file, line) since a
    // symbol can be called twice on the same line (e.g. chained calls).
    let mut seen: HashSet<(String, u32)> = HashSet::new();
    let callers_all: Vec<&Reference> = idx
        .refs_to(&chosen.name)
        .filter(|r| seen.insert((r.file.clone(), r.line)))
        .filter(|r| !opts.exclude_tests || !crate::entity::is_test_path(&r.file))
        .collect();
    let callers: Vec<Edge> = callers_all
        .iter()
        .take(depth)
        .map(|r| caller_edge(r))
        .collect();
    let skipped_callers = callers_all.len().saturating_sub(callers.len());

    // Callees — refs whose `caller` is this symbol's name. Split into real
    // callees (call / instantiation) vs related types (type_annotation) so
    // the agent sees the distinction without post-processing.
    let mut seen: HashSet<(String, u32, String)> = HashSet::new();
    let from_self: Vec<&Reference> = idx
        .refs_from(&chosen.name)
        .filter(|r| seen.insert((r.file.clone(), r.line, r.name.clone())))
        .collect();

    let (type_refs, call_refs): (Vec<&&Reference>, Vec<&&Reference>) = from_self
        .iter()
        .partition(|r| r.ref_kind == "type_annotation");

    let callees: Vec<Edge> = call_refs
        .iter()
        .take(depth)
        .map(|r| callee_edge(r))
        .collect();
    let skipped_callees = call_refs.len().saturating_sub(callees.len());

    let related_types: Vec<Edge> = type_refs
        .iter()
        .take(depth)
        .map(|r| callee_edge(r))
        .collect();
    let skipped_types = type_refs.len().saturating_sub(related_types.len());

    // Heritage parents: resolve each `extend`/`implement`/`trait_impl`
    // edge on the chosen entity to a defining class entity. Lets an
    // agent see `class Flask(App):` → App without a second tool call.
    let parents = resolve_parents(idx, chosen_entity);

    // Inheritance delta: when the chosen symbol is a method (has a
    // parent class), find other classes that define a method with the
    // same tail segment. Cap at 5 so we don't blow the budget.
    let (overrides, skipped_overrides) = find_overrides(idx, chosen_entity, opts);

    let body = if opts.with_body {
        read_entity_body(&opts.project_root, chosen_entity)
    } else {
        None
    };

    let (resolved_via, ancestor) = match resolved_via_heritage {
        Some(a) => (Some("heritage".to_string()), Some(a)),
        None => (None, None),
    };
    let mut ctx = Context {
        query: query.to_string(),
        chosen,
        body,
        alternatives,
        callers,
        callees,
        related_types,
        parents,
        overrides,
        skipped_callers,
        skipped_callees,
        skipped_types,
        skipped_overrides,
        resolved_via,
        ancestor,
        estimated_tokens: 0,
    };

    // Budget enforcement: render, estimate, trim back-half lists if over.
    enforce_budget(&mut ctx, opts);

    Some(ctx)
}

/// Tail segment of a qualified name — last `::`- or `.`-separated piece.
fn tail_segment(name: &str) -> &str {
    name.rsplit(|c| c == ':' || c == '.').next().unwrap_or(name)
}

/// Look up the class entity for a bare or qualified class name.
/// Prefers class-shaped kinds over imports, mirroring `resolve_parents`.
fn lookup_class<'a>(idx: &'a Index, name: &str) -> Option<&'a Entity> {
    let tail = tail_segment(name);
    let needles = if tail == name {
        vec![name.to_string()]
    } else {
        vec![name.to_string(), tail.to_string()]
    };
    for needle in needles {
        if let Some(e) = idx
            .entities_by_name(&needle)
            .filter(|e| e.kind != "import")
            .find(|e| {
                matches!(
                    e.kind.as_str(),
                    "class" | "struct" | "interface" | "trait" | "enum"
                )
            })
        {
            return Some(e);
        }
    }
    None
}

/// Heritage-aware fallback for `Parent::name` queries that didn't
/// resolve directly. Walks the parent class's `extend`/`implement`/
/// `trait_impl`/`embed` edges (BFS, depth-bounded) looking for an
/// ancestor that defines `name` as a child. Returns the matched entity
/// plus the ancestor's bare name where it was found.
///
/// Only fires when `query` parses as `Parent::name` (or its file-
/// qualified forms `file::Parent::name`). Other query shapes return
/// None — heritage walking doesn't apply to bare-name or
/// file-only queries.
fn resolve_via_heritage<'a>(
    idx: &'a Index,
    query: &str,
    opts: &ContextOptions,
) -> Option<(&'a Entity, String)> {
    let (file_hint, parent_hint, name) = split_query(query);
    let parent_name = parent_hint?;

    let start = lookup_class(idx, parent_name)?;
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(start.name.clone());
    let mut frontier: Vec<&Entity> = vec![start];

    // Bound the walk — pathological inheritance chains are rare, but a
    // shallow cap keeps the worst case predictable.
    for _depth in 0..16 {
        let mut next_frontier: Vec<&Entity> = Vec::new();
        for cls in &frontier {
            for edge in &cls.heritage {
                if !matches!(
                    edge.kind.as_str(),
                    "extend" | "implement" | "trait_impl" | "embed"
                ) {
                    continue;
                }
                let Some(parent_cls) = lookup_class(idx, &edge.target) else {
                    continue;
                };
                if !visited.insert(parent_cls.name.clone()) {
                    continue;
                }
                // Look for `name` as a child of this ancestor.
                let needle = parent_cls.name.as_str();
                let hit = idx
                    .entities_by_name(name)
                    .filter(|e| e.kind != "import")
                    .filter(|e| match file_hint {
                        Some(f) => e.file == f || e.file.ends_with(f),
                        None => true,
                    })
                    .filter(|e| {
                        e.parent.as_deref() == Some(needle)
                            && (!opts.exclude_tests || !crate::entity::is_test_path(&e.file))
                    })
                    .next();
                if let Some(e) = hit {
                    // Use the file-qualified form
                    // `<file>::<ClassName>` per issue #38 spec — gives
                    // the agent a click-to-query handle on the
                    // ancestor.
                    let qualified =
                        format!("{}::{}", parent_cls.file, parent_cls.name);
                    return Some((e, qualified));
                }
                next_frontier.push(parent_cls);
            }
        }
        if next_frontier.is_empty() {
            break;
        }
        frontier = next_frontier;
    }
    None
}

/// Resolve `chosen`'s heritage edges (extend / implement / trait_impl /
/// embed) to defining entities in the index. Each edge's `target` is
/// looked up via `entities_by_name` — which indexes by both the
/// qualified name and the bare leaf — so `target = "App"` and
/// `target = "flask.sansio.app.App"` both reach the same definition.
/// Duplicates by (file, name) are filtered (a class may extend
/// multiple targets that resolve to overlapping definitions); class
/// definitions are preferred over imports.
fn resolve_parents(idx: &Index, chosen: &Entity) -> Vec<SymbolRef> {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut out: Vec<SymbolRef> = Vec::new();
    for edge in &chosen.heritage {
        // Try qualified target first, then fall back to its tail segment.
        let needles = {
            let tail = tail_segment(&edge.target);
            if tail == edge.target.as_str() {
                vec![edge.target.clone()]
            } else {
                vec![edge.target.clone(), tail.to_string()]
            }
        };
        for needle in needles {
            // Prefer class-shaped definitions; skip imports so
            // `class Flask(App):` doesn't resolve to a `from … import App`.
            let candidate = idx
                .entities_by_name(&needle)
                .filter(|e| e.kind != "import")
                .find(|e| {
                    matches!(
                        e.kind.as_str(),
                        "class" | "struct" | "interface" | "trait" | "enum"
                    )
                });
            if let Some(e) = candidate
                && seen.insert((e.file.clone(), e.name.clone()))
            {
                out.push(SymbolRef::from_entity(e));
                break;
            }
        }
    }
    out
}

/// Find other classes in the index that define a method with the same
/// tail segment as `chosen`. Returns (selected, skipped_count).
///
/// A "method with the same tail" is an entity whose:
///   * tail segment equals `chosen`'s tail segment
///   * parent is `Some(...)` AND differs from `chosen`'s parent
///   * kind is `method` / `function` / `fn`
///
/// This lets `sigil context Parameter.get_default` automatically
/// surface `Option.get_default` as an override, so the agent doesn't
/// need a second `sigil where` call to spot inheritance.
fn find_overrides(
    idx: &Index,
    chosen: &Entity,
    opts: &ContextOptions,
) -> (Vec<SymbolRef>, usize) {
    let Some(chosen_parent) = chosen.parent.as_deref() else {
        return (Vec::new(), 0);
    };
    if !matches!(chosen.kind.as_str(), "method" | "function" | "fn") {
        return (Vec::new(), 0);
    }
    let target_tail = tail_segment(&chosen.name);

    let candidates: Vec<&Entity> = idx
        .entities
        .iter()
        .filter(|e| tail_segment(&e.name) == target_tail)
        .filter(|e| matches!(e.kind.as_str(), "method" | "function" | "fn"))
        .filter(|e| {
            e.parent
                .as_deref()
                .map(|p| p != chosen_parent)
                .unwrap_or(false)
        })
        .filter(|e| !opts.exclude_tests || !crate::entity::is_test_path(&e.file))
        .collect();

    // Dedupe by (file, parent) so Python @overload stubs don't surface
    // multiple times.
    use std::collections::HashSet;
    let mut seen: HashSet<(String, Option<String>)> = HashSet::new();
    let mut unique: Vec<&Entity> = Vec::new();
    for e in candidates {
        if seen.insert((e.file.clone(), e.parent.clone())) {
            unique.push(e);
        }
    }

    const MAX_OVERRIDES: usize = 5;
    let total = unique.len();
    let selected: Vec<SymbolRef> = unique
        .into_iter()
        .take(MAX_OVERRIDES)
        .map(SymbolRef::from_entity)
        .collect();
    let skipped = total.saturating_sub(selected.len());
    (selected, skipped)
}

/// Read the 1-indexed inclusive line range `[line_start..=line_end]` of
/// `entity.file` relative to `root`. Returns `None` on any I/O error or
/// if the range overshoots the file — the caller treats a missing body
/// as "no body included", which is strictly better than surfacing half
/// a method.
fn read_entity_body(root: &std::path::Path, entity: &Entity) -> Option<String> {
    let path = root.join(&entity.file);
    let content = std::fs::read_to_string(&path).ok()?;
    let start = entity.line_start.saturating_sub(1) as usize;
    let end = entity.line_end as usize;
    let lines: Vec<&str> = content.lines().collect();
    if start >= lines.len() {
        return None;
    }
    let end = end.min(lines.len());
    if end <= start {
        return None;
    }
    Some(lines[start..end].join("\n"))
}

fn caller_edge(r: &Reference) -> Edge {
    Edge {
        file: r.file.clone(),
        line: r.line,
        symbol: r.caller.clone().unwrap_or_else(|| "<top-level>".to_string()),
        kind: r.ref_kind.clone(),
        caller: r.caller.clone(),
    }
}

fn callee_edge(r: &Reference) -> Edge {
    Edge {
        file: r.file.clone(),
        line: r.line,
        symbol: r.name.clone(),
        kind: r.ref_kind.clone(),
        caller: r.caller.clone(),
    }
}

/// Token estimator — 4 bytes ≈ 1 token, same heuristic `sigil map` uses.
fn estimate_tokens(s: &str) -> usize {
    (s.len() + 3) / 4
}

/// Trim alternatives / callees / related_types / callers (in that order of
/// priority — always preserve the chosen entity and at least one caller) so
/// the rendered output fits within `opts.budget`.
fn enforce_budget(ctx: &mut Context, opts: &ContextOptions) {
    // Markdown is the widest renderer — budget against that form so the
    // other formats (smaller) always fit.
    let mut estimated = estimate_tokens(&render_markdown(ctx));
    if opts.budget == 0 || estimated <= opts.budget {
        ctx.estimated_tokens = estimated;
        return;
    }

    // Drop alternatives first — they're disambiguators, not context.
    while estimated > opts.budget && !ctx.alternatives.is_empty() {
        ctx.alternatives.pop();
        estimated = estimate_tokens(&render_markdown(ctx));
    }

    // Then trim related_types.
    while estimated > opts.budget && !ctx.related_types.is_empty() {
        ctx.related_types.pop();
        ctx.skipped_types += 1;
        estimated = estimate_tokens(&render_markdown(ctx));
    }

    // Then callees.
    while estimated > opts.budget && !ctx.callees.is_empty() {
        ctx.callees.pop();
        ctx.skipped_callees += 1;
        estimated = estimate_tokens(&render_markdown(ctx));
    }

    // Finally callers — but keep at least one. A symbol with no caller
    // context is barely useful; letting the budget drop it entirely would
    // defeat the command's purpose.
    while estimated > opts.budget && ctx.callers.len() > 1 {
        ctx.callers.pop();
        ctx.skipped_callers += 1;
        estimated = estimate_tokens(&render_markdown(ctx));
    }

    ctx.estimated_tokens = estimated;
}

// ──────────────────────────────────────────────────────────────────────────
// Renderers. Markdown is the source of truth for budget estimation since
// it's the largest form; Agent/Full can only be smaller.
// ──────────────────────────────────────────────────────────────────────────

pub fn render_markdown(ctx: &Context) -> String {
    let mut out = String::with_capacity(2048);
    let c = &ctx.chosen;

    out.push_str(&format!("# `{}`\n\n", display_symbol(c)));
    out.push_str(&format!(
        "**{}** in `{}`:{}-{}",
        c.kind, c.file, c.line_start, c.line_end,
    ));
    if let Some(vis) = &c.visibility {
        out.push_str(&format!(" · {}", vis));
    }
    if let Some(br) = &c.blast_radius {
        out.push_str(&format!(
            " · blast {}f/{}c/{}t",
            br.direct_files, br.direct_callers, br.transitive_callers
        ));
    }
    out.push_str("\n\n");

    if let Some(sig) = &c.sig {
        out.push_str("## Signature\n\n");
        out.push_str("```\n");
        out.push_str(sig.trim());
        out.push_str("\n```\n\n");
    }

    // Author-provided description (Python docstring, Rust /// block, godoc).
    // Sits between Signature and Body so the natural reading order is:
    // "what is this signature → what does the author say it does → here
    // are the actual lines (when --with-body is set) → who calls it."
    if let Some(doc) = &c.doc {
        out.push_str("## Doc\n\n");
        out.push_str(doc.trim());
        out.push_str("\n\n");
    }

    if let Some(body) = &ctx.body {
        out.push_str("## Body\n\n");
        out.push_str("```\n");
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("```\n\n");
    }

    if !ctx.overrides.is_empty() {
        out.push_str(&format!(
            "## Overrides ({}{})\n\n",
            ctx.overrides.len(),
            if ctx.skipped_overrides > 0 {
                format!(", +{} more", ctx.skipped_overrides)
            } else {
                String::new()
            },
        ));
        for o in &ctx.overrides {
            let parent = o.parent.as_deref().unwrap_or("<top-level>");
            let name_tail = o.name.rsplit(|ch| ch == ':' || ch == '.').next().unwrap_or(&o.name);
            out.push_str(&format!(
                "- `{parent}.{name_tail}` in `{}`:{}-{}\n",
                o.file, o.line_start, o.line_end
            ));
        }
        out.push_str("\n");
    }

    render_edge_section(
        &mut out,
        "Callers",
        &ctx.callers,
        ctx.skipped_callers,
        /* show_target */ false,
    );
    render_edge_section(
        &mut out,
        "Callees",
        &ctx.callees,
        ctx.skipped_callees,
        /* show_target */ true,
    );
    render_edge_section(
        &mut out,
        "Related types",
        &ctx.related_types,
        ctx.skipped_types,
        /* show_target */ true,
    );

    if !ctx.alternatives.is_empty() {
        out.push_str(&format!(
            "## Ambiguous — {} other match(es)\n\n",
            ctx.alternatives.len()
        ));
        for alt in &ctx.alternatives {
            out.push_str(&format!(
                "- `{}` at `{}`:{}",
                display_symbol(alt),
                alt.file,
                alt.line_start
            ));
            if let Some(br) = &alt.blast_radius {
                out.push_str(&format!(" (blast {}f)", br.direct_files));
            }
            out.push('\n');
        }
        out.push('\n');
    }

    out
}

fn render_edge_section(
    out: &mut String,
    heading: &str,
    edges: &[Edge],
    skipped: usize,
    show_target: bool,
) {
    if edges.is_empty() && skipped == 0 {
        return;
    }
    out.push_str(&format!("## {}", heading));
    if !edges.is_empty() {
        out.push_str(&format!(" ({})", edges.len()));
    }
    out.push_str("\n\n");
    for e in edges {
        if show_target {
            out.push_str(&format!(
                "- `{}` → `{}`  _{}_  `{}:{}`\n",
                e.caller.as_deref().unwrap_or("<top-level>"),
                e.symbol,
                e.kind,
                e.file,
                e.line,
            ));
        } else {
            out.push_str(&format!(
                "- `{}`  _{}_  `{}:{}`\n",
                e.symbol, e.kind, e.file, e.line
            ));
        }
    }
    if skipped > 0 {
        out.push_str(&format!("- _+{} more truncated by budget_\n", skipped));
    }
    out.push('\n');
}

fn display_symbol(s: &SymbolRef) -> String {
    match &s.parent {
        Some(p) => format!("{}::{}", p, s.name),
        None => s.name.clone(),
    }
}

/// Compact short-keyed JSON tuned for LLM token economy. One-line bundle,
/// no whitespace; callers that want pretty output pass `--format json`.
#[derive(Debug, Clone, Serialize)]
struct AgentView<'a> {
    q: &'a str,
    f: &'a str,
    n: &'a str,
    k: &'a str,
    l: [u32; 2],
    #[serde(skip_serializing_if = "Option::is_none")]
    p: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    s: Option<&'a str>,
    /// Author-provided description (Python docstring, Rust ///, godoc).
    /// Distinct from `s` (the literal signature) — short-keyed `d` keeps
    /// it cheap when present, and `skip_serializing_if` keeps the wire
    /// shape unchanged for entities without a doc.
    #[serde(skip_serializing_if = "Option::is_none")]
    d: Option<&'a str>,
    /// Source body, emitted only when the caller asked for `--with-body`.
    #[serde(skip_serializing_if = "Option::is_none")]
    b: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    v: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    br: Option<[u32; 3]>,
    cr: Vec<AgentEdge<'a>>, // callers
    ce: Vec<AgentEdge<'a>>, // callees
    rt: Vec<AgentEdge<'a>>, // related types
    /// Heritage block. Surfaces resolved parent classes so an agent
    /// doesn't have to issue a separate `sigil heritage` call to find
    /// where an inherited member lives. Elided when `parents` is empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    h: Option<AgentHeritage<'a>>,
    /// `"heritage"` when the chosen entity was found by walking the
    /// parent class's inheritance chain. Elided for direct matches.
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_via: Option<&'a str>,
    /// File-qualified handle for the ancestor where the inherited
    /// member lives (e.g. `src/flask/sansio/app.py::App`). Set only
    /// alongside `resolved_via = "heritage"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    ancestor: Option<&'a str>,
    #[serde(skip_serializing_if = "is_zero_skip")]
    sk: [usize; 3], // [callers, callees, types]
}

#[derive(Debug, Clone, Serialize)]
struct AgentHeritage<'a> {
    parents: Vec<AgentSymbolRef<'a>>,
}

#[derive(Debug, Clone, Serialize)]
struct AgentSymbolRef<'a> {
    f: &'a str,
    n: &'a str,
    k: &'a str,
    l: [u32; 2],
}

fn is_zero_skip(s: &[usize; 3]) -> bool {
    s.iter().all(|x| *x == 0)
}

#[derive(Debug, Clone, Serialize)]
struct AgentEdge<'a> {
    f: &'a str,
    l: u32,
    s: &'a str, // symbol
    k: &'a str, // kind
}

pub fn render_agent_json(ctx: &Context) -> String {
    fn edge<'a>(e: &'a Edge) -> AgentEdge<'a> {
        AgentEdge {
            f: &e.file,
            l: e.line,
            s: &e.symbol,
            k: &e.kind,
        }
    }
    let br = ctx
        .chosen
        .blast_radius
        .as_ref()
        .map(|b| [b.direct_callers, b.direct_files, b.transitive_callers]);
    let h = if ctx.parents.is_empty() {
        None
    } else {
        Some(AgentHeritage {
            parents: ctx
                .parents
                .iter()
                .map(|p| AgentSymbolRef {
                    f: &p.file,
                    n: &p.name,
                    k: &p.kind,
                    l: [p.line_start, p.line_end],
                })
                .collect(),
        })
    };
    let view = AgentView {
        q: &ctx.query,
        f: &ctx.chosen.file,
        n: &ctx.chosen.name,
        k: &ctx.chosen.kind,
        l: [ctx.chosen.line_start, ctx.chosen.line_end],
        p: ctx.chosen.parent.as_deref(),
        s: ctx.chosen.sig.as_deref(),
        d: ctx.chosen.doc.as_deref(),
        b: ctx.body.as_deref(),
        v: ctx.chosen.visibility.as_deref(),
        br,
        cr: ctx.callers.iter().map(edge).collect(),
        ce: ctx.callees.iter().map(edge).collect(),
        rt: ctx.related_types.iter().map(edge).collect(),
        h,
        resolved_via: ctx.resolved_via.as_deref(),
        ancestor: ctx.ancestor.as_deref(),
        sk: [ctx.skipped_callers, ctx.skipped_callees, ctx.skipped_types],
    };
    serde_json::to_string(&view).expect("AgentView serializes infallibly")
}

pub fn render_full_json(ctx: &Context, pretty: bool) -> String {
    if pretty {
        serde_json::to_string_pretty(ctx).expect("Context serializes infallibly")
    } else {
        serde_json::to_string(ctx).expect("Context serializes infallibly")
    }
}

/// Compact short-keyed view of [`FileContext`]. Mirrors the per-symbol
/// `AgentView` shape but for a whole file digest: `kind="file"`, the
/// per-entity rows use `n`/`k`/`l`/`v`/`d` keys, and `l` is rendered
/// as a `[start, end]` tuple to halve the per-row JSON overhead.
#[derive(Debug, Clone, Serialize)]
struct FileAgentView<'a> {
    q: &'a str,
    kind: &'static str,
    f: &'a str,
    entities: Vec<FileAgentEntity<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_callers: Option<Vec<AgentEdge<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_callees: Option<Vec<AgentEdge<'a>>>,
}

#[derive(Debug, Clone, Serialize)]
struct FileAgentEntity<'a> {
    n: &'a str,
    k: &'a str,
    l: [u32; 2],
    #[serde(skip_serializing_if = "Option::is_none")]
    v: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    d: Option<&'a str>,
}

pub fn render_file_agent_json(fc: &FileContext) -> String {
    fn edge<'a>(e: &'a Edge) -> AgentEdge<'a> {
        AgentEdge {
            f: &e.file,
            l: e.line,
            s: &e.symbol,
            k: &e.kind,
        }
    }
    let view = FileAgentView {
        q: &fc.q,
        kind: "file",
        f: &fc.file,
        entities: fc
            .entities
            .iter()
            .map(|e| FileAgentEntity {
                n: &e.name,
                k: &e.kind,
                l: [e.line_start, e.line_end],
                v: e.visibility.as_deref(),
                d: e.doc.as_deref(),
            })
            .collect(),
        top_callers: fc
            .top_callers
            .as_ref()
            .map(|edges| edges.iter().map(edge).collect()),
        top_callees: fc
            .top_callees
            .as_ref()
            .map(|edges| edges.iter().map(edge).collect()),
    };
    serde_json::to_string(&view).expect("FileAgentView serializes infallibly")
}

pub fn render_file_full_json(fc: &FileContext, pretty: bool) -> String {
    if pretty {
        serde_json::to_string_pretty(fc).expect("FileContext serializes infallibly")
    } else {
        serde_json::to_string(fc).expect("FileContext serializes infallibly")
    }
}

pub fn render_file_markdown(fc: &FileContext) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", fc.file));
    if !fc.entities.is_empty() {
        out.push_str("## Symbols\n\n");
        for e in &fc.entities {
            out.push_str(&format!(
                "- `{}` ({}) @ L{}-{}",
                e.name, e.kind, e.line_start, e.line_end
            ));
            if let Some(v) = &e.visibility {
                out.push_str(&format!(" [{}]", v));
            }
            out.push('\n');
            if let Some(d) = &e.doc {
                let trimmed = d.lines().next().unwrap_or("").trim();
                if !trimmed.is_empty() {
                    out.push_str(&format!("  > {}\n", trimmed));
                }
            }
        }
        out.push('\n');
    }
    if let Some(callers) = &fc.top_callers
        && !callers.is_empty()
    {
        out.push_str("## Top callers\n\n");
        for e in callers {
            out.push_str(&format!("- `{}` @ {}:{}\n", e.symbol, e.file, e.line));
        }
        out.push('\n');
    }
    if let Some(callees) = &fc.top_callees
        && !callees.is_empty()
    {
        out.push_str("## Top callees\n\n");
        for e in callees {
            out.push_str(&format!("- `{}` @ {}:{}\n", e.symbol, e.file, e.line));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{BlastRadius, Entity, Reference};
    use crate::query::index::Index;

    fn ent_full(
        file: &str,
        name: &str,
        kind: &str,
        parent: Option<&str>,
        sig: Option<&str>,
        visibility: Option<&str>,
        blast_files: u32,
    ) -> Entity {
        Entity {
            file: file.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            line_start: 10,
            line_end: 20,
            parent: parent.map(str::to_string),
            qualified_name: None,
            sig: sig.map(str::to_string),
            meta: None,
            body_hash: None,
            sig_hash: None,
            struct_hash: "deadbeef".to_string(),
            visibility: visibility.map(str::to_string),
            rank: None,
            blast_radius: Some(BlastRadius {
                direct_callers: blast_files * 2,
                direct_files: blast_files,
                transitive_callers: blast_files * 5,
            }),
            doc: None,
            heritage: Vec::new(),
            alias: None,        }
    }

    fn refr(file: &str, caller: Option<&str>, name: &str, kind: &str, line: u32) -> Reference {
        Reference {
            file: file.to_string(),
            caller: caller.map(str::to_string),
            name: name.to_string(),
            ref_kind: kind.to_string(),
            line,
            confidence: None,
            callee_id: None,
        }
    }

    #[test]
    fn split_query_forms() {
        assert_eq!(split_query("foo"), (None, None, "foo"));
        assert_eq!(split_query("Foo::bar"), (None, Some("Foo"), "bar"));
        assert_eq!(
            split_query("src/x.rs::bar"),
            (Some("src/x.rs"), None, "bar")
        );
        assert_eq!(
            split_query("src/x.rs::Foo::bar"),
            (Some("src/x.rs"), Some("Foo"), "bar")
        );
    }

    #[test]
    fn resolve_returns_highest_blast_first() {
        let idx = Index::build(
            vec![
                ent_full("a.rs", "Config", "struct", None, None, None, 1),
                ent_full("b.rs", "Config", "struct", None, None, None, 5), // louder
                ent_full("c.rs", "Config", "struct", None, None, None, 3),
            ],
            vec![],
        );
        let matches = resolve(&idx, "Config");
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].file, "b.rs", "highest direct_files first");
        assert_eq!(matches[1].file, "c.rs");
        assert_eq!(matches[2].file, "a.rs");
    }

    #[test]
    fn resolve_with_file_hint_filters_candidates() {
        let idx = Index::build(
            vec![
                ent_full("a.rs", "Config", "struct", None, None, None, 1),
                ent_full("src/x.rs", "Config", "struct", None, None, None, 5),
            ],
            vec![],
        );
        let matches = resolve(&idx, "src/x.rs::Config");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].file, "src/x.rs");
    }

    #[test]
    fn resolve_with_parent_hint_filters_candidates() {
        let idx = Index::build(
            vec![
                ent_full("a.rs", "new", "function", Some("Foo"), None, None, 3),
                ent_full("a.rs", "new", "function", Some("Bar"), None, None, 5),
                ent_full("a.rs", "new", "function", None, None, None, 1),
            ],
            vec![],
        );
        let matches = resolve(&idx, "Foo::new");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].parent.as_deref(), Some("Foo"));
    }

    #[test]
    fn resolve_skips_imports() {
        let idx = Index::build(
            vec![
                ent_full("a.rs", "Config", "import", None, None, None, 0),
                ent_full("b.rs", "Config", "struct", None, None, None, 5),
            ],
            vec![],
        );
        let matches = resolve(&idx, "Config");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].kind, "struct");
    }

    #[test]
    fn build_context_populates_callers_and_callees() {
        let idx = Index::build(
            vec![ent_full(
                "a.rs",
                "process",
                "function",
                None,
                Some("fn process(x: T) -> R"),
                Some("public"),
                3,
            )],
            vec![
                refr("b.rs", Some("main"), "process", "call", 1),
                refr("c.rs", Some("wrapper"), "process", "call", 2),
                refr("a.rs", Some("process"), "T", "type_annotation", 1),
                refr("a.rs", Some("process"), "helper", "call", 3),
            ],
        );
        let ctx = build_context(&idx, "process", &ContextOptions { budget: 0, depth: 10, format: ContextFormat::Markdown, exclude_tests: false, ..ContextOptions::default() }).unwrap();
        assert_eq!(ctx.chosen.name, "process");
        assert_eq!(ctx.callers.len(), 2);
        assert_eq!(ctx.callees.len(), 1, "only `helper` is a pure callee");
        assert_eq!(ctx.related_types.len(), 1, "`T` is a type_annotation");
        assert_eq!(ctx.related_types[0].symbol, "T");
    }

    #[test]
    fn missing_symbol_returns_none() {
        let idx = Index::build(
            vec![ent_full("a.rs", "foo", "function", None, None, None, 0)],
            vec![],
        );
        assert!(build_context(&idx, "nonexistent", &ContextOptions::default()).is_none());
    }

    #[test]
    fn build_file_context_returns_top_level_outline_when_file_matches() {
        let idx = Index::build(
            vec![
                ent_full("src/foo.rs", "Foo", "struct", None, None, None, 0),
                ent_full("src/foo.rs", "helper", "function", None, None, None, 0),
                // Method on Foo — not top-level outline; should be excluded.
                ent_full("src/foo.rs", "method", "method", Some("Foo"), None, None, 0),
                // Different file — should be excluded.
                ent_full("src/bar.rs", "Bar", "struct", None, None, None, 0),
            ],
            vec![],
        );
        let fc = build_file_context(&idx, "src/foo.rs").expect("file in index");
        assert_eq!(fc.q, "src/foo.rs");
        assert_eq!(fc.file, "src/foo.rs");
        let names: Vec<&str> = fc.entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"Foo"), "missing Foo in {:?}", names);
        assert!(names.contains(&"helper"), "missing helper in {:?}", names);
        assert!(!names.contains(&"method"), "method should not appear as top-level");
        assert!(!names.contains(&"Bar"), "different file should not appear");
    }

    #[test]
    fn build_file_context_aggregates_top_callers_from_other_files() {
        // Two top-level entities defined in src/foo.rs (`Foo`, `helper`).
        // External callers from src/main.rs reference both. The
        // file-context should aggregate them under `top_callers`.
        let idx = Index::build(
            vec![
                ent_full("src/foo.rs", "Foo", "struct", None, None, None, 0),
                ent_full("src/foo.rs", "helper", "function", None, None, None, 0),
            ],
            vec![
                refr("src/main.rs", Some("main"), "Foo", "call", 5),
                refr("src/main.rs", Some("main"), "helper", "call", 6),
                // A self-ref from inside the file — should be excluded.
                refr("src/foo.rs", Some("Foo"), "helper", "call", 2),
            ],
        );
        let fc = build_file_context(&idx, "src/foo.rs").expect("file matches");
        let callers = fc.top_callers.expect("top_callers populated");
        // 2 external refs: main → Foo, main → helper.
        assert_eq!(callers.len(), 2);
        // The self-ref from inside the file is filtered out.
        assert!(
            callers.iter().all(|e| e.file != "src/foo.rs"),
            "self-refs leaked into top_callers: {:?}",
            callers
        );
    }

    #[test]
    fn build_file_context_aggregates_top_callees_from_inside_file() {
        // Foo (in src/foo.rs) calls External.do (in src/ext.rs).
        // top_callees should surface that outbound ref.
        let idx = Index::build(
            vec![
                ent_full("src/foo.rs", "Foo", "struct", None, None, None, 0),
                ent_full("src/ext.rs", "External", "function", None, None, None, 0),
            ],
            vec![
                // Outbound call from Foo to External — should be picked up.
                refr("src/foo.rs", Some("Foo"), "External", "call", 10),
                // Self-call inside foo.rs — should be filtered out (target
                // resolves to an entity in the same file).
                refr("src/foo.rs", Some("Foo"), "Foo", "call", 11),
            ],
        );
        let fc = build_file_context(&idx, "src/foo.rs").expect("file matches");
        let callees = fc.top_callees.expect("top_callees populated");
        assert_eq!(callees.len(), 1);
        assert_eq!(callees[0].symbol, "External");
    }

    #[test]
    fn render_file_agent_json_uses_short_keys_per_issue_37_spec() {
        let fc = FileContext {
            q: "src/foo.rs".to_string(),
            file: "src/foo.rs".to_string(),
            entities: vec![FileEntity {
                name: "Foo".to_string(),
                kind: "struct".to_string(),
                line_start: 10,
                line_end: 50,
                visibility: Some("public".to_string()),
                doc: Some("A foo".to_string()),
            }],
            top_callers: Some(vec![]),
            top_callees: Some(vec![]),
        };
        let out = render_file_agent_json(&fc);
        // Compact, no newlines.
        assert!(!out.contains('\n'));
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(v["q"], "src/foo.rs");
        assert_eq!(v["kind"], "file");
        assert_eq!(v["f"], "src/foo.rs");
        assert_eq!(v["entities"][0]["n"], "Foo");
        assert_eq!(v["entities"][0]["k"], "struct");
        // `l` is a [start, end] tuple per the issue spec.
        assert_eq!(v["entities"][0]["l"][0], 10);
        assert_eq!(v["entities"][0]["l"][1], 50);
        assert_eq!(v["entities"][0]["v"], "public");
        assert_eq!(v["entities"][0]["d"], "A foo");
    }

    #[test]
    fn render_file_full_json_serializes_long_keys() {
        let fc = FileContext {
            q: "src/foo.rs".to_string(),
            file: "src/foo.rs".to_string(),
            entities: vec![FileEntity {
                name: "Foo".to_string(),
                kind: "struct".to_string(),
                line_start: 10,
                line_end: 50,
                visibility: None,
                doc: None,
            }],
            top_callers: Some(vec![]),
            top_callees: Some(vec![]),
        };
        let out = render_file_full_json(&fc, false);
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        // Long-form keys for the unabridged JSON view.
        assert_eq!(v["q"], "src/foo.rs");
        assert_eq!(v["file"], "src/foo.rs");
        assert_eq!(v["entities"][0]["name"], "Foo");
        assert_eq!(v["entities"][0]["line_start"], 10);
        assert_eq!(v["entities"][0]["line_end"], 50);
    }

    #[test]
    fn render_file_markdown_includes_file_path_and_entity_list() {
        let fc = FileContext {
            q: "src/foo.rs".to_string(),
            file: "src/foo.rs".to_string(),
            entities: vec![
                FileEntity {
                    name: "Foo".to_string(),
                    kind: "struct".to_string(),
                    line_start: 10,
                    line_end: 50,
                    visibility: None,
                    doc: None,
                },
                FileEntity {
                    name: "helper".to_string(),
                    kind: "function".to_string(),
                    line_start: 60,
                    line_end: 70,
                    visibility: None,
                    doc: None,
                },
            ],
            top_callers: Some(vec![]),
            top_callees: Some(vec![]),
        };
        let md = render_file_markdown(&fc);
        assert!(md.contains("src/foo.rs"));
        assert!(md.contains("Foo"));
        assert!(md.contains("helper"));
        assert!(md.contains("struct"));
        assert!(md.contains("function"));
    }

    #[test]
    fn build_file_context_returns_none_when_file_absent() {
        let idx = Index::build(
            vec![ent_full("src/foo.rs", "Foo", "struct", None, None, None, 0)],
            vec![],
        );
        assert!(build_file_context(&idx, "src/nonexistent.rs").is_none());
    }

    #[test]
    fn render_no_match_agent_emits_compact_json_on_stdout() {
        let nm = NoMatch {
            q: "fooz".to_string(),
            resolved: false,
            reason: "no entity matches `fooz`".to_string(),
            candidates: vec![Candidate {
                f: "src/a.rs".to_string(),
                n: "foo".to_string(),
                k: "function".to_string(),
                l: 12,
            }],
        };
        let out = render_no_match(&nm, ContextFormat::Agent, false);
        let body = match out {
            NoMatchOutput::Stdout(s) => s,
            NoMatchOutput::Stderr(_) => panic!("agent format must emit on stdout"),
        };
        // Compact JSON: no newlines or pretty indent.
        assert!(!body.contains('\n'), "agent format should be compact");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["q"], "fooz");
        assert_eq!(v["resolved"], false);
        assert_eq!(v["reason"], "no entity matches `fooz`");
        assert_eq!(v["candidates"][0]["n"], "foo");
        assert_eq!(v["candidates"][0]["f"], "src/a.rs");
        assert_eq!(v["candidates"][0]["k"], "function");
        assert_eq!(v["candidates"][0]["l"], 12);
    }

    #[test]
    fn render_no_match_full_pretty_emits_pretty_json_on_stdout() {
        let nm = NoMatch {
            q: "fooz".to_string(),
            resolved: false,
            reason: "no entity matches `fooz`".to_string(),
            candidates: vec![],
        };
        let out = render_no_match(&nm, ContextFormat::Full, true);
        let body = match out {
            NoMatchOutput::Stdout(s) => s,
            NoMatchOutput::Stderr(_) => panic!("json format must emit on stdout"),
        };
        assert!(body.contains('\n'), "pretty JSON should span multiple lines");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["q"], "fooz");
    }

    #[test]
    fn render_no_match_markdown_emits_stderr_text() {
        let nm = NoMatch {
            q: "fooz".to_string(),
            resolved: false,
            reason: "no entity matches `fooz`".to_string(),
            candidates: vec![],
        };
        let out = render_no_match(&nm, ContextFormat::Markdown, false);
        let body = match out {
            NoMatchOutput::Stderr(s) => s,
            NoMatchOutput::Stdout(_) => panic!("markdown format must emit on stderr"),
        };
        assert!(body.contains("no entity matches"));
        assert!(body.contains("fooz"));
    }

    #[test]
    fn build_no_match_falls_back_to_suggest_similar_on_typo() {
        // "proess_dta" has no substring overlap with "process_data" but
        // is within edit-distance bounds. search() returns empty; the
        // fallback should fill candidates by name-similarity.
        let idx = Index::build(
            vec![ent_full(
                "src/lib.rs",
                "process_data",
                "function",
                None,
                None,
                None,
                0,
            )],
            vec![],
        );
        let direct = idx.search("proess_dta", crate::query::index::Scope::All, None, None, 10);
        assert!(direct.is_empty(), "precondition: search must miss");

        let nm = build_no_match(&idx, "proess_dta");
        assert!(
            nm.candidates.iter().any(|c| c.n == "process_data"),
            "expected typo fallback to surface `process_data`, got: {:?}",
            nm.candidates.iter().map(|c| &c.n).collect::<Vec<_>>()
        );
    }

    #[test]
    fn build_no_match_surfaces_file_matches_with_basename() {
        let idx = Index::build(
            vec![ent_full(
                "src/auth_helpers.rs",
                "something_else",
                "function",
                None,
                None,
                None,
                0,
            )],
            vec![],
        );
        let nm = build_no_match(&idx, "auth_helpers");
        let file_hits: Vec<_> = nm.candidates.iter().filter(|c| c.k == "file").collect();
        assert_eq!(file_hits.len(), 1);
        assert_eq!(file_hits[0].f, "src/auth_helpers.rs");
        assert_eq!(file_hits[0].n, "auth_helpers.rs", "n is the basename");
        assert_eq!(file_hits[0].l, 0);
    }

    #[test]
    fn render_agent_json_surfaces_parents_under_h_key() {
        let mut subclass = ent_full(
            "src/sub.rs",
            "Subclass",
            "class",
            None,
            None,
            None,
            0,
        );
        subclass.heritage = vec![crate::entity::HeritageEdge {
            kind: "extend".to_string(),
            target: "Superclass".to_string(),
        }];
        let superclass = ent_full(
            "src/super.rs",
            "Superclass",
            "class",
            None,
            None,
            None,
            0,
        );
        let idx = Index::build(vec![subclass, superclass], vec![]);
        let ctx = build_context(&idx, "Subclass", &ContextOptions::default()).expect("resolves");
        let agent = render_agent_json(&ctx);
        let v: serde_json::Value = serde_json::from_str(&agent).expect("valid JSON");
        let parents = v["h"]["parents"].as_array().expect("h.parents present");
        assert_eq!(parents.len(), 1);
        assert_eq!(parents[0]["n"], "Superclass");
        assert_eq!(parents[0]["f"], "src/super.rs");
        assert_eq!(parents[0]["k"], "class");
        // Issue #38 spec: l is a [start, end] tuple.
        assert_eq!(parents[0]["l"][0], 10);
        assert_eq!(parents[0]["l"][1], 20);
    }

    #[test]
    fn render_agent_json_omits_h_for_entity_without_parents() {
        let idx = Index::build(
            vec![ent_full("a.rs", "Foo", "class", None, None, None, 0)],
            vec![],
        );
        let ctx = build_context(&idx, "Foo", &ContextOptions::default()).expect("resolves");
        let agent = render_agent_json(&ctx);
        let v: serde_json::Value = serde_json::from_str(&agent).expect("valid JSON");
        assert!(v.get("h").is_none(), "h should be omitted when no parents");
    }

    #[test]
    fn render_agent_json_surfaces_resolved_via_heritage() {
        let mut subclass = ent_full(
            "src/sub.rs",
            "Subclass",
            "class",
            None,
            None,
            None,
            0,
        );
        subclass.heritage = vec![crate::entity::HeritageEdge {
            kind: "extend".to_string(),
            target: "Superclass".to_string(),
        }];
        let superclass = ent_full(
            "src/super.rs",
            "Superclass",
            "class",
            None,
            None,
            None,
            0,
        );
        let testing = ent_full(
            "src/super.rs",
            "testing",
            "variable",
            Some("Superclass"),
            None,
            None,
            0,
        );
        let idx = Index::build(vec![subclass, superclass, testing], vec![]);
        let ctx = build_context(&idx, "Subclass::testing", &ContextOptions::default())
            .expect("heritage resolution");
        let agent = render_agent_json(&ctx);
        let v: serde_json::Value = serde_json::from_str(&agent).expect("valid JSON");
        assert_eq!(v["resolved_via"], "heritage");
        assert_eq!(v["ancestor"], "src/super.rs::Superclass");
    }

    #[test]
    fn build_context_resolves_subclass_member_via_heritage() {
        // Superclass defines `testing`. Subclass extends Superclass but
        // does NOT define `testing` itself. Querying
        // `Subclass::testing` should walk the heritage edge to
        // Superclass and return its `testing` member with the marker
        // `resolved_via = "heritage"` and `ancestor = "Superclass"`.
        let mut subclass = ent_full(
            "src/sub.rs",
            "Subclass",
            "class",
            None,
            None,
            None,
            0,
        );
        subclass.heritage = vec![crate::entity::HeritageEdge {
            kind: "extend".to_string(),
            target: "Superclass".to_string(),
        }];
        let superclass = ent_full(
            "src/super.rs",
            "Superclass",
            "class",
            None,
            None,
            None,
            0,
        );
        let testing = ent_full(
            "src/super.rs",
            "testing",
            "variable",
            Some("Superclass"),
            None,
            None,
            0,
        );
        let idx = Index::build(vec![subclass, superclass, testing], vec![]);

        let ctx = build_context(&idx, "Subclass::testing", &ContextOptions::default())
            .expect("heritage-aware resolution must succeed");
        assert_eq!(ctx.chosen.name, "testing");
        assert_eq!(ctx.chosen.file, "src/super.rs");
        assert_eq!(ctx.resolved_via.as_deref(), Some("heritage"));
        assert_eq!(ctx.ancestor.as_deref(), Some("src/super.rs::Superclass"));
    }

    #[test]
    fn build_context_populates_parents_from_heritage_edges() {
        // Subclass extends Superclass. The bundle for Subclass should
        // surface Superclass under `parents` so an agent doesn't have
        // to issue a separate `sigil heritage` call to discover it.
        let mut subclass = ent_full(
            "src/sub.rs",
            "Subclass",
            "class",
            None,
            None,
            None,
            0,
        );
        subclass.heritage = vec![crate::entity::HeritageEdge {
            kind: "extend".to_string(),
            target: "Superclass".to_string(),
        }];
        let superclass = ent_full(
            "src/super.rs",
            "Superclass",
            "class",
            None,
            None,
            None,
            0,
        );
        let idx = Index::build(vec![subclass, superclass], vec![]);
        let ctx = build_context(&idx, "Subclass", &ContextOptions::default()).expect("resolves");
        assert_eq!(ctx.parents.len(), 1, "expected one parent");
        let p = &ctx.parents[0];
        assert_eq!(p.name, "Superclass");
        assert_eq!(p.file, "src/super.rs");
        assert_eq!(p.kind, "class");
    }

    #[test]
    fn build_no_match_returns_substring_match_as_candidate() {
        let idx = Index::build(
            vec![ent_full(
                "src/lib.rs",
                "process_data",
                "function",
                None,
                None,
                None,
                0,
            )],
            vec![],
        );
        let nm = build_no_match(&idx, "process");
        assert_eq!(nm.q, "process");
        assert!(!nm.resolved);
        assert!(!nm.reason.is_empty(), "reason should be a human-readable string");
        assert_eq!(nm.candidates.len(), 1);
        let c = &nm.candidates[0];
        assert_eq!(c.n, "process_data");
        assert_eq!(c.f, "src/lib.rs");
        assert_eq!(c.k, "function");
        assert_eq!(c.l, 10);
    }

    #[test]
    fn alternatives_populated_when_ambiguous() {
        let idx = Index::build(
            vec![
                ent_full("a.rs", "Config", "struct", None, None, None, 5),
                ent_full("b.rs", "Config", "struct", None, None, None, 3),
                ent_full("c.rs", "Config", "struct", None, None, None, 1),
            ],
            vec![],
        );
        let ctx = build_context(&idx, "Config", &ContextOptions { budget: 0, depth: 10, format: ContextFormat::Markdown, exclude_tests: false, ..ContextOptions::default() }).unwrap();
        assert_eq!(ctx.chosen.file, "a.rs");
        assert_eq!(ctx.alternatives.len(), 2);
    }

    #[test]
    fn depth_caps_each_section() {
        let idx = Index::build(
            vec![ent_full("a.rs", "foo", "function", None, None, None, 0)],
            (0..20)
                .flat_map(|i| {
                    vec![
                        refr(&format!("f{i}.rs"), Some("m"), "foo", "call", i as u32),
                        refr("a.rs", Some("foo"), &format!("cb{i}"), "call", i as u32),
                        refr("a.rs", Some("foo"), &format!("T{i}"), "type_annotation", i as u32),
                    ]
                })
                .collect(),
        );
        let ctx = build_context(&idx, "foo", &ContextOptions { budget: 0, depth: 3, format: ContextFormat::Markdown, exclude_tests: false, ..ContextOptions::default() }).unwrap();
        assert_eq!(ctx.callers.len(), 3);
        assert_eq!(ctx.callees.len(), 3);
        assert_eq!(ctx.related_types.len(), 3);
        assert_eq!(ctx.skipped_callers, 17);
        assert_eq!(ctx.skipped_callees, 17);
        assert_eq!(ctx.skipped_types, 17);
    }

    #[test]
    fn budget_trims_but_keeps_chosen_and_one_caller() {
        let idx = Index::build(
            vec![ent_full("a.rs", "foo", "function", None, Some("fn foo()"), None, 0)],
            (0..50)
                .flat_map(|i| {
                    vec![
                        refr(&format!("f{i}.rs"), Some("m"), "foo", "call", i as u32),
                        refr("a.rs", Some("foo"), &format!("cb{i}"), "call", i as u32),
                    ]
                })
                .collect(),
        );
        // Absurdly small budget — implementation must keep at least 1 caller.
        let ctx = build_context(&idx, "foo", &ContextOptions { budget: 50, depth: 50, format: ContextFormat::Markdown, exclude_tests: false, ..ContextOptions::default() }).unwrap();
        assert_eq!(ctx.chosen.name, "foo");
        assert!(ctx.callers.len() >= 1);
        assert!(ctx.callees.is_empty() || ctx.callees.len() < 50);
        assert!(ctx.skipped_callers > 0 || ctx.skipped_callees > 0);
    }

    #[test]
    fn render_markdown_has_expected_sections() {
        let idx = Index::build(
            vec![ent_full(
                "a.rs",
                "foo",
                "function",
                None,
                Some("fn foo(x: T) -> R"),
                Some("public"),
                2,
            )],
            vec![
                refr("b.rs", Some("main"), "foo", "call", 42),
                refr("a.rs", Some("foo"), "T", "type_annotation", 1),
                refr("a.rs", Some("foo"), "helper", "call", 2),
            ],
        );
        let ctx = build_context(&idx, "foo", &ContextOptions { budget: 0, depth: 10, format: ContextFormat::Markdown, exclude_tests: false, ..ContextOptions::default() }).unwrap();
        let md = render_markdown(&ctx);
        assert!(md.starts_with("# `foo`"));
        assert!(md.contains("## Signature"));
        assert!(md.contains("fn foo(x: T) -> R"));
        assert!(md.contains("## Callers"));
        assert!(md.contains("## Callees"));
        assert!(md.contains("## Related types"));
        assert!(md.contains("public"));
        assert!(md.contains("b.rs"));
    }

    #[test]
    fn context_renders_doc_section_when_entity_has_docstring() {
        // Issue #12: the docstring is the cleanest "what does X do" signal
        // a source provides; agents should see it without a follow-up read.
        let mut e = ent_full(
            "repomap.py",
            "tags_cache_error",
            "function",
            None,
            Some("def tags_cache_error(self, original_error=None):"),
            Some("public"),
            1,
        );
        e.doc = Some(
            "Handle SQLite errors by trying to recreate cache, falling back to dict if needed"
                .to_string(),
        );
        let idx = Index::build(vec![e], vec![]);
        let ctx = build_context(
            &idx,
            "tags_cache_error",
            &ContextOptions {
                budget: 0,
                depth: 10,
                format: ContextFormat::Markdown,
                exclude_tests: false,
                ..ContextOptions::default()
            },
        )
        .unwrap();
        let md = render_markdown(&ctx);
        assert!(md.contains("## Doc"), "Doc section missing: {md}");
        assert!(
            md.contains("Handle SQLite errors"),
            "doc body missing: {md}"
        );
        // And the agent JSON exposes it as `d`.
        let agent = render_agent_json(&ctx);
        assert!(agent.contains("\"d\":"), "agent view missing `d` key: {agent}");
    }

    #[test]
    fn context_for_constant_includes_literal_value_in_signature() {
        // Regression: `code.context RETRY_TIMEOUT` must surface the value
        // (sig text) so downstream consumers don't have to do a follow-up
        // file read just to learn that RETRY_TIMEOUT == 60.
        let idx = Index::build(
            vec![ent_full(
                "config.py",
                "RETRY_TIMEOUT",
                "constant",
                None,
                Some("60"),
                Some("public"),
                1,
            )],
            vec![],
        );
        let ctx = build_context(
            &idx,
            "RETRY_TIMEOUT",
            &ContextOptions {
                budget: 0,
                depth: 10,
                format: ContextFormat::Markdown,
                exclude_tests: false,
                ..ContextOptions::default()
            },
        )
        .unwrap();
        let md = render_markdown(&ctx);
        assert!(md.contains("## Signature"), "signature block missing: {md}");
        assert!(
            md.contains("60"),
            "literal value 60 missing from constant context: {md}"
        );
    }

    #[test]
    fn render_agent_json_is_compact_and_short_keyed() {
        // Use a non-trivial fixture so the comparison against markdown is
        // meaningful. At 10+ edges markdown's per-bullet prose overhead
        // exceeds the JSON structure cost.
        let idx = Index::build(
            vec![ent_full(
                "a.rs",
                "foo",
                "function",
                None,
                Some("pub fn foo(x: Input, cfg: Config) -> Result<Output, Error>"),
                Some("public"),
                5,
            )],
            (0..10)
                .flat_map(|i| {
                    vec![
                        refr(&format!("callers/c{i}.rs"), Some("main_caller"), "foo", "call", i as u32 + 1),
                        refr("a.rs", Some("foo"), &format!("callee_{i}"), "call", i as u32 + 50),
                        refr("a.rs", Some("foo"), &format!("Type{i}"), "type_annotation", i as u32 + 100),
                    ]
                })
                .collect(),
        );
        let ctx = build_context(
            &idx,
            "foo",
            &ContextOptions { budget: 0, depth: 10, format: ContextFormat::Agent, exclude_tests: false, ..ContextOptions::default() },
        )
        .unwrap();
        let agent = render_agent_json(&ctx);
        let markdown = render_markdown(&ctx);

        // Structural properties that actually matter for agent ingestion:
        //   - single-line (fits cleanly into a tool-result slot)
        //   - short, stable keys (so tokens-per-key doesn't explode)
        //   - no long human-readable prose (markdown headings, etc.)
        //
        // Byte count vs markdown isn't a useful invariant — at modest
        // fixture sizes JSON structure overhead (quoted keys + commas)
        // roughly matches markdown bullet + backtick overhead, and the
        // winner flips depending on string lengths.
        assert!(!agent.contains('\n'), "agent format must be single-line");
        assert!(!agent.contains("## "), "agent format must not contain markdown headings");
        assert!(agent.contains("\"q\":"));
        assert!(agent.contains("\"cr\":"));
        assert!(agent.contains("\"ce\":"));
        assert!(agent.contains("\"rt\":"));
        // Sanity: the rendered agent JSON actually parses.
        let _: serde_json::Value = serde_json::from_str(&agent).expect("agent JSON must parse");
        // Keep `markdown` referenced so the fixture stays useful if a future
        // invariant uses it again.
        let _ = markdown.len();
    }

    #[test]
    fn with_body_includes_source_lines() {
        // Write a real file so read_entity_body has something to read.
        // Use a per-test temp subdir so parallel test runs don't clobber it.
        let tmp = std::env::temp_dir().join(format!(
            "sigil-context-with-body-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&tmp).expect("mkdir temp");
        let file_path = tmp.join("a.rs");
        std::fs::write(
            &file_path,
            "// L1\nfn foo() {\n    answer();\n}\n// L5\n",
        )
        .expect("write fixture");

        let mut e = ent_full("a.rs", "foo", "function", None, Some("fn foo()"), Some("public"), 0);
        // Lines 2..=4 of the fixture file = the fn body.
        e.line_start = 2;
        e.line_end = 4;
        let idx = Index::build(vec![e], vec![]);

        let opts = ContextOptions {
            budget: 0,
            depth: 10,
            format: ContextFormat::Markdown,
            exclude_tests: false,
            with_body: true,
            project_root: tmp.clone(),
        };
        let ctx = build_context(&idx, "foo", &opts).expect("resolves");
        let body = ctx.body.as_deref().expect("body populated when --with-body set");
        assert!(body.contains("fn foo()"));
        assert!(body.contains("answer();"));
        assert!(!body.contains("L5"), "body must not leak lines past line_end");

        let md = render_markdown(&ctx);
        assert!(md.contains("## Body"), "markdown renderer emits Body section");
        assert!(md.contains("fn foo()"));

        let agent = render_agent_json(&ctx);
        assert!(agent.contains("\"b\":"), "agent view emits b field with body");
    }

    #[test]
    fn with_body_off_by_default() {
        let idx = Index::build(
            vec![ent_full("a.rs", "foo", "function", None, Some("fn foo()"), None, 0)],
            vec![],
        );
        let ctx = build_context(&idx, "foo", &ContextOptions::default()).expect("resolves");
        assert!(ctx.body.is_none(), "body is None unless --with-body is set");
        let md = render_markdown(&ctx);
        assert!(!md.contains("## Body"), "markdown omits Body section by default");
    }

    #[test]
    fn with_body_missing_file_degrades_gracefully() {
        let mut e = ent_full("does-not-exist.rs", "foo", "function", None, None, None, 0);
        e.line_start = 1;
        e.line_end = 5;
        let idx = Index::build(vec![e], vec![]);
        let opts = ContextOptions {
            with_body: true,
            project_root: std::path::PathBuf::from("/nonexistent/root"),
            ..ContextOptions::default()
        };
        let ctx = build_context(&idx, "foo", &opts).expect("resolves even without body");
        assert!(ctx.body.is_none(), "missing file = body None, not error");
    }

    #[test]
    fn format_parse_covers_known_values() {
        assert_eq!(ContextFormat::parse("agent"), Some(ContextFormat::Agent));
        assert_eq!(ContextFormat::parse("markdown"), Some(ContextFormat::Markdown));
        assert_eq!(ContextFormat::parse("md"), Some(ContextFormat::Markdown));
        assert_eq!(ContextFormat::parse("json"), Some(ContextFormat::Full));
        assert_eq!(ContextFormat::parse("full"), Some(ContextFormat::Full));
        assert_eq!(ContextFormat::parse("nonsense"), None);
    }
}
