//! `sigil where <symbol>` — single-shot locator.
//!
//! Consolidates the common SWE-bench-Lite phase-1 flow of "find the
//! definition(s) of this name" into one command with a bundled answer.
//! The same question today takes an agent: `sigil search foo` (list
//! many hits) → `read_file` the indicated line range (find the class) →
//! optionally another grep/search to verify siblings. `sigil where` does
//! all of that at index-time and returns one record per definition.
//!
//! Matching rule: the last `::` / `.`-separated segment of an entity
//! name equals the queried symbol. This lets `sigil where get_default`
//! match `Parameter.get_default` and `Option.get_default` but NOT
//! `CliRunner.get_default_prog_name`. A substring search is too noisy
//! for a "locator"-shaped command.
//!
//! Results are ordered by file-rank desc (most-imported files first),
//! which on real repos surfaces the framework-level definition ahead of
//! one-off helper copies. Without rank info the order falls back to the
//! index's stable (file, line) order.

use serde::Serialize;

use crate::entity::Entity;
use crate::query::index::Index;

/// Kinds that count as "a place where something is defined." Variables and
/// imports stay excluded — a freshly-bound local or an import alias isn't
/// what a `sigil where` consumer is usually after. **Constants** are
/// included: load-bearing module-level tunables (`RETRY_TIMEOUT = 60`,
/// `ANTHROPIC_BETA_HEADER = "..."`) carry the same "where is X" question
/// shape as functions, and downstream tools (Knova wiki renderer, agent
/// context-packs) need to be able to resolve them.
const DEFINITION_KINDS: &[&str] = &[
    "class",
    "struct",
    "enum",
    "trait",
    "interface",
    "function",
    "fn",
    "method",
    "type_alias",
    "module",
    "constant",
];

/// Default cap on rows returned — tuned to keep the common case one-glance
/// readable and the payload under ~1 KB. `sigil where --limit 0` lifts
/// the cap.
pub const DEFAULT_LIMIT: usize = 10;

/// Filters applied before ranking/limiting. All fields `None` = no filter.
#[derive(Debug, Clone, Default)]
pub struct WhereFilters {
    /// Exact match on the enclosing class/module. Use "" to require no
    /// parent (top-level definitions only).
    pub parent: Option<String>,
    /// Case-sensitive substring match on the file path. Useful when many
    /// hits are scattered across a monorepo.
    pub file: Option<String>,
    /// Include definitions under typical test paths. Off by default —
    /// test files dilute a "find the implementation" answer.
    pub include_tests: bool,
}

/// One definition surfaced by `sigil where`. Compact shape that maps
/// directly to the JSON row emitted on `--json`.
///
/// The `name` field is the entity's **tail segment only** (e.g.
/// `get_default`, not `Parameter.get_default`). The qualifying class
/// lives in `parent`; duplicating it in `name` just wastes bytes on
/// every row. Consumers that want the full qualified name can join
/// `parent` + `.` + `name` when parent is non-null.
#[derive(Debug, Clone, Serialize)]
pub struct Definition {
    pub name: String,
    pub file: String,
    pub line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_end: Option<u32>,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
    /// File-level PageRank score — the ranking key. Emitted so consumers
    /// can re-sort or filter client-side without losing the signal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rank: Option<f64>,
    /// Number of same-(name, parent, file, kind) entities — captures
    /// Python `@overload` stubs where the signature repeats.
    #[serde(skip_serializing_if = "is_one")]
    pub overloads: u32,
    /// Whether the defining file lives under a typical test path.
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub in_test: bool,
}

fn is_one(n: &u32) -> bool {
    *n == 1
}

/// Full locator report — the symbol asked for + each matching definition,
/// plus truncation metadata for the agent-facing "narrow the search" hint.
#[derive(Debug, Clone, Serialize)]
pub struct WhereReport {
    pub symbol: String,
    pub definitions: Vec<Definition>,
    /// Total number of deduped definitions matched before the limit was
    /// applied. Equal to `definitions.len()` when not truncated.
    pub total_matches: u32,
    /// True iff a limit was applied and hid rows. Consumers (and the
    /// Markdown renderer) use this to show a "… N more, rerun with
    /// --limit 0" affordance.
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub truncated: bool,
}

/// Last `::`- or `.`-separated segment of an entity name.
fn tail_segment(name: &str) -> &str {
    name.rsplit(|c| c == ':' || c == '.').next().unwrap_or(name)
}

/// Heuristic for "this file is test code" — mirrors `is_test_path` from
/// `entity.rs` but at the path level. Used for the `in_test` flag and
/// optional test-file filtering.
fn is_test_file(file: &str) -> bool {
    crate::entity::is_test_path(file)
}

/// Find definitions of `symbol`. `limit` of 0 or `usize::MAX` returns all
/// rows; any other value caps the output and sets `truncated = true` when
/// rows were hidden.
pub fn find_definitions(
    idx: &Index,
    symbol: &str,
    filters: &WhereFilters,
    limit: usize,
) -> WhereReport {
    // Collect every entity whose tail segment matches and whose kind is
    // a definition-kind. `Index::entities` is sorted by (file, line_start)
    // already, so iteration preserves a stable on-disk order.
    let mut matches: Vec<&Entity> = idx
        .entities
        .iter()
        .filter(|e| tail_segment(&e.name) == symbol)
        .filter(|e| DEFINITION_KINDS.contains(&e.kind.as_str()))
        .collect();

    if !filters.include_tests {
        matches.retain(|e| !is_test_file(&e.file));
    }
    if let Some(parent) = filters.parent.as_deref() {
        // Empty string → require no parent (top-level only). Non-empty →
        // exact match against either the raw parent or its tail segment,
        // so `--parent ModelChoiceField` works even when the index stores
        // the parent as `django.forms.models.ModelChoiceField`.
        if parent.is_empty() {
            matches.retain(|e| e.parent.is_none());
        } else {
            matches.retain(|e| match e.parent.as_deref() {
                Some(p) => p == parent || tail_segment(p) == parent,
                None => false,
            });
        }
    }
    if let Some(needle) = filters.file.as_deref() {
        matches.retain(|e| e.file.contains(needle));
    }

    // Dedupe by (file, parent, kind) — overloads collapse into one
    // `Definition` with `overloads: N`. Keep the earliest line (first
    // seen) as the canonical line for the record. Track the max rank
    // seen in the group (rank is file-level so overloads share it).
    use std::collections::BTreeMap;
    #[allow(clippy::type_complexity)]
    let mut groups: BTreeMap<
        (String, Option<String>, String),
        (u32, u32, Option<String>, u32, bool, String, Option<f64>),
    > = BTreeMap::new();
    let mut order: Vec<(String, Option<String>, String)> = Vec::new();

    for e in matches {
        let key = (e.file.clone(), e.parent.clone(), e.kind.clone());
        groups
            .entry(key.clone())
            .and_modify(|(_, _, _, n, _, _, _)| *n += 1)
            .or_insert_with(|| {
                order.push(key);
                (
                    e.line_start,
                    e.line_end,
                    e.sig.clone(),
                    1,
                    is_test_file(&e.file),
                    e.name.clone(),
                    e.rank,
                )
            });
    }

    let mut definitions: Vec<Definition> = order
        .into_iter()
        .map(|key| {
            let (line_start, line_end, sig, overloads, in_test, full_name, rank) =
                groups[&key].clone();
            let (file, parent, kind) = key;
            let line_end = if line_end != line_start {
                Some(line_end)
            } else {
                None
            };
            let name = tail_segment(&full_name).to_string();
            Definition {
                name,
                file,
                line: line_start,
                line_end,
                kind,
                parent,
                sig,
                rank,
                overloads,
                in_test,
            }
        })
        .collect();

    // Sort by rank desc (None ranks last), tie-break on the original
    // (file, line) order which the Vec already reflects — so we use a
    // stable sort on just the rank key.
    definitions.sort_by(|a, b| {
        let ar = a.rank.unwrap_or(f64::NEG_INFINITY);
        let br = b.rank.unwrap_or(f64::NEG_INFINITY);
        br.partial_cmp(&ar).unwrap_or(std::cmp::Ordering::Equal)
    });

    let total_matches = definitions.len() as u32;
    let effective_limit = if limit == 0 { usize::MAX } else { limit };
    let truncated = definitions.len() > effective_limit;
    if truncated {
        definitions.truncate(effective_limit);
    }

    WhereReport {
        symbol: symbol.to_string(),
        definitions,
        total_matches,
        truncated,
    }
}

/// Agent-facing "narrow the search" hint emitted on stderr when the
/// result set was truncated. Recommends filters from least → most
/// expressive, ending on the SQL escape hatch for cases that don't fit
/// the flag surface.
pub fn narrow_hint(report: &WhereReport) -> Option<String> {
    if !report.truncated {
        return None;
    }
    let shown = report.definitions.len();
    let total = report.total_matches;
    let symbol = &report.symbol;
    Some(format!(
        "sigil: {total} definitions matched, showing top {shown} by rank. \
         Narrow with `--parent CLASS`, `--file PATH_SUBSTR`, or run \
         `sigil where {symbol} --limit 0` for all. \
         Need compound filters? `sigil query \"SELECT file, parent, line_start \
         FROM entities WHERE name = '{symbol}' AND parent = 'CLASS'\"`."
    ))
}

pub fn render_markdown(report: &WhereReport) -> String {
    if report.definitions.is_empty() {
        return format!("No definition of `{}` found.\n", report.symbol);
    }
    let mut out = format!("{}\n", report.symbol);
    for d in &report.definitions {
        let class = d.parent.as_deref().unwrap_or("<top-level>");
        let range = match d.line_end {
            Some(e) => format!("{}:{}-{}", d.file, d.line, e),
            None => format!("{}:{}", d.file, d.line),
        };
        let overload_note = if d.overloads > 1 {
            format!(", {} overloads", d.overloads)
        } else {
            String::new()
        };
        let test_note = if d.in_test { ", test" } else { "" };
        out.push_str(&format!(
            "  {class}.{name}  {range}  ({kind}{overload_note}{test_note})\n",
            name = d.name,
            kind = d.kind,
        ));
        if let Some(sig) = d.sig.as_deref() {
            out.push_str(&format!("    {sig}\n"));
        }
    }
    if report.truncated {
        let hidden = report.total_matches as usize - report.definitions.len();
        out.push_str(&format!(
            "  … {hidden} more (rerun with --limit 0, or filter with --parent/--file)\n"
        ));
    }
    out
}

pub fn render_json(report: &WhereReport, pretty: bool) -> String {
    if pretty {
        serde_json::to_string_pretty(report).expect("WhereReport serializes infallibly")
    } else {
        serde_json::to_string(report).expect("WhereReport serializes infallibly")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{BlastRadius, Entity, Reference};
    use crate::query::index::Index;

    fn ent(file: &str, name: &str, kind: &str, parent: Option<&str>, line: u32) -> Entity {
        Entity {
            file: file.into(),
            name: name.into(),
            kind: kind.into(),
            line_start: line,
            line_end: line + 2,
            parent: parent.map(String::from),
            qualified_name: None,
            sig: Some(format!("sig of {name}")),
            meta: None,
            body_hash: None,
            sig_hash: None,
            struct_hash: "h".into(),
            visibility: Some("public".into()),
            rank: None,
            blast_radius: Some(BlastRadius::default()),
            doc: None,
        }
    }

    fn ent_ranked(
        file: &str,
        name: &str,
        kind: &str,
        parent: Option<&str>,
        line: u32,
        rank: f64,
    ) -> Entity {
        let mut e = ent(file, name, kind, parent, line);
        e.rank = Some(rank);
        e
    }

    #[test]
    fn where_matches_tail_segment_across_parents() {
        let idx = Index::build(
            vec![
                ent("a.py", "Parameter.get_default", "method", Some("Parameter"), 10),
                ent("a.py", "Option.get_default", "method", Some("Option"), 50),
                ent("a.py", "CliRunner.get_default_prog_name", "method", Some("CliRunner"), 100),
            ],
            vec![],
        );
        let report = find_definitions(&idx, "get_default", &WhereFilters::default(), DEFAULT_LIMIT);
        assert_eq!(report.definitions.len(), 2, "only exact tail match, not prefix");
        // With no rank info both fall back to index order.
        assert_eq!(report.definitions[0].parent.as_deref(), Some("Parameter"));
        assert_eq!(report.definitions[0].name, "get_default", "name is tail-only, not qualified");
        assert_eq!(report.definitions[1].parent.as_deref(), Some("Option"));
        assert_eq!(report.definitions[1].name, "get_default");
        assert_eq!(report.total_matches, 2);
        assert!(!report.truncated);
    }

    #[test]
    fn where_orders_by_rank_desc() {
        let idx = Index::build(
            vec![
                ent_ranked("low.py", "A.f", "method", Some("A"), 10, 0.01),
                ent_ranked("hi.py", "B.f", "method", Some("B"), 20, 0.90),
                ent_ranked("mid.py", "C.f", "method", Some("C"), 30, 0.50),
            ],
            vec![],
        );
        let report = find_definitions(&idx, "f", &WhereFilters::default(), DEFAULT_LIMIT);
        assert_eq!(report.definitions[0].file, "hi.py");
        assert_eq!(report.definitions[1].file, "mid.py");
        assert_eq!(report.definitions[2].file, "low.py");
    }

    #[test]
    fn where_filters_by_parent_exact_and_tail() {
        let idx = Index::build(
            vec![
                ent("a.py", "forms.ModelChoiceField.to_python", "method",
                    Some("django.forms.models.ModelChoiceField"), 10),
                ent("a.py", "forms.ChoiceField.to_python", "method",
                    Some("django.forms.fields.ChoiceField"), 50),
                ent("a.py", "forms.IntegerField.to_python", "method",
                    Some("django.forms.fields.IntegerField"), 100),
            ],
            vec![],
        );
        let filt = WhereFilters {
            parent: Some("ModelChoiceField".into()),
            ..WhereFilters::default()
        };
        let r = find_definitions(&idx, "to_python", &filt, DEFAULT_LIMIT);
        assert_eq!(r.definitions.len(), 1);
        assert_eq!(r.definitions[0].parent.as_deref(),
                   Some("django.forms.models.ModelChoiceField"));

        let filt_full = WhereFilters {
            parent: Some("django.forms.fields.ChoiceField".into()),
            ..WhereFilters::default()
        };
        let r = find_definitions(&idx, "to_python", &filt_full, DEFAULT_LIMIT);
        assert_eq!(r.definitions.len(), 1);
    }

    #[test]
    fn where_filter_parent_empty_means_top_level() {
        let idx = Index::build(
            vec![
                ent("a.py", "foo", "function", None, 10),
                ent("a.py", "C.foo", "method", Some("C"), 30),
            ],
            vec![],
        );
        let filt = WhereFilters {
            parent: Some(String::new()),
            ..WhereFilters::default()
        };
        let r = find_definitions(&idx, "foo", &filt, DEFAULT_LIMIT);
        assert_eq!(r.definitions.len(), 1);
        assert_eq!(r.definitions[0].kind, "function");
        assert!(r.definitions[0].parent.is_none());
    }

    #[test]
    fn where_filter_file_substring() {
        let idx = Index::build(
            vec![
                ent("django/forms/models.py", "A.to_python", "method", Some("A"), 10),
                ent("django/forms/fields.py", "B.to_python", "method", Some("B"), 10),
            ],
            vec![],
        );
        let filt = WhereFilters {
            file: Some("models.py".into()),
            ..WhereFilters::default()
        };
        let r = find_definitions(&idx, "to_python", &filt, DEFAULT_LIMIT);
        assert_eq!(r.definitions.len(), 1);
        assert_eq!(r.definitions[0].file, "django/forms/models.py");
    }

    #[test]
    fn where_limit_truncates_and_reports_total() {
        let mut ents: Vec<Entity> = (0..15)
            .map(|i| ent_ranked(&format!("f{}.py", i), &format!("C{}.m", i),
                                "method", Some(&format!("C{}", i)), 10, i as f64))
            .collect();
        ents.reverse();
        let idx = Index::build(ents, vec![]);
        let r = find_definitions(&idx, "m", &WhereFilters::default(), 3);
        assert_eq!(r.definitions.len(), 3);
        assert_eq!(r.total_matches, 15);
        assert!(r.truncated);
        // Top-3 by rank: ranks 14, 13, 12.
        assert_eq!(r.definitions[0].file, "f14.py");
        assert_eq!(r.definitions[1].file, "f13.py");
        assert_eq!(r.definitions[2].file, "f12.py");

        let all = find_definitions(&idx, "m", &WhereFilters::default(), 0);
        assert_eq!(all.definitions.len(), 15);
        assert!(!all.truncated);
    }

    #[test]
    fn where_narrow_hint_only_when_truncated() {
        let mut ents: Vec<Entity> = (0..12)
            .map(|i| ent(&format!("f{}.py", i), &format!("C{}.m", i),
                         "method", Some(&format!("C{}", i)), 10))
            .collect();
        ents.reverse();
        let idx = Index::build(ents, vec![]);
        let r = find_definitions(&idx, "m", &WhereFilters::default(), DEFAULT_LIMIT);
        let hint = narrow_hint(&r).expect("expected hint when truncated");
        assert!(hint.contains("--parent"));
        assert!(hint.contains("--file"));
        assert!(hint.contains("sigil query"));

        let r_all = find_definitions(&idx, "m", &WhereFilters::default(), 0);
        assert!(narrow_hint(&r_all).is_none());
    }

    #[test]
    fn where_collapses_python_overloads() {
        let idx = Index::build(
            vec![
                ent("a.py", "P.get_default", "method", Some("P"), 10),
                ent("a.py", "P.get_default", "method", Some("P"), 15),
                ent("a.py", "P.get_default", "method", Some("P"), 20),
            ],
            vec![],
        );
        let report = find_definitions(&idx, "get_default", &WhereFilters::default(), DEFAULT_LIMIT);
        assert_eq!(report.definitions.len(), 1);
        assert_eq!(report.definitions[0].overloads, 3);
        assert_eq!(report.definitions[0].line, 10, "earliest line wins");
    }

    #[test]
    fn where_filters_tests_by_default() {
        let idx = Index::build(
            vec![
                ent("src/core.py", "P.get_default", "method", Some("P"), 10),
                ent("tests/test_core.py", "FakeP.get_default", "method", Some("FakeP"), 30),
            ],
            vec![],
        );
        let default = find_definitions(&idx, "get_default", &WhereFilters::default(), DEFAULT_LIMIT);
        assert_eq!(default.definitions.len(), 1, "test file filtered out by default");
        let with_tests = find_definitions(
            &idx,
            "get_default",
            &WhereFilters { include_tests: true, ..WhereFilters::default() },
            DEFAULT_LIMIT,
        );
        assert_eq!(with_tests.definitions.len(), 2);
        assert!(with_tests.definitions.iter().any(|d| d.in_test));
    }

    #[test]
    fn where_skips_variables_and_imports() {
        let idx = Index::build(
            vec![
                ent("a.py", "foo", "variable", None, 10),
                ent("a.py", "foo", "import", None, 20),
                ent("a.py", "foo", "function", None, 30),
            ],
            vec![],
        );
        let report = find_definitions(&idx, "foo", &WhereFilters::default(), DEFAULT_LIMIT);
        assert_eq!(report.definitions.len(), 1);
        assert_eq!(report.definitions[0].kind, "function");
    }

    #[test]
    fn where_finds_constants() {
        // Module-level constants (e.g. `RETRY_TIMEOUT = 60`) are valid
        // "where is X defined" answers — agents asking about a load-bearing
        // tunable should find it without falling back to a grep.
        let idx = Index::build(
            vec![
                ent("config.py", "RETRY_TIMEOUT", "constant", None, 5),
                ent("other.py", "RETRY_TIMEOUT", "variable", None, 9),
            ],
            vec![],
        );
        let report = find_definitions(
            &idx,
            "RETRY_TIMEOUT",
            &WhereFilters::default(),
            DEFAULT_LIMIT,
        );
        assert_eq!(report.definitions.len(), 1);
        assert_eq!(report.definitions[0].kind, "constant");
        assert_eq!(report.definitions[0].file, "config.py");
    }

    // Silence "unused" warning on Reference — kept here for future
    // call-tracing on `sigil where` (e.g. an "also calls this" block).
    #[allow(dead_code)]
    fn _ref_shape() -> Reference {
        Reference {
            file: "a.py".into(),
            caller: Some("main".into()),
            name: "foo".into(),
            ref_kind: "call".into(),
            line: 1,
        }
    }
}
