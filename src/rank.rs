//! Phase 1 ranking: file-level PageRank + per-entity blast radius.
//!
//! Everything here is a pure function over `(Vec<Entity>, Vec<Reference>)` so
//! it unit-tests without touching disk or git. Callers decide whether to
//! persist the output into the Entity slots (`rank`, `blast_radius`).
//!
//! ## Graph
//!
//! Sigil doesn't store a first-class import edge list. We derive one from the
//! reference table by the rule: "if file A contains a reference whose target
//! symbol is defined in file B, add edge A → B." Ambiguous targets (the same
//! name defined in multiple files) contribute a fractional weight 1/N to each
//! candidate, which is PageRank's standard way of handling uncertain edges.
//!
//! ## Algorithm (file-level PageRank)
//!
//! Standard power iteration:
//!   - Damping factor 0.85.
//!   - Uniform teleportation vector (every file equally likely to jump to).
//!   - 50 iterations, or until L1 delta < 1e-6 (we stop on whichever comes
//!     first).
//!   - Dangling-node handling: mass from files with no outbound edges is
//!     redistributed uniformly across every file in the same iteration.
//!
//! At 1M entities the full pass is comfortably under a second.
//!
//! ## Blast radius
//!
//! For each entity, count:
//!   - `direct_callers`  : number of `Reference` rows with `name == entity.name`.
//!   - `direct_files`    : distinct `file` across those rows.
//!   - `transitive_callers`: BFS over the reverse-call graph, capped at depth 3.
//!
//! The depth cap is intentional. On highly-connected symbols (e.g.
//! `fmt::Display`) an unbounded transitive closure approaches the whole
//! codebase and stops being useful. Depth 3 captures the impact neighborhood
//! a reviewer actually cares about.

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::entity::{BlastRadius, Entity, Reference};

/// Output of a single rank pass. Pure data; callers decide how to persist it.
#[derive(Debug, Default, Clone)]
pub struct RankedIndex {
    /// PageRank score per file. Files with no entities or refs still appear
    /// with their teleportation-only score so downstream callers can look up
    /// any path they see in the index without error.
    pub file_rank: HashMap<String, f64>,

    /// Blast radius per entity. Keyed on (file, name, parent) because a repo
    /// can contain multiple entities with the same name (e.g. two `new` methods
    /// on different structs) and we need to tell them apart when populating
    /// Entity.blast_radius later.
    pub blast: HashMap<EntityKey, BlastRadius>,
}

/// Unique key for an entity in the Phase 1 rank output.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EntityKey {
    pub file: String,
    pub name: String,
    pub parent: Option<String>,
}

impl EntityKey {
    pub fn from_entity(e: &Entity) -> Self {
        Self {
            file: e.file.clone(),
            name: e.name.clone(),
            parent: e.parent.clone(),
        }
    }
}

/// Tunable knobs. Default constants are the values we ship; expose them as
/// fields so callers (tests, CLI `--rank-*` flags later) can vary them.
#[derive(Debug, Clone)]
pub struct RankConfig {
    pub damping: f64,
    pub max_iterations: u32,
    pub convergence_epsilon: f64,
    pub transitive_depth: u32,
}

impl Default for RankConfig {
    fn default() -> Self {
        Self {
            damping: 0.85,
            max_iterations: 50,
            convergence_epsilon: 1e-6,
            transitive_depth: 3,
        }
    }
}

/// One pass: derive the file graph, run PageRank, compute blast radius per
/// entity. Pure function — no I/O, no ordering assumptions on the inputs.
pub fn rank(entities: &[Entity], references: &[Reference]) -> RankedIndex {
    rank_with_config(entities, references, &RankConfig::default())
}

/// Populate `Entity.blast_radius` in place from a rank pass. `Entity.rank`
/// is intentionally left alone — file-level scores live in `.sigil/rank.json`
/// and get joined on read. A future week layers visibility multipliers on
/// top to produce per-entity rank values.
pub fn apply_blast_radius(entities: &mut [Entity], ranked: &RankedIndex) {
    for e in entities.iter_mut() {
        let key = EntityKey::from_entity(e);
        if let Some(br) = ranked.blast.get(&key) {
            e.blast_radius = Some(*br);
        }
    }
}

/// Serializable on-disk form of the file-level rank pass. Written to
/// `.sigil/rank.json` by `sigil index --rank` (default on).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RankManifest {
    pub version: String,
    pub sigil_version: String,
    pub damping: f64,
    pub iterations_max: u32,
    pub transitive_depth: u32,
    pub file_count: usize,
    pub file_rank: HashMap<String, f64>,
}

impl RankManifest {
    pub fn from_ranked(ranked: &RankedIndex, cfg: &RankConfig) -> Self {
        Self {
            version: "1".to_string(),
            sigil_version: env!("CARGO_PKG_VERSION").to_string(),
            damping: cfg.damping,
            iterations_max: cfg.max_iterations,
            transitive_depth: cfg.transitive_depth,
            file_count: ranked.file_rank.len(),
            file_rank: ranked.file_rank.clone(),
        }
    }
}

pub fn rank_with_config(
    entities: &[Entity],
    references: &[Reference],
    cfg: &RankConfig,
) -> RankedIndex {
    let name_index = build_name_index(entities);
    let edges = build_file_edges(references, &name_index);

    // The universe of nodes in the graph is every file we've seen — either as
    // the home of an entity or as the origin of a reference. Files with
    // entities but no outbound refs still deserve a rank (teleport only).
    let mut nodes: HashSet<&str> = HashSet::new();
    for e in entities {
        nodes.insert(&e.file);
    }
    for r in references {
        nodes.insert(&r.file);
    }

    let file_rank = pagerank(&nodes, &edges, cfg);
    let blast = compute_blast(entities, references, cfg.transitive_depth);

    RankedIndex { file_rank, blast }
}

/// name → list of files that define at least one entity with this name.
/// Ambiguous names (shared across files) are handled downstream by splitting
/// edge weight equally across candidates.
fn build_name_index(entities: &[Entity]) -> HashMap<&str, Vec<&str>> {
    let mut out: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in entities {
        out.entry(e.name.as_str())
            .or_default()
            .push(e.file.as_str());
    }
    // Dedupe per-name — multiple entities in the same file sharing a name
    // shouldn't inflate the edge weight.
    for files in out.values_mut() {
        files.sort();
        files.dedup();
    }
    out
}

/// Map of (src_file → HashMap<dst_file, weight>) — fractional weights because
/// ambiguous references split 1/N across candidate definitions.
fn build_file_edges(
    references: &[Reference],
    name_index: &HashMap<&str, Vec<&str>>,
) -> HashMap<String, HashMap<String, f64>> {
    let mut edges: HashMap<String, HashMap<String, f64>> = HashMap::new();
    for r in references {
        let Some(targets) = name_index.get(r.name.as_str()) else {
            continue; // reference points outside the indexed code — skip
        };
        // Don't count self-loops toward file rank; they'd just inflate a
        // file's own score without adding graph signal.
        let targets: Vec<&str> = targets.iter().copied().filter(|f| *f != r.file).collect();
        if targets.is_empty() {
            continue;
        }
        let weight = 1.0 / targets.len() as f64;
        let src_bucket = edges.entry(r.file.clone()).or_default();
        for t in targets {
            *src_bucket.entry(t.to_string()).or_insert(0.0) += weight;
        }
    }
    edges
}

/// Standard PageRank power iteration over the file graph. Dangling nodes
/// (files with no outbound weight) have their mass redistributed uniformly.
fn pagerank(
    nodes: &HashSet<&str>,
    edges: &HashMap<String, HashMap<String, f64>>,
    cfg: &RankConfig,
) -> HashMap<String, f64> {
    let n = nodes.len();
    if n == 0 {
        return HashMap::new();
    }
    let n_f = n as f64;
    let initial = 1.0 / n_f;
    let teleport = (1.0 - cfg.damping) / n_f;

    let mut scores: HashMap<String, f64> = nodes.iter().map(|f| (f.to_string(), initial)).collect();

    // Precompute per-source outbound totals (sum of weights leaving each file).
    let out_totals: HashMap<&str, f64> = edges
        .iter()
        .map(|(f, m)| (f.as_str(), m.values().sum()))
        .collect();

    for _ in 0..cfg.max_iterations {
        // Dangling mass = total rank held by files with no outbound edges,
        // redistributed uniformly. Skipping this step would drain rank into
        // sinks and produce unstable results on real repos.
        let dangling_mass: f64 = scores
            .iter()
            .filter(|(f, _)| !out_totals.contains_key(f.as_str()))
            .map(|(_, s)| *s)
            .sum();
        let dangling_share = cfg.damping * dangling_mass / n_f;

        let mut next: HashMap<String, f64> =
            nodes.iter().map(|f| (f.to_string(), teleport + dangling_share)).collect();

        // Push rank along every edge, weighted by edge weight / source's
        // outbound total.
        for (src, dsts) in edges {
            let src_score = *scores.get(src).unwrap_or(&0.0);
            let src_total = *out_totals.get(src.as_str()).unwrap_or(&0.0);
            if src_total <= 0.0 {
                continue;
            }
            for (dst, w) in dsts {
                if let Some(slot) = next.get_mut(dst) {
                    *slot += cfg.damping * src_score * (w / src_total);
                }
            }
        }

        // Early stop on L1 convergence.
        let delta: f64 = scores
            .iter()
            .map(|(f, s)| (s - next.get(f).copied().unwrap_or(0.0)).abs())
            .sum();
        scores = next;
        if delta < cfg.convergence_epsilon {
            break;
        }
    }

    scores
}

/// For each entity, count direct callers, distinct caller files, and
/// transitive callers via BFS over the reverse-call graph (capped at
/// `transitive_depth`).
fn compute_blast(
    entities: &[Entity],
    references: &[Reference],
    transitive_depth: u32,
) -> HashMap<EntityKey, BlastRadius> {
    // `refs_to_name` maps a target symbol name to every (file, caller) that
    // references it. Mirror the alias-expansion that
    // `src/query/index.rs::Index::build` does for `refs_by_name` so an
    // entity with a qualified-form name (Ruby `Faraday.Connection`,
    // Kotlin/Scala `Foo.Bar`) finds refs stored as
    // `Faraday::Connection.new` or similar. Without this, blast_radius
    // came back None on every Ruby/Kotlin/Scala class with a
    // non-trivial namespace.
    let mut refs_to_name: HashMap<String, Vec<&Reference>> = HashMap::new();
    for r in references {
        // Literal name
        refs_to_name.entry(r.name.clone()).or_default().push(r);
        // bare_leaf (latest `.` or `::`)
        if let Some(leaf) = crate::query::index::bare_leaf(&r.name) {
            refs_to_name.entry(leaf.to_string()).or_default().push(r);
        }
        // `::` head + its leaf (uppercase-gated inside helper)
        if r.name.contains("::") {
            for h in crate::query::index::head_prefixes_with_sep(&r.name, "::") {
                refs_to_name.entry(h).or_default().push(r);
            }
        }
        // `.` head + its leaf
        if r.name.contains('.') {
            for h in crate::query::index::head_prefixes_with_sep(&r.name, ".") {
                refs_to_name.entry(h).or_default().push(r);
            }
        }
    }

    let mut blast: HashMap<EntityKey, BlastRadius> = HashMap::new();
    for e in entities {
        // Match by literal entity name AND by its qualified-name form
        // AND by its bare_leaf form. The qualified-name fallback catches
        // entities where the parser stores `name` with one separator
        // and refs use another (Ruby `Faraday.Connection` entity vs
        // `Faraday::Connection.new` refs). The bare_leaf fallback
        // catches entities with mixed-separator qualified_names that
        // don't match either format directly (rspec-core
        // `RSpec.Core.Runner` with `qualified_name=RSpec.Core::Runner`
        // never matches refs that use `RSpec::Core::Runner` — but
        // `Runner` does match the leaf-indexed aliases).
        let mut hits: Vec<&Reference> = refs_to_name
            .get(e.name.as_str())
            .cloned()
            .unwrap_or_default();
        if let Some(qn) = e.qualified_name.as_deref() {
            if qn != e.name {
                if let Some(extra) = refs_to_name.get(qn) {
                    for r in extra {
                        hits.push(r);
                    }
                }
            }
        }
        if let Some(leaf) = crate::query::index::bare_leaf(&e.name) {
            if leaf != e.name {
                if let Some(extra) = refs_to_name.get(leaf) {
                    for r in extra {
                        hits.push(r);
                    }
                }
            }
        }
        // Dedup by pointer identity to avoid double-counting refs that
        // matched multiple lookup paths.
        let direct: Vec<&Reference> = {
            let mut seen: HashSet<*const Reference> = HashSet::new();
            hits.into_iter()
                .filter(|r| seen.insert(*r as *const Reference))
                .collect()
        };
        let direct_callers = direct.len() as u32;
        let direct_files: HashSet<&str> = direct.iter().map(|r| r.file.as_str()).collect();
        let direct_files_count = direct_files.len() as u32;
        // BFS seed: try the same three name forms as the direct-caller
        // lookup (`e.name`, `e.qualified_name`, `bare_leaf(e.name)`)
        // so transitive coverage matches direct for mixed-separator
        // qualified-name entities (Ruby/Kotlin/Scala). Without this,
        // `Faraday.Connection` showed direct=2 but transitive=0 even
        // when a chain existed.
        let mut seeds: Vec<&str> = vec![e.name.as_str()];
        if let Some(qn) = e.qualified_name.as_deref() {
            if qn != e.name {
                seeds.push(qn);
            }
        }
        if let Some(leaf) = crate::query::index::bare_leaf(&e.name) {
            if leaf != e.name {
                seeds.push(leaf);
            }
        }
        let transitive = transitive_caller_count(&seeds, &refs_to_name, transitive_depth);

        blast.insert(
            EntityKey::from_entity(e),
            BlastRadius {
                direct_callers,
                direct_files: direct_files_count,
                transitive_callers: transitive,
            },
        );
    }
    blast
}

/// BFS over the reverse-call graph: how many unique callers reach any
/// `seed` within `max_depth` hops. Multiple seeds let mixed-separator
/// qualified-name entities reach their inbound refs (Ruby/Kotlin/Scala —
/// `Faraday.Connection` seeds `Faraday::Connection` and `Connection`).
/// The visited set is shared across seeds so a caller reachable from
/// more than one seed is counted once.
fn transitive_caller_count(
    seeds: &[&str],
    refs_to_name: &HashMap<String, Vec<&Reference>>,
    max_depth: u32,
) -> u32 {
    if max_depth == 0 || seeds.is_empty() {
        return 0;
    }
    let mut visited: HashSet<&str> = HashSet::new();
    // Queue items: (symbol_name, depth_from_seed).
    let mut queue: VecDeque<(&str, u32)> = VecDeque::new();
    for seed in seeds {
        if visited.insert(seed) {
            queue.push_back((seed, 0));
        }
    }

    let mut count: u32 = 0;
    while let Some((sym, depth)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }
        let Some(incoming) = refs_to_name.get(sym) else {
            continue;
        };
        for r in incoming {
            let Some(caller) = r.caller.as_deref() else {
                continue; // top-level ref with no enclosing caller
            };
            if visited.insert(caller) {
                count += 1;
                queue.push_back((caller, depth + 1));
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{Entity, Reference};

    // Helpers keep tests readable — no field churn when Entity/Reference grow
    // more slots in later phases.

    fn ent(file: &str, name: &str, kind: &str) -> Entity {
        Entity {
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
            struct_hash: "deadbeef".to_string(),
            visibility: None,
            rank: None,
            blast_radius: None,
            doc: None,
            heritage: Vec::new(),
        }
    }

    fn refr(file: &str, caller: Option<&str>, name: &str) -> Reference {
        Reference {
            file: file.to_string(),
            caller: caller.map(str::to_string),
            name: name.to_string(),
            ref_kind: "call".to_string(),
            line: 1,
            confidence: None,
            callee_id: None,
        }
    }

    // ──────────────────────────────────────────────────────────────────
    // Trivial / degenerate inputs.
    // ──────────────────────────────────────────────────────────────────

    #[test]
    fn empty_input_produces_empty_output() {
        let r = rank(&[], &[]);
        assert!(r.file_rank.is_empty());
        assert!(r.blast.is_empty());
    }

    #[test]
    fn entities_without_refs_get_uniform_rank() {
        let entities = vec![
            ent("a.rs", "foo", "function"),
            ent("b.rs", "bar", "function"),
        ];
        let r = rank(&entities, &[]);
        assert_eq!(r.file_rank.len(), 2);
        let a = r.file_rank["a.rs"];
        let b = r.file_rank["b.rs"];
        assert!((a - b).abs() < 1e-9, "two unconnected files get equal rank");
        assert!((a - 0.5).abs() < 1e-6);
    }

    // ──────────────────────────────────────────────────────────────────
    // PageRank sanity — topology should determine relative ranking.
    // ──────────────────────────────────────────────────────────────────

    #[test]
    fn more_referenced_file_outranks_less_referenced() {
        // a.rs + b.rs both call foo (in popular.rs). unused.rs is isolated.
        // Expected: popular.rs has highest rank.
        let entities = vec![
            ent("popular.rs", "foo", "function"),
            ent("unused.rs", "_", "function"),
            ent("a.rs", "caller_a", "function"),
            ent("b.rs", "caller_b", "function"),
        ];
        let refs = vec![
            refr("a.rs", Some("caller_a"), "foo"),
            refr("b.rs", Some("caller_b"), "foo"),
        ];
        let r = rank(&entities, &refs);
        let popular = r.file_rank["popular.rs"];
        let unused = r.file_rank["unused.rs"];
        let a = r.file_rank["a.rs"];
        assert!(popular > a, "popular.rs ({popular}) should outrank a.rs ({a})");
        assert!(popular > unused, "popular.rs should outrank isolated unused.rs");
    }

    #[test]
    fn pagerank_scores_sum_to_approximately_one() {
        let entities = vec![
            ent("a.rs", "foo", "function"),
            ent("b.rs", "bar", "function"),
            ent("c.rs", "baz", "function"),
        ];
        let refs = vec![
            refr("a.rs", Some("caller"), "bar"),
            refr("c.rs", Some("caller"), "bar"),
            refr("b.rs", Some("caller"), "baz"),
        ];
        let r = rank(&entities, &refs);
        let total: f64 = r.file_rank.values().sum();
        assert!((total - 1.0).abs() < 1e-4, "PageRank scores should sum to ~1, got {total}");
    }

    #[test]
    fn self_loops_do_not_inflate_rank() {
        // a.rs referencing its own symbol must not boost a.rs.
        let entities = vec![
            ent("a.rs", "foo", "function"),
            ent("b.rs", "bar", "function"),
        ];
        let self_ref = vec![refr("a.rs", Some("foo"), "foo")]; // self-loop on a.rs
        let no_ref: Vec<Reference> = vec![];
        let with_loop = rank(&entities, &self_ref);
        let without_loop = rank(&entities, &no_ref);
        assert!(
            (with_loop.file_rank["a.rs"] - without_loop.file_rank["a.rs"]).abs() < 1e-6,
            "self-loop should not change a.rs rank"
        );
    }

    #[test]
    fn ambiguous_names_split_edge_weight() {
        // `Config` defined in both a.rs and b.rs. c.rs refers to `Config`.
        // Expected: a.rs and b.rs each receive half the weight, are equal, and
        // both outrank c.rs which has no incoming edges.
        let entities = vec![
            ent("a.rs", "Config", "struct"),
            ent("b.rs", "Config", "struct"),
            ent("c.rs", "user", "function"),
        ];
        let refs = vec![refr("c.rs", Some("user"), "Config")];
        let r = rank(&entities, &refs);
        let a = r.file_rank["a.rs"];
        let b = r.file_rank["b.rs"];
        assert!((a - b).abs() < 1e-6, "ambiguous targets should split rank equally");
        assert!(a > r.file_rank["c.rs"]);
    }

    // ──────────────────────────────────────────────────────────────────
    // Blast radius.
    // ──────────────────────────────────────────────────────────────────

    #[test]
    fn blast_direct_counts_match_ref_table() {
        let entities = vec![ent("a.rs", "foo", "function")];
        let refs = vec![
            refr("b.rs", Some("m"), "foo"),
            refr("c.rs", Some("m"), "foo"),
            refr("c.rs", Some("n"), "foo"),
        ];
        let r = rank(&entities, &refs);
        let key = EntityKey::from_entity(&entities[0]);
        let br = &r.blast[&key];
        assert_eq!(br.direct_callers, 3);
        assert_eq!(br.direct_files, 2, "b.rs + c.rs — two distinct caller files");
    }

    #[test]
    fn blast_entity_with_no_callers_is_zero() {
        let entities = vec![ent("a.rs", "never_called", "function")];
        let r = rank(&entities, &[]);
        let br = &r.blast[&EntityKey::from_entity(&entities[0])];
        assert_eq!(br.direct_callers, 0);
        assert_eq!(br.direct_files, 0);
        assert_eq!(br.transitive_callers, 0);
    }

    #[test]
    fn blast_transitive_chain_respects_depth() {
        // main → helper → worker → leaf. `leaf` is our entity of interest.
        // Default depth cap is 3, which means "count callers within 3 edges":
        //   hop 1: worker (caller of leaf)
        //   hop 2: helper (caller of worker)
        //   hop 3: main   (caller of helper)
        // → transitive_callers = 3.
        //
        // The depth check in BFS stops *further expansion* at hop 3, so a
        // 4-link chain would stop at main and not count anyone beyond.
        let entities = vec![ent("a.rs", "leaf", "function")];
        let refs = vec![
            refr("a.rs", Some("main"), "helper"),
            refr("a.rs", Some("helper"), "worker"),
            refr("a.rs", Some("worker"), "leaf"),
        ];
        let r = rank(&entities, &refs);
        let br = &r.blast[&EntityKey::from_entity(&entities[0])];
        assert_eq!(br.transitive_callers, 3);

        // Tighten the cap to 1 and only the immediate caller (worker) counts.
        let r1 = rank_with_config(
            &entities,
            &refs,
            &RankConfig { transitive_depth: 1, ..RankConfig::default() },
        );
        let br1 = &r1.blast[&EntityKey::from_entity(&entities[0])];
        assert_eq!(br1.transitive_callers, 1);
    }

    #[test]
    fn blast_handles_cycles_without_panic() {
        // a → b → c → a. Blast from any starting point should terminate.
        let entities = vec![ent("f.rs", "a", "function")];
        let refs = vec![
            refr("f.rs", Some("a"), "b"),
            refr("f.rs", Some("b"), "c"),
            refr("f.rs", Some("c"), "a"),
        ];
        let r = rank(&entities, &refs);
        let br = &r.blast[&EntityKey::from_entity(&entities[0])];
        // `a`'s callers (reverse graph): c. c's callers: b. b's callers: a → already visited.
        // So transitive = 2 (b, c), never revisits `a`.
        assert_eq!(br.transitive_callers, 2);
    }

    #[test]
    fn blast_same_name_in_different_files_gets_distinct_keys() {
        let entities = vec![
            ent("a.rs", "new", "function"),
            ent("b.rs", "new", "function"),
        ];
        let refs = vec![refr("c.rs", Some("m"), "new")];
        let r = rank(&entities, &refs);
        assert_eq!(r.blast.len(), 2, "one BlastRadius entry per (file, name, parent) key");
        // Both share the same direct_callers count (ref table matches on name alone)
        // but the keys are distinct so downstream consumers can distinguish them.
        let a_key = EntityKey::from_entity(&entities[0]);
        let b_key = EntityKey::from_entity(&entities[1]);
        assert_ne!(a_key, b_key);
        assert_eq!(r.blast[&a_key].direct_callers, 1);
        assert_eq!(r.blast[&b_key].direct_callers, 1);
    }

    // ──────────────────────────────────────────────────────────────────
    // Week 2: apply_blast_radius + RankManifest plumbing.
    // ──────────────────────────────────────────────────────────────────

    #[test]
    fn apply_blast_radius_populates_matching_entities() {
        let mut entities = vec![
            ent("a.rs", "foo", "function"),
            ent("a.rs", "bar", "function"),
        ];
        let refs = vec![
            refr("b.rs", Some("m"), "foo"),
            refr("b.rs", Some("n"), "foo"),
            refr("c.rs", Some("m"), "bar"),
        ];
        let ranked = rank(&entities, &refs);
        apply_blast_radius(&mut entities, &ranked);

        let foo_br = entities[0].blast_radius.unwrap();
        assert_eq!(foo_br.direct_callers, 2);
        assert_eq!(foo_br.direct_files, 1); // both callers in b.rs

        let bar_br = entities[1].blast_radius.unwrap();
        assert_eq!(bar_br.direct_callers, 1);
        assert_eq!(bar_br.direct_files, 1);
    }

    #[test]
    fn blast_radius_handles_qualified_ref_names() {
        // QA pass on faraday (Ruby): entity name `Faraday.Connection`
        // (with qualified_name `Faraday::Connection`) had 27 inbound
        // refs of form `Faraday::Connection.new` (plus chained
        // variants) but blast_radius came back None — the compute_blast
        // lookup was literal-name only, missing the same bare_leaf /
        // cc-head / dot-head expansion that `Index::refs_by_name`
        // performs at index time. Entities with qualified-form names
        // (Ruby/Kotlin/Scala emit `Foo.Bar`) should see their
        // blast_radius populated from refs that target them via `::`
        // or chain forms.
        let mut e = ent("a.rb", "Faraday.Connection", "class");
        e.qualified_name = Some("Faraday::Connection".to_string());
        let mut entities = vec![e];
        let refs = vec![
            refr("b.rb", Some("u"), "Faraday::Connection.new"),
            refr("c.rb", Some("u"), "Faraday::Connection.new"),
        ];
        let ranked = rank(&entities, &refs);
        apply_blast_radius(&mut entities, &ranked);
        let br = entities[0]
            .blast_radius
            .as_ref()
            .expect("blast_radius should be set for entity with qualified-form name");
        assert!(
            br.direct_callers >= 2,
            "expected ≥2 direct callers from `Faraday::Connection.new` refs; got {}",
            br.direct_callers
        );
    }

    #[test]
    fn blast_radius_transitive_callers_use_expanded_seed() {
        // Regression: PR #26 review surfaced that direct-callers lookup
        // expanded to try `e.name`, `e.qualified_name`, AND
        // `bare_leaf(e.name)`, but `transitive_caller_count` was still
        // called with only `&e.name`. For a Ruby entity
        // `Faraday.Connection` (qualified_name `Faraday::Connection`),
        // refs are indexed under `Faraday::Connection` / `Connection`
        // but NOT under `Faraday.Connection` — so BFS started cold
        // (transitive=0) even though direct=2. Setup: a 2-hop
        // transitive chain that the BFS can ONLY follow when seeded by
        // qualified_name or bare_leaf.
        let mut e = ent("a.rb", "Faraday.Connection", "class");
        e.qualified_name = Some("Faraday::Connection".to_string());
        let mut entities = vec![e];
        let refs = vec![
            // Direct hop: u_outer calls Faraday::Connection.new
            refr("b.rb", Some("u_outer"), "Faraday::Connection.new"),
            // Indirect hop: top_caller calls u_outer (transitive seed
            // is "Connection" via bare_leaf, then BFS visits ref
            // targeting u_outer once it follows the chain).
            refr("c.rb", Some("top_caller"), "u_outer"),
        ];
        let ranked = rank(&entities, &refs);
        apply_blast_radius(&mut entities, &ranked);
        let br = entities[0].blast_radius.as_ref().expect("blast_radius should be set");
        assert!(
            br.direct_callers >= 1,
            "expected ≥1 direct caller; got {}",
            br.direct_callers
        );
        assert!(
            br.transitive_callers >= 1,
            "transitive BFS should follow the chain from Faraday::Connection.new → u_outer → top_caller; got transitive={}",
            br.transitive_callers
        );
    }

    #[test]
    fn blast_radius_handles_mixed_separator_qualified_name() {
        // QA pass on rspec-core: entity name `RSpec.Core.Runner` with
        // qualified_name `RSpec.Core::Runner` (parser emits parent
        // with `.`-separators joined to leaf by `::`). Real refs use
        // `RSpec::Core::Runner.run` (all `::`). Neither entity.name
        // nor entity.qualified_name matches that ref form directly.
        // The bare_leaf of the entity (`Runner`) DOES match the
        // leaf-indexed alias keys though.
        let mut e = ent("a.rb", "RSpec.Core.Runner", "class");
        e.qualified_name = Some("RSpec.Core::Runner".to_string()); // mixed
        let mut entities = vec![e];
        let refs = vec![
            refr("b.rb", Some("u"), "RSpec::Core::Runner.run"),
            refr("c.rb", Some("u"), "RSpec::Core::Runner.new"),
            refr("d.rb", Some("u"), "RSpec::Core::Runner.autorun"),
        ];
        let ranked = rank(&entities, &refs);
        apply_blast_radius(&mut entities, &ranked);
        let br = entities[0]
            .blast_radius
            .as_ref()
            .expect("bare_leaf fallback should rescue this");
        assert!(
            br.direct_callers >= 3,
            "expected ≥3 direct callers via bare_leaf fallback; got {}",
            br.direct_callers
        );
    }

    #[test]
    fn apply_blast_radius_leaves_unmatched_none() {
        // Entities that have no row in the rank pass (impossible under
        // normal use — rank covers all entities — but covered for safety).
        let mut entities = vec![ent("a.rs", "foo", "function")];
        let empty = RankedIndex::default();
        apply_blast_radius(&mut entities, &empty);
        assert!(entities[0].blast_radius.is_none());
    }

    #[test]
    fn rank_manifest_roundtrips_through_json() {
        let entities = vec![ent("a.rs", "foo", "function")];
        let refs = vec![refr("b.rs", Some("m"), "foo")];
        let cfg = RankConfig::default();
        let ranked = rank_with_config(&entities, &refs, &cfg);

        let manifest = RankManifest::from_ranked(&ranked, &cfg);
        assert_eq!(manifest.version, "1");
        assert_eq!(manifest.damping, 0.85);
        assert_eq!(manifest.transitive_depth, 3);
        assert_eq!(manifest.file_count, 2); // a.rs + b.rs

        let json = serde_json::to_string(&manifest).unwrap();
        let back: RankManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.file_count, manifest.file_count);
        for (k, v) in &manifest.file_rank {
            assert!((back.file_rank[k] - v).abs() < 1e-12);
        }
    }

    #[test]
    fn config_knobs_take_effect() {
        // Dialing transitive_depth to 0 short-circuits the BFS.
        let entities = vec![ent("a.rs", "leaf", "function")];
        let refs = vec![refr("a.rs", Some("caller"), "leaf")];
        let shallow = RankConfig {
            transitive_depth: 0,
            ..RankConfig::default()
        };
        let r = rank_with_config(&entities, &refs, &shallow);
        let br = &r.blast[&EntityKey::from_entity(&entities[0])];
        assert_eq!(br.direct_callers, 1, "direct count is depth-independent");
        assert_eq!(br.transitive_callers, 0, "depth=0 → no BFS");
    }
}
