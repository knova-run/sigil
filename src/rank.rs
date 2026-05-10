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
    // references it. Used both for direct counts and BFS.
    let mut refs_to_name: HashMap<&str, Vec<&Reference>> = HashMap::new();
    for r in references {
        refs_to_name.entry(r.name.as_str()).or_default().push(r);
    }

    let mut blast: HashMap<EntityKey, BlastRadius> = HashMap::new();
    for e in entities {
        let direct = refs_to_name.get(e.name.as_str()).cloned().unwrap_or_default();
        let direct_callers = direct.len() as u32;
        let direct_files: HashSet<&str> = direct.iter().map(|r| r.file.as_str()).collect();
        let direct_files_count = direct_files.len() as u32;
        let transitive = transitive_caller_count(&e.name, &refs_to_name, transitive_depth);

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

/// BFS over the reverse-call graph: how many unique callers reach `seed`
/// within `max_depth` hops. The caller-name set doubles as a visited set to
/// prevent cycles from blowing up the count.
fn transitive_caller_count(
    seed: &str,
    refs_to_name: &HashMap<&str, Vec<&Reference>>,
    max_depth: u32,
) -> u32 {
    if max_depth == 0 {
        return 0;
    }
    let mut visited: HashSet<&str> = HashSet::new();
    // Queue items: (symbol_name, depth_from_seed).
    let mut queue: VecDeque<(&str, u32)> = VecDeque::new();
    queue.push_back((seed, 0));
    visited.insert(seed);

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
