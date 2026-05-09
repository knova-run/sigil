//! `sigil duplicates` — clone detection via `body_hash` collisions.
//!
//! Sigil already computes `body_hash` on every entity during indexing (a
//! normalized content hash — ignores whitespace and formatting). Clone
//! detection is therefore free: group entities by `body_hash` where count
//! > 1, filter by minimum body size, and emit the groups.
//!
//! This is the deterministic alternative to graphify's LLM-inferred
//! `semantically_similar_to` edges for the *code* case. Same signal, zero
//! hallucination, zero model cost.

use std::collections::HashMap;

use serde::Serialize;

use crate::entity::Entity;
use crate::query::index::Index;

#[derive(Debug, Clone)]
pub struct DuplicatesOptions {
    /// Ignore entities whose body is fewer than this many lines. Eliminates
    /// noise from single-line getters, empty stubs, re-exports, etc.
    pub min_lines: u32,
    /// Skip these kinds — imports aren't "code clones" in any useful sense.
    /// Configurable so callers can include them for audit purposes.
    pub exclude_kinds: Vec<String>,
    /// Drop groups larger than this (usually auto-generated code). 0 = no cap.
    pub max_group_size: usize,
    pub format: DuplicatesFormat,
}

impl Default for DuplicatesOptions {
    fn default() -> Self {
        Self {
            min_lines: 3,
            exclude_kinds: vec![
                "import".to_string(),
                "use".to_string(),
                "package".to_string(),
            ],
            max_group_size: 0,
            format: DuplicatesFormat::Markdown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuplicatesFormat {
    Markdown,
    Json,
}

impl DuplicatesFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "markdown" | "md" => Some(Self::Markdown),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

/// A single clone cluster.
#[derive(Debug, Clone, Serialize)]
pub struct CloneGroup {
    pub body_hash: String,
    pub lines: u32,
    pub members: Vec<CloneMember>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CloneMember {
    pub file: String,
    pub name: String,
    pub kind: String,
    pub line_start: u32,
    pub line_end: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DuplicatesReport {
    pub total_entities_scanned: usize,
    pub groups: Vec<CloneGroup>,
    pub total_clones: usize,
}

/// Main entry. Pure over Index.
pub fn find_duplicates(idx: &Index, opts: &DuplicatesOptions) -> DuplicatesReport {
    let mut by_hash: HashMap<&str, Vec<&Entity>> = HashMap::new();

    for e in &idx.entities {
        if opts.exclude_kinds.iter().any(|k| k == &e.kind) {
            continue;
        }
        let body_len = (e.line_end as i64 - e.line_start as i64 + 1).max(0) as u32;
        if body_len < opts.min_lines {
            continue;
        }
        let Some(hash) = e.body_hash.as_deref() else {
            continue; // entity has no body_hash (custom parsers may omit it)
        };
        by_hash.entry(hash).or_default().push(e);
    }

    let mut groups: Vec<CloneGroup> = by_hash
        .into_iter()
        .filter(|(_, members)| members.len() > 1)
        .filter(|(_, members)| {
            if opts.max_group_size == 0 {
                true
            } else {
                members.len() <= opts.max_group_size
            }
        })
        .map(|(hash, members)| {
            let mut sorted: Vec<&Entity> = members;
            sorted.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.line_start.cmp(&b.line_start)));
            let lines = sorted
                .first()
                .map(|e| e.line_end.saturating_sub(e.line_start) + 1)
                .unwrap_or(0);
            CloneGroup {
                body_hash: hash.to_string(),
                lines,
                members: sorted
                    .into_iter()
                    .map(|e| CloneMember {
                        file: e.file.clone(),
                        name: e.name.clone(),
                        kind: e.kind.clone(),
                        line_start: e.line_start,
                        line_end: e.line_end,
                        parent: e.parent.clone(),
                    })
                    .collect(),
            }
        })
        .collect();

    // Sort groups by impact: largest clusters first, break ties by line count
    // so bigger duplicated blocks outrank smaller ones at the same multiplicity.
    groups.sort_by(|a, b| {
        b.members
            .len()
            .cmp(&a.members.len())
            .then_with(|| b.lines.cmp(&a.lines))
            .then_with(|| {
                a.members
                    .first()
                    .map(|m| (m.file.as_str(), m.line_start))
                    .cmp(&b.members.first().map(|m| (m.file.as_str(), m.line_start)))
            })
    });

    let total_clones: usize = groups.iter().map(|g| g.members.len()).sum();

    DuplicatesReport {
        total_entities_scanned: idx.entities.len(),
        total_clones,
        groups,
    }
}

pub fn render_markdown(report: &DuplicatesReport) -> String {
    let mut out = String::with_capacity(2048);
    out.push_str("# Duplicates\n\n");
    out.push_str(&format!(
        "Scanned {} entities · {} clone groups · {} members across groups\n\n",
        report.total_entities_scanned,
        report.groups.len(),
        report.total_clones,
    ));
    if report.groups.is_empty() {
        out.push_str("_No clones found above the configured thresholds._\n");
        return out;
    }
    for (i, g) in report.groups.iter().enumerate() {
        out.push_str(&format!(
            "## Group {} — {} copies, {} lines each (body_hash `{}`)\n\n",
            i + 1,
            g.members.len(),
            g.lines,
            g.body_hash,
        ));
        for m in &g.members {
            let parent = m
                .parent
                .as_deref()
                .map(|p| format!(" (in {})", p))
                .unwrap_or_default();
            out.push_str(&format!(
                "- `{}`:{}-{} — {} **{}**{}\n",
                m.file, m.line_start, m.line_end, m.kind, m.name, parent,
            ));
        }
        out.push('\n');
    }
    out
}

pub fn render_json(report: &DuplicatesReport, pretty: bool) -> String {
    if pretty {
        serde_json::to_string_pretty(report).expect("report serializes infallibly")
    } else {
        serde_json::to_string(report).expect("report serializes infallibly")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::Entity;
    use crate::query::index::Index;

    fn ent(
        file: &str,
        name: &str,
        kind: &str,
        body_hash: Option<&str>,
        line_start: u32,
        line_end: u32,
    ) -> Entity {
        Entity {
            file: file.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            line_start,
            line_end,
            parent: None,
            sig: None,
            meta: None,
            body_hash: body_hash.map(str::to_string),
            sig_hash: None,
            struct_hash: "s".to_string(),
            visibility: None,
            rank: None,
            blast_radius: None,
            doc: None,
        }
    }

    #[test]
    fn empty_index_returns_zero_groups() {
        let idx = Index::default();
        let r = find_duplicates(&idx, &DuplicatesOptions::default());
        assert_eq!(r.groups.len(), 0);
        assert_eq!(r.total_clones, 0);
    }

    #[test]
    fn finds_basic_clone_pair() {
        let idx = Index::build(
            vec![
                ent("a.rs", "foo", "function", Some("abc123"), 1, 10),
                ent("b.rs", "bar", "function", Some("abc123"), 20, 29),
                ent("c.rs", "baz", "function", Some("def456"), 1, 10),
            ],
            vec![],
        );
        let r = find_duplicates(&idx, &DuplicatesOptions::default());
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].members.len(), 2);
        assert_eq!(r.groups[0].body_hash, "abc123");
    }

    #[test]
    fn excludes_short_bodies() {
        // Both entities share a hash but span only 2 lines each — below
        // default min_lines=3.
        let idx = Index::build(
            vec![
                ent("a.rs", "one_liner", "function", Some("x"), 1, 2),
                ent("b.rs", "another", "function", Some("x"), 5, 6),
            ],
            vec![],
        );
        let r = find_duplicates(&idx, &DuplicatesOptions::default());
        assert_eq!(r.groups.len(), 0);
    }

    #[test]
    fn excludes_import_kind_by_default() {
        let idx = Index::build(
            vec![
                ent("a.rs", "use std::x", "import", Some("h"), 1, 5),
                ent("b.rs", "use std::x", "import", Some("h"), 1, 5),
            ],
            vec![],
        );
        let r = find_duplicates(&idx, &DuplicatesOptions::default());
        assert_eq!(r.groups.len(), 0, "imports skipped by default");
    }

    #[test]
    fn skips_entities_without_body_hash() {
        let idx = Index::build(
            vec![
                ent("a.rs", "foo", "function", None, 1, 10),
                ent("b.rs", "bar", "function", None, 1, 10),
            ],
            vec![],
        );
        let r = find_duplicates(&idx, &DuplicatesOptions::default());
        assert_eq!(r.groups.len(), 0);
    }

    #[test]
    fn groups_sorted_by_size_desc() {
        let idx = Index::build(
            vec![
                // Group A: 3 copies, 10 lines
                ent("a1.rs", "f", "function", Some("A"), 1, 10),
                ent("a2.rs", "f", "function", Some("A"), 1, 10),
                ent("a3.rs", "f", "function", Some("A"), 1, 10),
                // Group B: 2 copies, 10 lines
                ent("b1.rs", "g", "function", Some("B"), 1, 10),
                ent("b2.rs", "g", "function", Some("B"), 1, 10),
            ],
            vec![],
        );
        let r = find_duplicates(&idx, &DuplicatesOptions::default());
        assert_eq!(r.groups.len(), 2);
        assert_eq!(r.groups[0].members.len(), 3);
        assert_eq!(r.groups[1].members.len(), 2);
    }

    #[test]
    fn max_group_size_drops_huge_clusters() {
        let idx = Index::build(
            (0..10)
                .map(|i| ent(&format!("f{i}.rs"), "gen", "function", Some("h"), 1, 10))
                .collect(),
            vec![],
        );
        let opts = DuplicatesOptions {
            max_group_size: 5,
            ..DuplicatesOptions::default()
        };
        let r = find_duplicates(&idx, &opts);
        assert!(r.groups.is_empty(), "group of 10 exceeds max_group_size=5");
    }

    #[test]
    fn members_sorted_by_file_then_line() {
        let idx = Index::build(
            vec![
                ent("b.rs", "f", "function", Some("h"), 20, 29),
                ent("a.rs", "f", "function", Some("h"), 10, 19),
                ent("a.rs", "f", "function", Some("h"), 100, 109),
            ],
            vec![],
        );
        let r = find_duplicates(&idx, &DuplicatesOptions::default());
        let paths: Vec<(&str, u32)> = r.groups[0]
            .members
            .iter()
            .map(|m| (m.file.as_str(), m.line_start))
            .collect();
        assert_eq!(paths, vec![("a.rs", 10), ("a.rs", 100), ("b.rs", 20)]);
    }

    #[test]
    fn markdown_renderer_outputs_header_and_groups() {
        let idx = Index::build(
            vec![
                ent("a.rs", "f", "function", Some("h"), 1, 10),
                ent("b.rs", "g", "function", Some("h"), 1, 10),
            ],
            vec![],
        );
        let r = find_duplicates(&idx, &DuplicatesOptions::default());
        let md = render_markdown(&r);
        assert!(md.starts_with("# Duplicates"));
        assert!(md.contains("Group 1"));
        assert!(md.contains("a.rs"));
        assert!(md.contains("b.rs"));
    }

    #[test]
    fn markdown_handles_empty_report() {
        let idx = Index::default();
        let r = find_duplicates(&idx, &DuplicatesOptions::default());
        let md = render_markdown(&r);
        assert!(md.contains("No clones found"));
    }

    #[test]
    fn format_parse_covers_known_values() {
        assert_eq!(
            DuplicatesFormat::parse("markdown"),
            Some(DuplicatesFormat::Markdown)
        );
        assert_eq!(
            DuplicatesFormat::parse("json"),
            Some(DuplicatesFormat::Json)
        );
        assert_eq!(DuplicatesFormat::parse("junk"), None);
    }
}
