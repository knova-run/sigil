//! Heritage graph CLI: `sigil heritage <symbol>`.
//!
//! Reads `.sigil/entities.jsonl` and answers "what does this symbol extend
//! / embed?" (outgoing edges, recorded on the entity itself) and "who
//! extends / embeds this symbol?" (incoming edges, derived by scanning every
//! other entity's heritage vec for a matching target).
//!
//! Currently the only heritage kind populated by sigil is `embed` (Go struct
//! embedding). Class extension, trait impl, and interface implementation
//! land in follow-up work — the query surface is forward-compatible.

use serde::Serialize;

use crate::entity::Entity;
use crate::query::index::Index;

/// One incoming heritage edge: an entity that names this symbol in its own
/// `heritage` vec.
#[derive(Debug, Clone, Serialize)]
pub struct IncomingHeritage {
    /// Heritage kind from the source entity (e.g. `"embed"`).
    pub kind: String,
    /// Source entity that points at the queried symbol.
    pub from: String,
    /// File containing the source entity.
    pub file: String,
    /// Line where the source entity is declared.
    pub line: u32,
}

/// One outgoing heritage edge from the queried symbol.
#[derive(Debug, Clone, Serialize)]
pub struct OutgoingHeritage {
    pub kind: String,
    pub target: String,
}

/// Final report shape — serialised as JSON on stdout.
#[derive(Debug, Clone, Serialize)]
pub struct HeritageReport {
    pub symbol: String,
    /// One definition row per matching entity (a symbol can be ambiguous).
    pub definitions: Vec<DefinitionView>,
    pub outgoing: Vec<OutgoingHeritage>,
    pub incoming: Vec<IncomingHeritage>,
}

/// Light entity projection. Drops the BLAKE3 hashes + tokens so the JSON is
/// compact enough to drop directly into an agent prompt.
#[derive(Debug, Clone, Serialize)]
pub struct DefinitionView {
    pub file: String,
    pub name: String,
    pub kind: String,
    pub line_start: u32,
    pub line_end: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

impl From<&Entity> for DefinitionView {
    fn from(e: &Entity) -> Self {
        Self {
            file: e.file.clone(),
            name: e.name.clone(),
            kind: e.kind.clone(),
            line_start: e.line_start,
            line_end: e.line_end,
            parent: e.parent.clone(),
        }
    }
}

/// Build the heritage report for a single symbol against an in-memory Index.
///
/// Match rules:
/// * Outgoing edges come from every entity whose `name` equals the query.
///   Multiple definitions ⇒ multiple definition rows, but a single merged
///   `outgoing` vec (deduped on `(kind, target)`).
/// * Incoming edges: scan every entity's heritage vec for `target == query`
///   OR `target` whose last segment equals the query (so `pkg.Foo` matches
///   a query for `Foo`).
/// Entity kinds that can carry heritage edges. Excludes `import` and
/// markdown `section`/`code_block` rows that leak in via the
/// leaf-indexing pass — those aren't real type definitions and would
/// pollute the report (QA pass on slate: `heritage Editor` was returning
/// 366 import refs as "definitions").
const HERITAGE_DEFINITION_KINDS: &[&str] = &[
    "class",
    "struct",
    "interface",
    "trait",
    "enum",
    "object",
    "module",
    "type_alias",
];

fn is_heritage_definition_kind(kind: &str) -> bool {
    HERITAGE_DEFINITION_KINDS.contains(&kind)
}

pub fn build_report(idx: &Index, symbol: &str) -> HeritageReport {
    let definitions: Vec<&Entity> = idx
        .entities_by_name(symbol)
        .filter(|e| is_heritage_definition_kind(&e.kind))
        .collect();

    // Outgoing: merge across all matching definitions, dedupe.
    let mut outgoing: Vec<OutgoingHeritage> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for def in &definitions {
        for edge in &def.heritage {
            let key = (edge.kind.clone(), edge.target.clone());
            if seen.insert(key) {
                outgoing.push(OutgoingHeritage {
                    kind: edge.kind.clone(),
                    target: edge.target.clone(),
                });
            }
        }
    }

    // Incoming: scan every entity in the index.
    let mut incoming: Vec<IncomingHeritage> = Vec::new();
    for e in &idx.entities {
        for edge in &e.heritage {
            if edge_targets_symbol(&edge.target, symbol) {
                incoming.push(IncomingHeritage {
                    kind: edge.kind.clone(),
                    from: e.name.clone(),
                    file: e.file.clone(),
                    line: e.line_start,
                });
            }
        }
    }
    // Stable ordering: by file then line.
    incoming.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));

    HeritageReport {
        symbol: symbol.to_string(),
        definitions: definitions.iter().map(|e| DefinitionView::from(*e)).collect(),
        outgoing,
        incoming,
    }
}

/// Does a heritage edge's target name refer to `symbol`?
///
/// Heritage targets emitted by the Go extractor are either bare names
/// (`Bar`) or selector form (`pkg.Bar`). Match both — the user querying
/// `Bar` shouldn't have to know which form a particular extractor used.
fn edge_targets_symbol(target: &str, symbol: &str) -> bool {
    if target == symbol {
        return true;
    }
    if let Some(tail) = target.rsplit('.').next() {
        if tail == symbol {
            return true;
        }
    }
    if let Some(tail) = target.rsplit('/').next() {
        if tail == symbol {
            return true;
        }
    }
    false
}

/// Render the report as pretty or compact JSON.
pub fn render_json(report: &HeritageReport, pretty: bool) -> String {
    if pretty {
        serde_json::to_string_pretty(report).unwrap_or_default()
    } else {
        serde_json::to_string(report).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{HeritageEdge, Reference};

    fn ent(name: &str, file: &str, heritage: Vec<(&str, &str)>) -> Entity {
        Entity {
            file: file.to_string(),
            name: name.to_string(),
            kind: "struct".to_string(),
            line_start: 1,
            line_end: 5,
            parent: None,
            qualified_name: None,
            sig: None,
            meta: None,
            body_hash: None,
            sig_hash: None,
            struct_hash: "x".to_string(),
            visibility: None,
            rank: None,
            blast_radius: None,
            doc: None,
            heritage: heritage
                .into_iter()
                .map(|(k, t)| HeritageEdge {
                    kind: k.to_string(),
                    target: t.to_string(),
                })
                .collect(),
            alias: None,
        }
    }

    #[test]
    fn definitions_filtered_to_class_like_kinds() {
        // Regression: QA pass on slate (typescript) showed `sigil
        // heritage Editor` returning 379 "definitions" — 366 of which
        // were `import` refs, 1 was a markdown section, plus
        // properties/variables. The heritage report should only list
        // class/interface/struct/trait/enum entities — things that
        // can actually CARRY heritage edges.
        let mut import_ent = ent("Editor", "a.ts", vec![]);
        import_ent.kind = "import".to_string();
        let mut section_ent = ent("Editor", "docs/notes.md", vec![]);
        section_ent.kind = "section".to_string();
        let mut prop_ent = ent("Editor", "b.ts", vec![]);
        prop_ent.kind = "property".to_string();
        let mut var_ent = ent("Editor", "c.ts", vec![]);
        var_ent.kind = "variable".to_string();
        let real_class = ent("Editor", "core.ts", vec![]); // kind=struct from helper default
        let idx = Index::build(
            vec![import_ent, section_ent, prop_ent, var_ent, real_class],
            Vec::<Reference>::new(),
        );
        let report = build_report(&idx, "Editor");
        assert_eq!(
            report.definitions.len(),
            1,
            "only the class-like entity should surface as a heritage definition; got {:?}",
            report.definitions.iter().map(|d| &d.file).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn outgoing_edges_collected_from_matching_definition() {
        let bar = ent("Bar", "a.go", vec![]);
        let foo = ent("Foo", "a.go", vec![("embed", "Bar")]);
        let idx = Index::build(vec![bar, foo], Vec::<Reference>::new());
        let report = build_report(&idx, "Foo");
        assert_eq!(report.outgoing.len(), 1);
        assert_eq!(report.outgoing[0].kind, "embed");
        assert_eq!(report.outgoing[0].target, "Bar");
    }

    #[test]
    fn incoming_edges_picked_up_by_reverse_scan() {
        let bar = ent("Bar", "a.go", vec![]);
        let foo = ent("Foo", "a.go", vec![("embed", "Bar")]);
        let idx = Index::build(vec![bar, foo], Vec::<Reference>::new());
        let report = build_report(&idx, "Bar");
        assert_eq!(report.incoming.len(), 1);
        assert_eq!(report.incoming[0].kind, "embed");
        assert_eq!(report.incoming[0].from, "Foo");
    }

    #[test]
    fn qualified_target_matches_bare_query() {
        // Heritage target `mypkg.Bar` should still match a query for `Bar`.
        let foo = ent("Foo", "a.go", vec![("embed", "mypkg.Bar")]);
        let idx = Index::build(vec![foo], Vec::<Reference>::new());
        let report = build_report(&idx, "Bar");
        assert_eq!(report.incoming.len(), 1);
    }

    #[test]
    fn no_definition_and_no_incoming_yields_empty_report() {
        let idx = Index::build(Vec::<Entity>::new(), Vec::<Reference>::new());
        let report = build_report(&idx, "Nope");
        assert!(report.definitions.is_empty());
        assert!(report.outgoing.is_empty());
        assert!(report.incoming.is_empty());
    }

    #[test]
    fn duplicate_outgoing_edges_deduped_across_definitions() {
        // Two same-named structs (in two files) both embedding Bar.
        let foo1 = ent("Foo", "a.go", vec![("embed", "Bar")]);
        let foo2 = ent("Foo", "b.go", vec![("embed", "Bar")]);
        let idx = Index::build(vec![foo1, foo2], Vec::<Reference>::new());
        let report = build_report(&idx, "Foo");
        // Two definitions but one outgoing edge.
        assert_eq!(report.definitions.len(), 2);
        assert_eq!(report.outgoing.len(), 1);
    }
}
