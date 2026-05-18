//! Code-aware rerank signals over a retrieval candidate set (Spike 4).
//!
//! The retriever (BM25 or Model2Vec) gives us a candidate list scored by
//! lexical / semantic relevance. Those scores ignore *code structure*
//! signals that an agent typically cares about:
//!
//!   - Hits in production source beat hits in test files.
//!   - Hits in central files (high PageRank) beat hits in leaf utility.
//!   - Function/class/struct definitions beat imports and bare variables.
//!   - Vendored/generated paths are almost never the right answer.
//!
//! Each signal is a multiplicative boost (or penalty) on the candidate
//! score. After applying every active signal, the list is re-sorted and
//! truncated to k. The retriever is expected to over-fetch — request
//! `k * CANDIDATE_OVER_FETCH` from the upstream and let rerank drop the
//! ones penalised below their original neighbours.

use crate::entity::{is_test_path, Entity};

/// How many extra candidates to pull from the retriever before reranking,
/// so penalty multipliers can demote bad hits out of the final top-k
/// without losing good ones below them. 3× is the usual headroom.
pub const CANDIDATE_OVER_FETCH: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RerankConfig {
    /// Multiplier for hits in test files. < 1.0 demotes test files.
    pub test_penalty: f32,
    /// Multiplier for hits in vendored / generated paths
    /// (`node_modules/`, `vendor/`, `*.pb.go`, etc.). < 1.0 demotes.
    pub vendored_penalty: f32,
    /// Multiplier for function / method / class / struct / impl / enum /
    /// interface / trait kinds. > 1.0 promotes definitions over imports
    /// and bare variables.
    pub definition_boost: f32,
    /// Multiplier for variable / constant / import / external kinds.
    /// < 1.0 demotes non-definitions.
    pub non_definition_penalty: f32,
    /// Maximum extra multiplier from file PageRank. Final boost is
    /// `1 + rank_boost_factor * normalized_rank`. `normalized_rank` is the
    /// entity's `rank` field clamped to [0, 1]. Defaults to 0.3 so a
    /// max-rank file is only 30% better than a zero-rank file.
    pub rank_boost_factor: f32,
}

impl Default for RerankConfig {
    fn default() -> Self {
        Self {
            // 0.45 makes a test hit have to score >2.2x its source rival
            // (before the definition_boost cancels on both sides) to
            // survive rerank — strong enough to surface production code
            // even when the test docstring lexically matches the query
            // better, but not so strong that legitimate test-only
            // questions ("where is the foo test fixture?") fail.
            test_penalty: 0.45,
            vendored_penalty: 0.30,
            definition_boost: 1.20,
            non_definition_penalty: 0.85,
            rank_boost_factor: 0.30,
        }
    }
}

/// Apply rerank signals to a (entity_idx, score) candidate list and
/// truncate to `k`. Input order is irrelevant; output is sorted
/// descending by adjusted score.
pub fn rerank(
    entities: &[Entity],
    candidates: Vec<(usize, f32)>,
    cfg: &RerankConfig,
    k: usize,
) -> Vec<(usize, f32)> {
    let mut adjusted: Vec<(usize, f32)> = candidates
        .into_iter()
        .map(|(i, raw)| (i, adjust_one(&entities[i], raw, cfg)))
        .collect();
    adjusted.sort_by(|a, b| {
        b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
    });
    adjusted.truncate(k);
    adjusted
}

fn adjust_one(e: &Entity, raw: f32, cfg: &RerankConfig) -> f32 {
    let mut s = raw;
    if is_test_path(&e.file) {
        s *= cfg.test_penalty;
    }
    if is_vendored_or_generated(&e.file) {
        s *= cfg.vendored_penalty;
    }
    match e.kind.as_str() {
        "function" | "method" | "class" | "struct" | "impl" | "enum"
        | "interface" | "trait" => {
            s *= cfg.definition_boost;
        }
        "variable" | "constant" | "import" | "external" => {
            s *= cfg.non_definition_penalty;
        }
        _ => {}
    }
    if let Some(rank) = e.rank {
        let clamped = rank.clamp(0.0, 1.0) as f32;
        s *= 1.0 + cfg.rank_boost_factor * clamped;
    }
    s
}

/// Heuristic match for vendored / generated / build-artifact paths.
/// Conservative: only flags shapes that are nearly universal across
/// ecosystems. Specific framework conventions live in `dead_code.rs`.
pub fn is_vendored_or_generated(file: &str) -> bool {
    let lower = file.replace('\\', "/").to_ascii_lowercase();
    lower.contains("/node_modules/")
        || lower.starts_with("node_modules/")
        || lower.contains("/vendor/")
        || lower.starts_with("vendor/")
        || lower.contains("/third_party/")
        || lower.starts_with("third_party/")
        || lower.contains("/third-party/")
        || lower.starts_with("third-party/")
        || lower.ends_with("_generated.go")
        || lower.ends_with(".pb.go")
        || lower.ends_with(".pb.ts")
        || lower.ends_with(".pb.py")
        || lower.contains(".generated.")
        || lower.contains("/dist/")
        || lower.starts_with("dist/")
        || lower.contains("/build/")
        || lower.starts_with("build/")
        || lower.contains("/target/")
        || lower.starts_with("target/")
        || lower.contains("/.next/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entity(file: &str, name: &str, kind: &str, rank: Option<f64>) -> Entity {
        Entity {
            file: file.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            line_start: 1,
            line_end: 5,
            parent: None,
            qualified_name: None,
            sig: None,
            meta: None,
            body_hash: None,
            sig_hash: None,
            struct_hash: String::new(),
            visibility: None,
            rank,
            blast_radius: None,
            doc: None,
            alias: None,
            heritage: Vec::new(),
        }
    }

    // --- is_vendored_or_generated -----------------------------------

    #[test]
    fn vendored_paths_detected() {
        assert!(is_vendored_or_generated("node_modules/foo/bar.js"));
        assert!(is_vendored_or_generated("packages/a/node_modules/b/c.ts"));
        assert!(is_vendored_or_generated("vendor/github.com/x/y/z.go"));
        assert!(is_vendored_or_generated("third_party/oss/lib.cpp"));
        assert!(is_vendored_or_generated("dist/bundle.js"));
        assert!(is_vendored_or_generated("target/debug/foo.rs"));
    }

    #[test]
    fn generated_paths_detected() {
        assert!(is_vendored_or_generated("internal/proto/foo.pb.go"));
        assert!(is_vendored_or_generated("internal/sched_generated.go"));
        assert!(is_vendored_or_generated("src/api.generated.ts"));
    }

    #[test]
    fn ordinary_source_paths_not_flagged() {
        assert!(!is_vendored_or_generated("src/foo.rs"));
        assert!(!is_vendored_or_generated("lib/utils.py"));
        assert!(!is_vendored_or_generated("internal/foo.go"));
    }

    // --- rerank signals ---------------------------------------------

    #[test]
    fn test_path_hits_demoted_below_source() {
        let entities = vec![
            make_entity("src/auth.rs", "login", "function", None),
            make_entity("tests/auth_test.rs", "test_login", "function", None),
        ];
        // Test file initially scores higher; rerank should flip the order.
        let candidates = vec![(0, 1.0), (1, 1.5)];
        let cfg = RerankConfig::default();
        let out = rerank(&entities, candidates, &cfg, 2);
        assert_eq!(out[0].0, 0, "production source should rank above test file");
    }

    #[test]
    fn definition_kinds_outrank_non_definitions_at_equal_score() {
        let entities = vec![
            make_entity("src/lib.rs", "foo", "import", None),
            make_entity("src/lib.rs", "Foo", "struct", None),
        ];
        let candidates = vec![(0, 1.0), (1, 1.0)];
        let cfg = RerankConfig::default();
        let out = rerank(&entities, candidates, &cfg, 2);
        assert_eq!(
            out[0].0, 1,
            "struct definition should rank above import at equal raw score"
        );
    }

    #[test]
    fn higher_rank_file_promoted_at_equal_score() {
        let entities = vec![
            make_entity("src/utility.rs", "helper", "function", Some(0.1)),
            make_entity("src/core.rs", "core_fn", "function", Some(0.9)),
        ];
        let candidates = vec![(0, 1.0), (1, 1.0)];
        let cfg = RerankConfig::default();
        let out = rerank(&entities, candidates, &cfg, 2);
        assert_eq!(out[0].0, 1, "high-rank file should rank above low-rank at equal raw score");
    }

    #[test]
    fn vendored_hit_demoted_below_source() {
        let entities = vec![
            make_entity("src/parse.rs", "parse", "function", None),
            make_entity("node_modules/foo/parse.js", "parse", "function", None),
        ];
        let candidates = vec![(0, 1.0), (1, 1.4)];
        let cfg = RerankConfig::default();
        let out = rerank(&entities, candidates, &cfg, 2);
        assert_eq!(out[0].0, 0, "source should rank above node_modules");
    }

    #[test]
    fn k_truncation_after_rerank() {
        let entities = vec![
            make_entity("a.rs", "foo", "function", None),
            make_entity("b.rs", "bar", "function", None),
            make_entity("c.rs", "baz", "function", None),
        ];
        let candidates = vec![(0, 1.0), (1, 0.9), (2, 0.8)];
        let out = rerank(&entities, candidates, &RerankConfig::default(), 2);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn rerank_is_idempotent_on_empty_candidates() {
        let entities: Vec<Entity> = Vec::new();
        let out = rerank(&entities, Vec::new(), &RerankConfig::default(), 10);
        assert!(out.is_empty());
    }

    #[test]
    fn strongly_penalised_hit_dropped_when_oversaturated_candidates_collapse() {
        // Realistic case: candidate set has 3 source hits and one test-file
        // hit that the retriever scored highest. After rerank, the test
        // hit drops to last; truncating to k=2 should yield the two
        // source hits.
        let entities = vec![
            make_entity("tests/x_test.rs", "test", "function", None),
            make_entity("src/a.rs", "a", "function", None),
            make_entity("src/b.rs", "b", "function", None),
            make_entity("src/c.rs", "c", "function", None),
        ];
        let candidates = vec![(0, 2.0), (1, 1.0), (2, 0.95), (3, 0.9)];
        let out = rerank(&entities, candidates, &RerankConfig::default(), 2);
        assert!(out.iter().all(|(i, _)| *i != 0), "test hit should drop out of top-2");
    }
}
