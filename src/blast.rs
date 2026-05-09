//! `sigil blast <symbol>` — impact summary for a symbol.
//!
//! Terse output:
//!
//!   Entity — called from 45 sites in 14 files. Transitive: 192 callers.
//!   Top callers by file rank:
//!     src/diff.rs:220     match_classify_enrich → Entity       [type_annotation]
//!     src/classifier.rs:116  is_public → Entity                 [type_annotation]
//!     ...
//!
//! The `blast_radius` field is already populated on every entity by
//! `sigil index`, so this command is a join between the symbol's blast
//! entry and the top-K callers ranked by caller-file PageRank.

use std::collections::HashMap;

use serde::Serialize;

use crate::entity::{BlastRadius, Entity, Reference};
use crate::query::index::Index;
use crate::rank::RankManifest;

#[derive(Debug, Clone)]
pub struct BlastOptions {
    /// How many top callers to surface. 0 = all.
    pub depth: usize,
    pub format: BlastFormat,
    /// Drop test-file callers from the output and from resolution. Default
    /// off — opt-in via `--exclude-tests`.
    pub exclude_tests: bool,
}

impl Default for BlastOptions {
    fn default() -> Self {
        Self {
            depth: 10,
            format: BlastFormat::Markdown,
            exclude_tests: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlastFormat {
    Markdown,
    Json,
    Agent,
}

impl BlastFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "markdown" | "md" => Some(Self::Markdown),
            "json" => Some(Self::Json),
            "agent" => Some(Self::Agent),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BlastReport {
    pub symbol: String,
    pub file: String,
    pub kind: String,
    pub line_start: u32,
    pub line_end: u32,
    pub blast: Option<BlastRadius>,
    pub top_callers: Vec<CallerRow>,
    pub skipped_callers: usize,
    /// Surfaced so disambiguation is visible to the caller.
    pub alternatives: Vec<BlastAlt>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallerRow {
    pub file: String,
    pub line: u32,
    pub kind: String,
    pub caller: Option<String>,
    pub caller_file_rank: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BlastAlt {
    pub file: String,
    pub kind: String,
    pub line_start: u32,
    pub line_end: u32,
    pub parent: Option<String>,
}

/// Run a blast query. None when the symbol doesn't resolve.
pub fn run_blast(
    idx: &Index,
    rank: &RankManifest,
    query: &str,
    opts: &BlastOptions,
) -> Option<BlastReport> {
    // Resolve: matching entities, sorted by blast direct_files desc.
    let mut matches: Vec<&Entity> = idx
        .entities_by_name(query)
        .filter(|e| e.kind != "import")
        .filter(|e| !opts.exclude_tests || !crate::entity::is_test_path(&e.file))
        .collect();
    if matches.is_empty() {
        return None;
    }
    matches.sort_by_key(|e| {
        std::cmp::Reverse(
            e.blast_radius
                .as_ref()
                .map(|b| b.direct_files)
                .unwrap_or(0),
        )
    });
    let chosen = matches[0];
    let alternatives: Vec<BlastAlt> = matches
        .iter()
        .skip(1)
        .take(4)
        .map(|e| BlastAlt {
            file: e.file.clone(),
            kind: e.kind.clone(),
            line_start: e.line_start,
            line_end: e.line_end,
            parent: e.parent.clone(),
        })
        .collect();

    // Join refs targeting `name` with caller-file rank. Sort by rank desc
    // so the most-load-bearing callers surface first.
    let mut callers = rank_sorted_callers(idx, &chosen.name, &rank.file_rank);
    if opts.exclude_tests {
        callers.retain(|r| !crate::entity::is_test_path(&r.file));
    }

    let (top_callers, skipped_callers) = if opts.depth == 0 {
        (callers, 0)
    } else {
        let total = callers.len();
        let kept: Vec<CallerRow> = callers.into_iter().take(opts.depth).collect();
        let skipped = total.saturating_sub(kept.len());
        (kept, skipped)
    };

    Some(BlastReport {
        symbol: chosen.name.clone(),
        file: chosen.file.clone(),
        kind: chosen.kind.clone(),
        line_start: chosen.line_start,
        line_end: chosen.line_end,
        blast: chosen.blast_radius,
        top_callers,
        skipped_callers,
        alternatives,
    })
}

/// Sort the callers of `name` by caller-file PageRank descending. Deduped
/// by (file, line) so a chained call on one line doesn't count twice.
fn rank_sorted_callers(
    idx: &Index,
    name: &str,
    file_rank: &HashMap<String, f64>,
) -> Vec<CallerRow> {
    let mut seen: std::collections::HashSet<(String, u32)> = std::collections::HashSet::new();
    let mut rows: Vec<CallerRow> = idx
        .refs_to(name)
        .filter(|r: &&Reference| seen.insert((r.file.clone(), r.line)))
        .map(|r| CallerRow {
            file: r.file.clone(),
            line: r.line,
            kind: r.ref_kind.clone(),
            caller: r.caller.clone(),
            caller_file_rank: file_rank.get(&r.file).copied().unwrap_or(0.0),
        })
        .collect();
    rows.sort_by(|a, b| {
        b.caller_file_rank
            .partial_cmp(&a.caller_file_rank)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });
    rows
}

pub fn render_markdown(report: &BlastReport) -> String {
    let mut out = String::with_capacity(1024);
    let br = report.blast;
    let impact = br
        .map(|b| {
            format!(
                "{} callers in {} files · transitive {} within depth 3",
                b.direct_callers, b.direct_files, b.transitive_callers
            )
        })
        .unwrap_or_else(|| "no blast data — run `sigil index`".to_string());

    out.push_str(&format!("# `{}`\n\n", report.symbol));
    out.push_str(&format!(
        "**{}** in `{}`:{}-{}\n\n",
        report.kind, report.file, report.line_start, report.line_end
    ));
    out.push_str(&format!("**Impact:** {}\n\n", impact));

    if !report.top_callers.is_empty() {
        out.push_str(&format!(
            "## Top callers by file rank ({})\n\n",
            report.top_callers.len()
        ));
        for row in &report.top_callers {
            let caller = row.caller.as_deref().unwrap_or("<top-level>");
            out.push_str(&format!(
                "- `{}:{}`  `{}` → `{}`  _{}_  (file rank {:.4})\n",
                row.file, row.line, caller, report.symbol, row.kind, row.caller_file_rank,
            ));
        }
        if report.skipped_callers > 0 {
            out.push_str(&format!("- _+{} more_\n", report.skipped_callers));
        }
        out.push('\n');
    }

    if !report.alternatives.is_empty() {
        out.push_str(&format!(
            "## Other `{}` definition(s)\n\n",
            report.symbol
        ));
        for alt in &report.alternatives {
            let parent = alt
                .parent
                .as_deref()
                .map(|p| format!(" (in {})", p))
                .unwrap_or_default();
            out.push_str(&format!(
                "- `{}`:{}  {} {}\n",
                alt.file, alt.line_start, alt.kind, parent
            ));
        }
        out.push('\n');
    }

    out
}

pub fn render_json(report: &BlastReport, pretty: bool) -> String {
    if pretty {
        serde_json::to_string_pretty(report).expect("BlastReport serializes infallibly")
    } else {
        serde_json::to_string(report).expect("BlastReport serializes infallibly")
    }
}

/// Compact short-keyed form for LLM ingestion.
pub fn render_agent(report: &BlastReport) -> String {
    #[derive(Serialize)]
    struct AgentEdge<'a> {
        f: &'a str,
        l: u32,
        k: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        c: Option<&'a str>,
        r: f64,
    }
    #[derive(Serialize)]
    struct AgentView<'a> {
        n: &'a str,
        f: &'a str,
        k: &'a str,
        l: [u32; 2],
        #[serde(skip_serializing_if = "Option::is_none")]
        br: Option<[u32; 3]>,
        cr: Vec<AgentEdge<'a>>,
        #[serde(skip_serializing_if = "is_zero")]
        sk: usize,
    }
    fn is_zero(n: &usize) -> bool {
        *n == 0
    }

    let view = AgentView {
        n: &report.symbol,
        f: &report.file,
        k: &report.kind,
        l: [report.line_start, report.line_end],
        br: report
            .blast
            .map(|b| [b.direct_callers, b.direct_files, b.transitive_callers]),
        cr: report
            .top_callers
            .iter()
            .map(|row| AgentEdge {
                f: &row.file,
                l: row.line,
                k: &row.kind,
                c: row.caller.as_deref(),
                r: row.caller_file_rank,
            })
            .collect(),
        sk: report.skipped_callers,
    };
    serde_json::to_string(&view).expect("AgentView serializes infallibly")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{BlastRadius, Entity, Reference};
    use crate::query::index::Index;
    use std::collections::HashMap;

    fn ent(file: &str, name: &str, kind: &str, blast_files: u32, blast_callers: u32) -> Entity {
        Entity {
            file: file.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            line_start: 1,
            line_end: 2,
            parent: None,
            sig: None,
            meta: None,
            body_hash: None,
            sig_hash: None,
            struct_hash: "d".to_string(),
            visibility: None,
            rank: None,
            blast_radius: Some(BlastRadius {
                direct_callers: blast_callers,
                direct_files: blast_files,
                transitive_callers: 0,
            }),
            doc: None,
        }
    }

    fn refr(file: &str, caller: Option<&str>, name: &str, kind: &str, line: u32) -> Reference {
        Reference {
            file: file.to_string(),
            caller: caller.map(str::to_string),
            name: name.to_string(),
            ref_kind: kind.to_string(),
            line,
        }
    }

    fn rank_of(pairs: &[(&str, f64)]) -> RankManifest {
        RankManifest {
            version: "1".to_string(),
            sigil_version: "test".to_string(),
            damping: 0.85,
            iterations_max: 0,
            transitive_depth: 3,
            file_count: pairs.len(),
            file_rank: pairs.iter().map(|(f, r)| (f.to_string(), *r)).collect(),
        }
    }

    #[test]
    fn missing_symbol_returns_none() {
        let idx = Index::default();
        assert!(run_blast(&idx, &rank_of(&[]), "nope", &BlastOptions::default()).is_none());
    }

    #[test]
    fn chooses_highest_blast_match() {
        let idx = Index::build(
            vec![
                ent("a.rs", "Foo", "struct", 1, 2),
                ent("b.rs", "Foo", "struct", 8, 20),
                ent("c.rs", "Foo", "struct", 3, 5),
            ],
            vec![],
        );
        let r = run_blast(&idx, &rank_of(&[]), "Foo", &BlastOptions::default()).unwrap();
        assert_eq!(r.file, "b.rs");
        assert_eq!(r.alternatives.len(), 2);
    }

    #[test]
    fn top_callers_sorted_by_file_rank() {
        let idx = Index::build(
            vec![ent("target.rs", "Foo", "struct", 0, 0)],
            vec![
                refr("low.rs", Some("m"), "Foo", "type_annotation", 1),
                refr("high.rs", Some("m"), "Foo", "type_annotation", 1),
                refr("mid.rs", Some("m"), "Foo", "type_annotation", 1),
            ],
        );
        let r = rank_of(&[
            ("low.rs", 0.01),
            ("high.rs", 0.9),
            ("mid.rs", 0.5),
        ]);
        let report = run_blast(&idx, &r, "Foo", &BlastOptions::default()).unwrap();
        let files: Vec<&str> = report.top_callers.iter().map(|c| c.file.as_str()).collect();
        assert_eq!(files, vec!["high.rs", "mid.rs", "low.rs"]);
    }

    #[test]
    fn depth_caps_callers_and_sets_skipped_count() {
        let idx = Index::build(
            vec![ent("target.rs", "Foo", "struct", 0, 0)],
            (0..20)
                .map(|i| refr(&format!("f{i}.rs"), Some("m"), "Foo", "call", i))
                .collect(),
        );
        let report = run_blast(
            &idx,
            &rank_of(&[]),
            "Foo",
            &BlastOptions { depth: 5, ..BlastOptions::default() },
        )
        .unwrap();
        assert_eq!(report.top_callers.len(), 5);
        assert_eq!(report.skipped_callers, 15);
    }

    #[test]
    fn same_line_callers_deduped() {
        let idx = Index::build(
            vec![ent("target.rs", "Foo", "struct", 0, 0)],
            vec![
                refr("caller.rs", Some("m"), "Foo", "call", 10),
                refr("caller.rs", Some("m"), "Foo", "call", 10), // duplicate
            ],
        );
        let report = run_blast(&idx, &rank_of(&[]), "Foo", &BlastOptions::default()).unwrap();
        assert_eq!(report.top_callers.len(), 1);
    }

    #[test]
    fn markdown_renderer_has_expected_headings() {
        let idx = Index::build(
            vec![ent("a.rs", "Foo", "struct", 3, 5)],
            vec![refr("b.rs", Some("m"), "Foo", "call", 1)],
        );
        let md = render_markdown(
            &run_blast(&idx, &rank_of(&[("b.rs", 0.1)]), "Foo", &BlastOptions::default()).unwrap(),
        );
        assert!(md.starts_with("# `Foo`"));
        assert!(md.contains("**Impact:**"));
        assert!(md.contains("Top callers"));
        assert!(md.contains("b.rs"));
    }

    #[test]
    fn agent_form_is_single_line_short_keys() {
        let idx = Index::build(
            vec![ent("a.rs", "Foo", "struct", 3, 5)],
            vec![refr("b.rs", Some("m"), "Foo", "call", 1)],
        );
        let agent = render_agent(
            &run_blast(&idx, &rank_of(&[("b.rs", 0.1)]), "Foo", &BlastOptions::default()).unwrap(),
        );
        assert!(!agent.contains('\n'));
        assert!(agent.contains("\"n\":"));
        assert!(agent.contains("\"cr\":"));
        let _: serde_json::Value = serde_json::from_str(&agent).expect("agent JSON must parse");
    }

    #[test]
    fn format_parse_covers_known_values() {
        assert_eq!(BlastFormat::parse("markdown"), Some(BlastFormat::Markdown));
        assert_eq!(BlastFormat::parse("md"), Some(BlastFormat::Markdown));
        assert_eq!(BlastFormat::parse("json"), Some(BlastFormat::Json));
        assert_eq!(BlastFormat::parse("agent"), Some(BlastFormat::Agent));
        assert_eq!(BlastFormat::parse("nope"), None);
    }
}
