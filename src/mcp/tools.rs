//! Pure MCP tool handlers — each function takes an `Index` and the
//! tool's typed inputs and returns a `serde_json::Value`. The rmcp
//! `ServerHandler` impl in `super::server` is just a JSON-RPC shim
//! around these.
//!
//! Kept free of any rmcp / async machinery so they're trivially
//! testable in plain `#[test]` functions.

use serde_json::{Value, json};

use crate::context::{
    ContextFormat, ContextOptions, build_context, build_file_context, build_no_match,
    render_agent_json, render_file_agent_json, render_file_full_json, render_full_json,
};
use crate::query::index::{Index, Scope, SearchHit};

/// `sigil_search` — symbol-aware substring search. Returns
/// `{ hits: [...] }`. Each symbol hit carries `f`, `n`, `k`, `l` and
/// optional `parent` / `sig`; each file hit is the same with
/// `k: "file"`, `l: 0`, `n: <basename>`.
pub fn search(idx: &Index, query: &str, limit: usize) -> Value {
    let hits = idx.search(query, Scope::All, None, None, limit);
    let rows: Vec<Value> = hits
        .into_iter()
        .map(|h| match h {
            SearchHit::Symbol(e) => {
                let mut row = json!({
                    "f": e.file,
                    "n": e.name,
                    "k": e.kind,
                    "l": e.line_start,
                });
                if let Some(parent) = &e.parent {
                    row["parent"] = json!(parent);
                }
                if let Some(sig) = &e.sig {
                    row["sig"] = json!(sig);
                }
                row
            }
            SearchHit::File(fh) => {
                let basename = std::path::Path::new(&fh.path)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| fh.path.clone());
                json!({
                    "f": fh.path,
                    "n": basename,
                    "k": "file",
                    "l": 0,
                })
            }
        })
        .collect();
    json!({ "hits": rows })
}

/// Options accepted by [`context`]. The MCP wire layer maps the
/// JSON-schema input to this struct; the pure function takes the
/// typed form so unit tests can construct it directly.
#[derive(Debug, Clone)]
pub struct ContextToolOptions {
    /// Include the symbol's source body in the bundle (maps to
    /// `--with-body` on the CLI).
    pub include_source: bool,
    /// When false, the unabridged JSON shape is returned for each
    /// bundle; default true emits the compact short-keyed Agent view.
    pub compact: bool,
    pub depth: usize,
    pub budget: usize,
}

impl Default for ContextToolOptions {
    fn default() -> Self {
        Self {
            include_source: false,
            compact: true,
            depth: 10,
            budget: 1500,
        }
    }
}

/// `get_context` — per-symbol context bundle. Accepts multiple
/// targets in one call (a single MCP round-trip for the common
/// "tell me about A, B, C" agent pattern). Each entry is the same
/// JSON the CLI would emit under `sigil context --format agent` (or
/// `--format json` when `compact=false`), with one extra wrinkle:
///   * If the target matches a file in the index, the per-file
///     digest from `build_file_context` is returned (issue #37).
///   * If neither matches, the entry is the no-match payload from
///     `build_no_match` (issue #36) so the agent gets candidates back
///     instead of an opaque error.
pub fn context(idx: &Index, targets: &[String], opts: &ContextToolOptions) -> Value {
    let bundles: Vec<Value> = targets
        .iter()
        .map(|q| context_one(idx, q, opts))
        .collect();
    json!({ "bundles": bundles })
}

fn context_one(idx: &Index, query: &str, opts: &ContextToolOptions) -> Value {
    // File path? Return file digest.
    if let Some(fc) = build_file_context(idx, query) {
        let payload = if opts.compact {
            render_file_agent_json(&fc)
        } else {
            render_file_full_json(&fc, false)
        };
        return serde_json::from_str(&payload).unwrap_or(Value::Null);
    }
    let ctx_opts = ContextOptions {
        budget: opts.budget,
        depth: opts.depth,
        format: if opts.compact {
            ContextFormat::Agent
        } else {
            ContextFormat::Full
        },
        exclude_tests: false,
        with_body: opts.include_source,
        project_root: std::path::PathBuf::from("."),
    };
    if let Some(ctx) = build_context(idx, query, &ctx_opts) {
        let payload = if opts.compact {
            render_agent_json(&ctx)
        } else {
            render_full_json(&ctx, false)
        };
        return serde_json::from_str(&payload).unwrap_or(Value::Null);
    }
    let nm = build_no_match(idx, query);
    serde_json::to_value(&nm).unwrap_or(Value::Null)
}

/// `get_why` — architectural decision intelligence. Three modes
/// (mirrors the issue #39 spec):
///   * `Some(query)` where `query` looks like a file path: return
///     decisions whose `file` is that path (or has it as a suffix).
///   * `Some(query)` (free text): return decisions whose `text` (or
///     `marker`) contains the query, case-insensitively.
///   * `None`: return all decisions ordered as `sort_markers` does
///     (file ascending, line ascending — the same shape
///     `sigil decisions` ships).
pub fn why(root: &std::path::Path, query: Option<&str>) -> Value {
    let mut markers = crate::decisions::extract_from_root(root);
    crate::decisions::sort_markers(&mut markers);

    let filtered: Vec<&crate::decisions::DecisionMarker> = match query {
        None => markers.iter().collect(),
        Some(q) => {
            let q_lower = q.to_lowercase();
            // File path filter: query contains `/` or ends in a
            // common source extension → treat as a path filter.
            let looks_like_path = q.contains('/')
                || std::path::Path::new(q)
                    .extension()
                    .map(|e| !e.is_empty())
                    .unwrap_or(false);
            markers
                .iter()
                .filter(|m| {
                    if looks_like_path {
                        m.file == q || m.file.ends_with(q)
                    } else {
                        m.text.to_lowercase().contains(&q_lower)
                            || m.marker.to_lowercase().contains(&q_lower)
                    }
                })
                .collect()
        }
    };
    json!({ "decisions": filtered })
}

/// `get_dead_code` — framework-aware dead-code findings, partitioned
/// by confidence into `safe_to_delete` (>= 0.70 — file-level and
/// exported orphans) and `review_first` (< 0.70 — internal helpers
/// where the false-positive rate is higher).
///
/// Mirrors `sigil dead-code` minus the CLI-level filters. The MCP
/// caller passes `min_confidence` (default 0.4 per the issue spec) and
/// `include_internals` (default false — when true, the search includes
/// the lower-confidence internal-helper tier).
pub fn dead_code(
    root: &std::path::Path,
    idx: &Index,
    min_confidence: f64,
    include_internals: bool,
) -> Value {
    let cfg = crate::dead_code::DeadCodeConfig {
        safe_only: false,
        include_low_confidence: include_internals,
        ..crate::dead_code::DeadCodeConfig::default()
    };
    let mut findings = crate::dead_code::find_dead_code_in_index(root, idx, &cfg);
    findings.retain(|c| c.confidence >= min_confidence);

    let (safe, review): (Vec<_>, Vec<_>) = findings
        .into_iter()
        .partition(|c| c.confidence >= 0.70);
    json!({
        "safe_to_delete": safe,
        "review_first": review,
    })
}

/// `get_overview` — cold-start architecture map. Returns the same
/// shape `sigil map --format json --top-entities 5` produces:
/// ranked files with their top entities, plus subsystems detected by
/// community detection. Pure over `(Index, RankManifest)`.
///
/// Composing hotspots / contracts / package-deps into this tool
/// requires additional disk-loaded artifacts (cochange manifest, git
/// history); they are intentionally left to follow-up tools so the
/// MCP server can serve a useful overview against any indexed repo
/// without pre-running other commands.
pub fn overview(idx: &Index, rank: &crate::rank::RankManifest, budget: usize) -> Value {
    let opts = crate::map::MapOptions {
        tokens: budget,
        top_entities_per_subsystem: 5,
        ..crate::map::MapOptions::default()
    };
    let map = crate::map::build_map(idx, rank, &opts);
    serde_json::to_value(&map).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::Entity;

    fn ent(file: &str, name: &str, kind: &str, line: u32) -> Entity {
        Entity {
            file: file.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            line_start: line,
            line_end: line,
            parent: None,
            qualified_name: None,
            sig: None,
            meta: None,
            body_hash: None,
            sig_hash: None,
            struct_hash: "deadbeef".to_string(),
            visibility: None,
            rank: None,
            blast_radius: None,
            doc: None,
            heritage: Vec::new(),
            alias: None,
        }
    }

    #[test]
    fn search_returns_symbol_hits_with_short_keys() {
        let idx = Index::build(
            vec![ent("src/lib.rs", "process_data", "function", 12)],
            vec![],
        );
        let v = search(&idx, "process", 10);
        let hits = v["hits"].as_array().expect("hits array");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["n"], "process_data");
        assert_eq!(hits[0]["f"], "src/lib.rs");
        assert_eq!(hits[0]["k"], "function");
        assert_eq!(hits[0]["l"], 12);
    }

    #[test]
    fn context_returns_bundle_for_resolved_symbol() {
        let idx = Index::build(
            vec![ent("src/lib.rs", "foo", "function", 12)],
            vec![],
        );
        let v = context(
            &idx,
            &["foo".to_string()],
            &ContextToolOptions::default(),
        );
        let bundles = v["bundles"].as_array().expect("bundles array");
        assert_eq!(bundles.len(), 1);
        assert_eq!(bundles[0]["n"], "foo");
        assert_eq!(bundles[0]["f"], "src/lib.rs");
    }

    #[test]
    fn context_returns_no_match_payload_for_unknown_symbol() {
        let idx = Index::build(
            vec![ent("src/lib.rs", "foo", "function", 12)],
            vec![],
        );
        let v = context(
            &idx,
            &["UnknownSymbol".to_string()],
            &ContextToolOptions::default(),
        );
        let b = &v["bundles"][0];
        assert_eq!(b["resolved"], false);
        assert_eq!(b["q"], "UnknownSymbol");
    }

    #[test]
    fn context_batches_multiple_targets() {
        let idx = Index::build(
            vec![
                ent("a.rs", "foo", "function", 1),
                ent("b.rs", "bar", "function", 1),
            ],
            vec![],
        );
        let v = context(
            &idx,
            &["foo".to_string(), "bar".to_string()],
            &ContextToolOptions::default(),
        );
        let bundles = v["bundles"].as_array().expect("bundles array");
        assert_eq!(bundles.len(), 2);
        assert_eq!(bundles[0]["n"], "foo");
        assert_eq!(bundles[1]["n"], "bar");
    }

    #[test]
    fn why_filters_by_free_text_query() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("notes.py");
        std::fs::write(
            &p,
            "# DECISION: pin to stdio transport\nx = 1\n# WHY: cleanup after fork\ny = 2\n",
        )
        .unwrap();

        let v = why(tmp.path(), Some("stdio"));
        let decisions = v["decisions"].as_array().expect("decisions array");
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0]["marker"], "DECISION");
        assert!(decisions[0]["text"]
            .as_str()
            .unwrap()
            .contains("stdio"));
    }

    #[test]
    fn why_with_no_query_returns_all_decisions() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a.rs");
        std::fs::write(&p, "// DECISION: option A\n// WHY: simpler\n").unwrap();

        let v = why(tmp.path(), None);
        assert_eq!(v["decisions"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn why_filters_by_file_path() {
        let tmp = tempfile::tempdir().unwrap();
        let p1 = tmp.path().join("a.rs");
        let p2 = tmp.path().join("b.rs");
        std::fs::write(&p1, "// DECISION: alpha\n").unwrap();
        std::fs::write(&p2, "// DECISION: beta\n").unwrap();

        let v = why(tmp.path(), Some("a.rs"));
        let decisions = v["decisions"].as_array().expect("decisions array");
        assert_eq!(decisions.len(), 1);
        assert!(decisions[0]["text"].as_str().unwrap().contains("alpha"));
    }

    #[test]
    fn dead_code_partitions_findings_by_confidence_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        // Empty index → no findings; assert the keys exist and arrays
        // are empty. Real-finding behavior is covered by
        // dead_code_integration tests.
        let idx = Index::build(Vec::new(), Vec::new());
        let v = dead_code(tmp.path(), &idx, 0.4, false);
        assert!(v["safe_to_delete"].is_array());
        assert!(v["review_first"].is_array());
    }

    #[test]
    fn overview_returns_map_json_with_files_and_subsystems_keys() {
        let idx = Index::build(
            vec![
                ent("src/a.rs", "Foo", "struct", 1),
                ent("src/b.rs", "Bar", "struct", 1),
            ],
            vec![],
        );
        let rank = crate::rank::RankManifest::default();
        let v = overview(&idx, &rank, 4000);
        // Shape: must look like the existing `sigil map --format json`
        // output — keys an MCP consumer can expect.
        assert!(v["meta"].is_object(), "meta block missing");
        assert!(v["files"].is_array(), "files array missing");
    }

    #[test]
    fn search_respects_limit() {
        let mut entities = Vec::new();
        for i in 0..30u32 {
            entities.push(ent("src/x.rs", &format!("foo{}", i), "function", i + 1));
        }
        let idx = Index::build(entities, vec![]);
        let v = search(&idx, "foo", 5);
        assert_eq!(v["hits"].as_array().unwrap().len(), 5);
    }
}
