//! `sigil outline` — hierarchical top-level tree of classes, top-level
//! functions, structs, enums, and traits across the codebase.
//!
//! Complements `sigil map` (which is rank-ordered and budget-aware) by
//! answering "show me what's in this directory / project structurally."
//! No PageRank, no token budget — every file gets its top-level items
//! listed exactly once.
//!
//! Designed to replace the common agent pattern of running `find` +
//! `ls` + multiple `sigil symbols FILE --depth 1` in a row.

use serde::Serialize;
use std::collections::BTreeMap;

use crate::entity::Entity;
use crate::query::index::Index;
use crate::query::is_top_level_outline;

#[derive(Debug, Clone, Serialize)]
pub struct OutlineEntry {
    pub name: String,
    pub kind: String,
    pub line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_end: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileOutline {
    pub path: String,
    pub entities: Vec<OutlineEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutlineReport {
    pub files: Vec<FileOutline>,
    pub total_files: usize,
    pub total_entities: usize,
}

/// Build a hierarchical outline.
///
/// `kind_filter` restricts the returned entities to the listed kinds. An
/// empty slice means "all outline-eligible kinds" (the pre-filter default).
/// Typical values: `["class"]` to match `grep -n "^class "`, `["class",
/// "function"]` to mirror the default outline shape. Unknown kinds are
/// silently skipped — they just won't match anything.
pub fn build_outline(
    idx: &Index,
    path_prefix: Option<&str>,
    kind_filter: &[String],
) -> OutlineReport {
    let mut by_file: BTreeMap<String, Vec<&Entity>> = BTreeMap::new();
    for e in &idx.entities {
        if !is_top_level_outline(e) {
            continue;
        }
        if let Some(prefix) = path_prefix {
            if !e.file.starts_with(prefix) {
                continue;
            }
        }
        if !kind_filter.is_empty() && !kind_filter.iter().any(|k| k == &e.kind) {
            continue;
        }
        by_file.entry(e.file.clone()).or_default().push(e);
    }

    let total_files = by_file.len();
    let mut total_entities = 0;
    let mut files = Vec::with_capacity(total_files);
    for (path, ents) in by_file {
        let mut entities: Vec<OutlineEntry> = ents
            .into_iter()
            .map(|e| OutlineEntry {
                name: e.name.clone(),
                kind: e.kind.clone(),
                line: e.line_start,
                line_end: if e.line_end != e.line_start {
                    Some(e.line_end)
                } else {
                    None
                },
                sig: e.sig.clone(),
            })
            .collect();
        entities.sort_by_key(|o| o.line);
        total_entities += entities.len();
        files.push(FileOutline { path, entities });
    }

    OutlineReport {
        files,
        total_files,
        total_entities,
    }
}

pub fn render_markdown(report: &OutlineReport) -> String {
    if report.files.is_empty() {
        return "No top-level symbols found under the given scope.\n".to_string();
    }
    let mut out = format!(
        "# Outline — {} files, {} top-level symbols\n\n",
        report.total_files, report.total_entities
    );
    for f in &report.files {
        out.push_str(&format!("## `{}`\n\n", f.path));
        for e in &f.entities {
            let range = match e.line_end {
                Some(end) => format!("{}-{}", e.line, end),
                None => format!("{}", e.line),
            };
            out.push_str(&format!("- **{}** `{}` — {}\n", e.kind, e.name, range));
        }
        out.push('\n');
    }
    out
}

pub fn render_json(report: &OutlineReport, pretty: bool) -> String {
    if pretty {
        serde_json::to_string_pretty(report).expect("OutlineReport serializes infallibly")
    } else {
        serde_json::to_string(report).expect("OutlineReport serializes infallibly")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{BlastRadius, Entity};
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
            heritage: Vec::new(),
        }
    }

    #[test]
    fn outline_groups_by_file_and_drops_nested() {
        let idx = Index::build(
            vec![
                ent("a.py", "Foo", "class", None, 10),
                ent("a.py", "Foo.bar", "method", Some("Foo"), 15), // nested — drop
                ent("a.py", "top_fn", "function", None, 40),
                ent("b.py", "OtherClass", "class", None, 1),
            ],
            vec![],
        );
        let report = build_outline(&idx, None, &[]);
        assert_eq!(report.total_files, 2);
        assert_eq!(report.total_entities, 3, "Foo, top_fn, OtherClass");
        assert_eq!(report.files[0].entities.len(), 2);
        assert_eq!(report.files[0].entities[0].name, "Foo");
        assert_eq!(report.files[0].entities[1].name, "top_fn");
    }

    #[test]
    fn outline_respects_path_prefix() {
        let idx = Index::build(
            vec![
                ent("src/a.py", "A", "class", None, 10),
                ent("tests/test_a.py", "TestA", "class", None, 10),
            ],
            vec![],
        );
        let src_only = build_outline(&idx, Some("src/"), &[]);
        assert_eq!(src_only.total_files, 1);
        assert_eq!(src_only.files[0].path, "src/a.py");
    }

    #[test]
    fn outline_kind_filter_restricts_to_listed_kinds() {
        let idx = Index::build(
            vec![
                ent("a.py", "Foo", "class", None, 10),
                ent("a.py", "helper_fn", "function", None, 30),
                ent("a.py", "_priv_fn", "function", None, 50),
                ent("a.py", "Bar", "class", None, 70),
            ],
            vec![],
        );
        let classes_only = build_outline(&idx, None, &["class".to_string()]);
        assert_eq!(classes_only.total_entities, 2);
        assert!(classes_only.files[0].entities.iter().all(|e| e.kind == "class"));

        // Empty slice = no filter — default behavior unchanged.
        let all = build_outline(&idx, None, &[]);
        assert_eq!(all.total_entities, 4);

        let both = build_outline(
            &idx,
            None,
            &["class".to_string(), "function".to_string()],
        );
        assert_eq!(both.total_entities, 4);
    }

    #[test]
    fn outline_skips_imports_and_variables() {
        let idx = Index::build(
            vec![
                ent("a.py", "os", "import", None, 1),
                ent("a.py", "CONFIG", "constant", None, 3),
                ent("a.py", "Foo", "class", None, 10),
            ],
            vec![],
        );
        let report = build_outline(&idx, None, &[]);
        assert_eq!(report.total_entities, 1);
        assert_eq!(report.files[0].entities[0].name, "Foo");
    }
}
