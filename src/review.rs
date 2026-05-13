//! `sigil review <refspec>` — PR-ready structural summary.
//!
//! Runs `sigil diff` under the hood, then joins in:
//!   * rank (from `.sigil/rank.json`)
//!   * blast radius (from `.sigil/entities.jsonl` via the Index)
//!   * co-change misses (from `.sigil/cochange.json` built by
//!     `sigil cochange`)
//!
//! and emits a markdown artifact agents + humans read instead of `git
//! diff`. Rank-ordered "most impactful changes" goes first so readers see
//! the important stuff before the bulk deltas.
//!
//! This is a renderer on top of data sigil already owns. No new parsing,
//! no new AST work — just the join.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context as _, Result};

use crate::cochange::{self, CochangeManifest};
use crate::diff::{self, DiffOptions};
use crate::diff_json::{ChangeKind, DiffResult, EntityDiff};
use crate::entity::{BlastRadius, Entity};
use crate::git;
use crate::query::index::Index;

/// CLI-facing knobs.
#[derive(Debug, Clone)]
pub struct ReviewOptions {
    pub format: ReviewFormat,
    pub top_k: usize,
    pub show_cochange: bool,
    pub cochange_min_weight: f64,
    pub cochange_top_per_file: usize,
}

impl Default for ReviewOptions {
    fn default() -> Self {
        Self {
            format: ReviewFormat::Markdown,
            top_k: 5,
            show_cochange: true,
            cochange_min_weight: 0.3,
            cochange_top_per_file: 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewFormat {
    Markdown,
    Json,
}

impl ReviewFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "markdown" | "md" => Some(Self::Markdown),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

/// Enriched per-entity row. Carries the underlying EntityDiff plus the
/// rank / blast we joined on.
#[derive(Debug, Clone)]
pub struct ReviewEntry<'a> {
    pub diff: &'a EntityDiff,
    pub file_rank: f64,
    pub blast: Option<BlastRadius>,
    /// Composite sort key: file_rank * log(1 + blast_files) * breaking_boost.
    pub impact_score: f64,
}

/// "This file changed and historically moves with X, but X did not change."
#[derive(Debug, Clone)]
pub struct CochangeMiss {
    pub file: String,
    pub expected_partner: String,
    pub weight: f64,
    pub support: u32,
}

/// Orchestrate: diff → Index load → rank load → cochange load → render.
pub fn run_review(
    root: &Path,
    refspec: &str,
    opts: &ReviewOptions,
) -> Result<String> {
    // 1. Parse refspec and compute the diff. Use the same DiffOptions
    //    defaults the `sigil diff` CLI does.
    let (base_ref, head_ref) =
        git::parse_ref_spec(refspec).map_err(|e| anyhow::anyhow!("parse refspec: {e}"))?;
    let dopts = DiffOptions {
        include_unchanged: false,
        verbose: false,
        include_context: false,
        context_lines: 0,
    };
    let result = diff::compute_diff(root, &base_ref, &head_ref, &dopts)
        .map_err(|e| anyhow::anyhow!("compute diff: {e}"))?;

    // 2. Load the current-state Index + rank manifest. The index reflects
    //    HEAD (where blast was computed); that's the right pool to query
    //    for post-change impact. Missing artifacts degrade gracefully.
    let idx = Index::load(root).context("load sigil index")?;
    let rank = crate::map::load_rank_manifest(root).context("load rank.json")?;

    let cochange_manifest = if opts.show_cochange {
        cochange::load(root).unwrap_or_default()
    } else {
        CochangeManifest::default()
    };

    let blast_by_key = blast_lookup(&idx);
    let enriched = enrich(&result, &rank.file_rank, &blast_by_key);

    let touched_files: HashSet<String> = result
        .entities
        .iter()
        .flat_map(|d| [d.file.clone(), d.old_file.clone().unwrap_or_default()])
        .filter(|f| !f.is_empty())
        .collect();
    let misses = if opts.show_cochange && !cochange_manifest.pairs.is_empty() {
        find_cochange_misses(&cochange_manifest, &touched_files, opts)
    } else {
        Vec::new()
    };

    match opts.format {
        ReviewFormat::Markdown => Ok(render_markdown(&result, &enriched, &misses, opts)),
        ReviewFormat::Json => render_json(&result, &enriched, &misses),
    }
}

/// Per-entity blast lookup keyed on the HEAD-side (file, name, parent) so
/// we surface impact as it stands in the head ref.
fn blast_lookup(idx: &Index) -> HashMap<(String, String, Option<String>), BlastRadius> {
    idx.entities
        .iter()
        .filter_map(|e: &Entity| {
            e.blast_radius
                .map(|br| ((e.file.clone(), e.name.clone(), e.parent.clone()), br))
        })
        .collect()
}

fn enrich<'a>(
    result: &'a DiffResult,
    file_rank: &HashMap<String, f64>,
    blast: &HashMap<(String, String, Option<String>), BlastRadius>,
) -> Vec<ReviewEntry<'a>> {
    let mut out: Vec<ReviewEntry<'a>> = result
        .entities
        .iter()
        .map(|d| {
            let rank = file_rank.get(&d.file).copied().unwrap_or(0.0);
            let br = d.new.as_ref().and_then(|e| {
                blast.get(&(e.file.clone(), e.name.clone(), e.parent.clone())).copied()
            }).or_else(|| {
                d.old.as_ref().and_then(|e| {
                    blast.get(&(e.file.clone(), e.name.clone(), e.parent.clone())).copied()
                })
            });
            let breaking_boost = if d.breaking { 3.0 } else { 1.0 };
            let files = br.map(|b| (b.direct_files as f64).ln_1p()).unwrap_or(0.0);
            let impact = rank * (files + 1.0) * breaking_boost;
            ReviewEntry {
                diff: d,
                file_rank: rank,
                blast: br,
                impact_score: impact,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.impact_score
            .partial_cmp(&a.impact_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

fn find_cochange_misses(
    manifest: &CochangeManifest,
    touched: &HashSet<String>,
    opts: &ReviewOptions,
) -> Vec<CochangeMiss> {
    let mut misses: Vec<CochangeMiss> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for file in touched {
        let partners = cochange::partners_of(manifest, file);
        for p in partners.iter().take(opts.cochange_top_per_file) {
            if p.weight < opts.cochange_min_weight {
                continue;
            }
            if touched.contains(p.file) {
                continue; // partner also changed — not a miss
            }
            // Dedupe across symmetric pairs so (file=a, partner=b) doesn't
            // also report (file=b, partner=a).
            let key = if file.as_str() < p.file {
                (file.clone(), p.file.to_string())
            } else {
                (p.file.to_string(), file.clone())
            };
            if !seen.insert(key) {
                continue;
            }
            misses.push(CochangeMiss {
                file: file.clone(),
                expected_partner: p.file.to_string(),
                weight: p.weight,
                support: p.support,
            });
        }
    }
    misses.sort_by(|a, b| {
        b.weight
            .partial_cmp(&a.weight)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    misses
}

// ──────────────────────────────────────────────────────────────────────────
// Renderers
// ──────────────────────────────────────────────────────────────────────────

fn is_import_kind(kind: &str) -> bool {
    matches!(kind, "import" | "use" | "package")
}

fn change_tag(k: &ChangeKind) -> &'static str {
    match k {
        ChangeKind::Added => "added",
        ChangeKind::Removed => "removed",
        ChangeKind::Modified => "modified",
        ChangeKind::Moved => "moved",
        ChangeKind::Renamed => "renamed",
        ChangeKind::FormattingOnly => "formatting",
    }
}

fn entry_line(entry: &ReviewEntry<'_>) -> String {
    let d = entry.diff;
    let blast_str = entry
        .blast
        .map(|b| {
            format!(
                " · blast {}f/{}c/{}t",
                b.direct_files, b.direct_callers, b.transitive_callers
            )
        })
        .unwrap_or_default();
    let breaking = if d.breaking { " ⚠ **breaking**" } else { "" };
    let old_name = d
        .old_name
        .as_ref()
        .filter(|o| o.as_str() != d.name)
        .map(|o| format!(" (was `{o}`)"))
        .unwrap_or_default();
    format!(
        "- _{}_ `{}` `{}`{}{} · rank {:.4}{}\n",
        change_tag(&d.change),
        d.kind,
        d.name,
        old_name,
        breaking,
        entry.file_rank,
        blast_str,
    )
}

fn render_markdown(
    result: &DiffResult,
    enriched: &[ReviewEntry<'_>],
    misses: &[CochangeMiss],
    opts: &ReviewOptions,
) -> String {
    let mut out = String::with_capacity(8 * 1024);
    out.push_str(&format!(
        "# Review — `{}..{}`\n\n",
        result.base_ref, result.head_ref
    ));

    let s = &result.summary;
    let total = s.added + s.removed + s.modified + s.moved + s.renamed + s.formatting_only;
    out.push_str(&format!(
        "{} entities changed · {}a / {}m / {}r / {}mv / {}rn / {}fmt",
        total, s.added, s.modified, s.removed, s.moved, s.renamed, s.formatting_only,
    ));
    if s.has_breaking_change {
        out.push_str(" · ⚠ breaking changes present");
    }
    out.push_str("\n\n");

    // Section 1: Most impactful changes (top-K by impact score).
    // Imports are excluded — they carry the blast radius of whatever they
    // pull in (Entity, std types, etc.) and flood the section with what is
    // structurally noise for a human reviewer. Imports still surface in the
    // per-file structural deltas section below.
    let impactful: Vec<&ReviewEntry<'_>> = enriched
        .iter()
        .filter(|e| !is_import_kind(&e.diff.kind))
        .take(opts.top_k)
        .collect();
    if !impactful.is_empty() {
        out.push_str(&format!("## Most impactful ({})\n\n", impactful.len()));
        for entry in &impactful {
            out.push_str(&format!(
                "- `{}` — {} in `{}` · rank {:.4}{}{}\n",
                entry.diff.name,
                change_tag(&entry.diff.change),
                entry.diff.file,
                entry.file_rank,
                entry
                    .blast
                    .map(|b| format!(" · blast {}f/{}c/{}t", b.direct_files, b.direct_callers, b.transitive_callers))
                    .unwrap_or_default(),
                if entry.diff.breaking { " · ⚠ breaking" } else { "" },
            ));
            if let Some(reason) = &entry.diff.breaking_reason {
                out.push_str(&format!("  _{}_\n", reason));
            }
        }
        out.push('\n');
    }

    // Section 2: Per-file structural deltas (all changes, grouped).
    out.push_str("## Structural deltas\n\n");
    let mut by_file: HashMap<String, Vec<&ReviewEntry<'_>>> = HashMap::new();
    for entry in enriched {
        by_file.entry(entry.diff.file.clone()).or_default().push(entry);
    }
    let mut file_order: Vec<(&String, f64)> = by_file
        .iter()
        .map(|(f, es)| {
            let rank = es.first().map(|e| e.file_rank).unwrap_or(0.0);
            (f, rank)
        })
        .collect();
    file_order.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    for (file, rank) in &file_order {
        out.push_str(&format!("### `{}` (rank {:.4})\n\n", file, rank));
        for entry in by_file.get(*file).unwrap() {
            out.push_str(&entry_line(entry));
        }
        out.push('\n');
    }

    // Section 3: Co-change misses.
    if !misses.is_empty() {
        out.push_str(&format!("## Co-change misses ({})\n\n", misses.len()));
        out.push_str(
            "_These files historically change with the files in this PR but did not this time._\n\n",
        );
        for m in misses {
            out.push_str(&format!(
                "- `{}` changed; expected companion `{}` (weight {:.2}, {} historical co-changes)\n",
                m.file, m.expected_partner, m.weight, m.support,
            ));
        }
        out.push('\n');
    }

    // Cross-file patterns section — reuse existing diff output detection.
    if !result.patterns.is_empty() {
        out.push_str(&format!("## Patterns ({})\n\n", result.patterns.len()));
        for p in &result.patterns {
            out.push_str(&format!(
                "- {} across {} file(s): _{}_\n",
                change_tag(&p.change),
                p.files.len(),
                p.description,
            ));
        }
        out.push('\n');
    }

    out
}

fn render_json(
    result: &DiffResult,
    enriched: &[ReviewEntry<'_>],
    misses: &[CochangeMiss],
) -> Result<String> {
    // Inline view type — stable field names, suitable for LLM / script
    // consumption. We don't need lifetime gymnastics: just clone into
    // owned scalars where cheap.
    let entries: Vec<_> = enriched
        .iter()
        .map(|e| {
            serde_json::json!({
                "change": change_tag(&e.diff.change),
                "name": e.diff.name,
                "kind": e.diff.kind,
                "file": e.diff.file,
                "breaking": e.diff.breaking,
                "file_rank": e.file_rank,
                "blast": e.blast.map(|b| serde_json::json!({
                    "direct_callers": b.direct_callers,
                    "direct_files": b.direct_files,
                    "transitive_callers": b.transitive_callers,
                })),
                "impact_score": e.impact_score,
            })
        })
        .collect();
    let misses_json: Vec<_> = misses
        .iter()
        .map(|m| {
            serde_json::json!({
                "file": m.file,
                "expected_partner": m.expected_partner,
                "weight": m.weight,
                "support": m.support,
            })
        })
        .collect();

    let out = serde_json::json!({
        "base_ref": result.base_ref,
        "head_ref": result.head_ref,
        "summary": result.summary,
        "entries": entries,
        "cochange_misses": misses_json,
        "patterns": result.patterns,
    });
    Ok(serde_json::to_string_pretty(&out)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff_json::DiffSummary;
    use crate::entity::{BlastRadius, Entity};

    fn ed(file: &str, name: &str, kind: &str, change: ChangeKind, breaking: bool) -> EntityDiff {
        let ent = Entity {
            file: file.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            line_start: 1,
            line_end: 2,
            parent: None,
            qualified_name: None,
            sig: None,
            meta: None,
            body_hash: None,
            sig_hash: None,
            struct_hash: "d".to_string(),
            visibility: None,
            rank: None,
            blast_radius: None,
            doc: None,
            heritage: Vec::new(),
            alias: None,        };
        EntityDiff {
            change,
            name: name.to_string(),
            kind: kind.to_string(),
            file: file.to_string(),
            old_file: None,
            old_name: None,
            sig_changed: None,
            body_changed: None,
            breaking,
            breaking_reason: None,
            old: Some(ent.clone()),
            new: Some(ent),
            inline_diff: None,
            change_details: None,
        }
    }

    fn result_with(entities: Vec<EntityDiff>) -> DiffResult {
        let has_breaking = entities.iter().any(|e| e.breaking);
        let mut s = DiffSummary {
            added: 0,
            removed: 0,
            modified: 0,
            moved: 0,
            renamed: 0,
            formatting_only: 0,
            has_breaking_change: has_breaking,
        };
        for d in &entities {
            match d.change {
                ChangeKind::Added => s.added += 1,
                ChangeKind::Removed => s.removed += 1,
                ChangeKind::Modified => s.modified += 1,
                ChangeKind::Moved => s.moved += 1,
                ChangeKind::Renamed => s.renamed += 1,
                ChangeKind::FormattingOnly => s.formatting_only += 1,
            }
        }
        DiffResult {
            base_ref: "main".to_string(),
            head_ref: "HEAD".to_string(),
            base_sha: None,
            head_sha: None,
            entities,
            patterns: Vec::new(),
            summary: s,
            old_sources: None,
            new_sources: None,
        }
    }

    #[test]
    fn impact_score_favors_high_rank_high_blast() {
        let r = result_with(vec![
            ed("core/hot.rs", "foo", "function", ChangeKind::Modified, false),
            ed("misc/cold.rs", "bar", "function", ChangeKind::Modified, false),
        ]);
        let rank: HashMap<String, f64> = [
            ("core/hot.rs".to_string(), 0.20),
            ("misc/cold.rs".to_string(), 0.01),
        ]
        .into_iter()
        .collect();
        let mut blast: HashMap<(String, String, Option<String>), BlastRadius> = HashMap::new();
        blast.insert(
            ("core/hot.rs".to_string(), "foo".to_string(), None),
            BlastRadius { direct_callers: 20, direct_files: 15, transitive_callers: 80 },
        );
        blast.insert(
            ("misc/cold.rs".to_string(), "bar".to_string(), None),
            BlastRadius { direct_callers: 1, direct_files: 1, transitive_callers: 1 },
        );
        let enriched = enrich(&r, &rank, &blast);
        assert_eq!(enriched[0].diff.name, "foo", "high-impact wins top slot");
        assert!(enriched[0].impact_score > enriched[1].impact_score);
    }

    #[test]
    fn breaking_changes_get_rank_multiplier() {
        let mut r = result_with(vec![
            ed("a.rs", "public_api", "function", ChangeKind::Modified, true),
            ed("a.rs", "noncrit", "function", ChangeKind::Modified, false),
        ]);
        // Sanity-check the summary before passing through.
        assert!(r.summary.has_breaking_change);
        let rank: HashMap<String, f64> = [("a.rs".to_string(), 0.1)].into_iter().collect();
        let blast: HashMap<(String, String, Option<String>), BlastRadius> = HashMap::new();
        // Force-populate entities with same file so the sort depends purely
        // on the breaking flag × the breaking multiplier.
        for e in &mut r.entities {
            if let Some(n) = &mut e.new {
                n.blast_radius = Some(BlastRadius { direct_callers: 1, direct_files: 1, transitive_callers: 1 });
            }
        }
        let enriched = enrich(&r, &rank, &blast);
        assert_eq!(enriched[0].diff.name, "public_api");
    }

    #[test]
    fn find_cochange_misses_surfaces_unchanged_partner() {
        let manifest = CochangeManifest {
            version: "1".to_string(),
            sigil_version: "test".to_string(),
            commits_scanned: 10,
            min_support: 1,
            file_count: 3,
            pairs: vec![
                cochange::Pair { a: "api/handler.rs".to_string(), b: "api/types.rs".to_string(), weight: 0.8, support: 8 },
                cochange::Pair { a: "api/handler.rs".to_string(), b: "unrelated.rs".to_string(), weight: 0.1, support: 2 },
            ],
        };
        let touched: HashSet<String> = ["api/handler.rs".to_string()].into_iter().collect();
        let opts = ReviewOptions::default();
        let misses = find_cochange_misses(&manifest, &touched, &opts);
        assert_eq!(misses.len(), 1, "only the high-weight partner fires; weak pair below min_weight");
        assert_eq!(misses[0].expected_partner, "api/types.rs");
    }

    #[test]
    fn cochange_miss_skipped_when_partner_also_touched() {
        let manifest = CochangeManifest {
            version: "1".to_string(),
            sigil_version: "test".to_string(),
            commits_scanned: 10,
            min_support: 1,
            file_count: 2,
            pairs: vec![cochange::Pair {
                a: "a.rs".to_string(),
                b: "b.rs".to_string(),
                weight: 0.9,
                support: 9,
            }],
        };
        let touched: HashSet<String> =
            ["a.rs".to_string(), "b.rs".to_string()].into_iter().collect();
        let misses = find_cochange_misses(&manifest, &touched, &ReviewOptions::default());
        assert!(misses.is_empty(), "both partners changed, not a miss");
    }

    #[test]
    fn cochange_miss_dedupes_symmetric_pair() {
        // Two touched files A, B. Partner pair (A,B) present but neither
        // side is a miss (other side is also touched). Different scenario:
        // C is touched, and has partners X and Y neither touched.
        let manifest = CochangeManifest {
            version: "1".to_string(),
            sigil_version: "test".to_string(),
            commits_scanned: 10,
            min_support: 1,
            file_count: 3,
            pairs: vec![
                cochange::Pair { a: "c.rs".to_string(), b: "x.rs".to_string(), weight: 0.7, support: 5 },
            ],
        };
        let touched: HashSet<String> = ["c.rs".to_string()].into_iter().collect();
        let misses = find_cochange_misses(&manifest, &touched, &ReviewOptions::default());
        assert_eq!(misses.len(), 1);
        assert_eq!(misses[0].expected_partner, "x.rs");
    }

    #[test]
    fn render_markdown_has_expected_sections() {
        let r = result_with(vec![ed(
            "a.rs",
            "foo",
            "function",
            ChangeKind::Modified,
            false,
        )]);
        let rank: HashMap<String, f64> = [("a.rs".to_string(), 0.5)].into_iter().collect();
        let blast: HashMap<(String, String, Option<String>), BlastRadius> = HashMap::new();
        let enriched = enrich(&r, &rank, &blast);
        let md = render_markdown(&r, &enriched, &[], &ReviewOptions::default());
        assert!(md.contains("# Review"));
        assert!(md.contains("## Most impactful"));
        assert!(md.contains("## Structural deltas"));
        assert!(md.contains("a.rs"));
        assert!(md.contains("foo"));
    }

    #[test]
    fn review_format_parse_covers_known_values() {
        assert_eq!(ReviewFormat::parse("markdown"), Some(ReviewFormat::Markdown));
        assert_eq!(ReviewFormat::parse("md"), Some(ReviewFormat::Markdown));
        assert_eq!(ReviewFormat::parse("json"), Some(ReviewFormat::Json));
        assert!(ReviewFormat::parse("junk").is_none());
    }
}
