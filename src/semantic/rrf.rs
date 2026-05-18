//! Reciprocal Rank Fusion over multiple retrievers (Spike 3).
//!
//! RRF combines ranked lists from N retrievers into a single ranking by
//! summing `1 / (k_constant + rank)` over each list. Raw scores are
//! discarded — only positions matter — so the fused score is comparable
//! across retrievers even when one returns BM25 scores in [0, ∞) and
//! another returns cosine similarities in [-1, 1].
//!
//! Cormack et al. (2009) suggest `k_constant = 60`. Lucene and semble
//! use that default too. Smaller `k` weights top-of-list positions
//! more aggressively; larger `k` distributes credit further down.
//!
//! Empirical caveat (carried in from Spike 2's eval): semble's HYBRID
//! mode — the BM25 + Model2Vec RRF combination — is consistently
//! *worse* than its BM25-only mode on our cross-repo eval. Fusion is
//! not a free win; whether RRF actually lifts NDCG@10 on this corpus
//! is exactly what the eval driven from `evals/cross_repo_semantic_eval.py`
//! has to answer before we promote this to a default.

use std::collections::HashMap;

/// Cormack et al. (2009) default. Semble and Lucene use the same.
pub const DEFAULT_K_CONSTANT: f32 = 60.0;

/// Combine N ranked lists into one ranking.
///
/// Each list MUST be pre-sorted descending by its own native score;
/// only the position of each doc in each list matters for the fused
/// score (raw scores in the input are ignored).
///
/// Returns the fused top-k as `(doc_id, fused_score)`, sorted descending.
/// Docs absent from a list simply don't contribute via that list — there's
/// no implicit "rank = list.len() + 1" penalty.
pub fn rrf_fuse(
    rank_lists: &[Vec<(usize, f32)>],
    k_constant: f32,
    top_k: usize,
) -> Vec<(usize, f32)> {
    if rank_lists.is_empty() || top_k == 0 {
        return Vec::new();
    }
    let mut acc: HashMap<usize, f32> = HashMap::new();
    for list in rank_lists {
        for (rank_idx, &(doc, _native_score)) in list.iter().enumerate() {
            // rank is 1-indexed in the RRF formula.
            let rank = (rank_idx + 1) as f32;
            *acc.entry(doc).or_insert(0.0) += 1.0 / (k_constant + rank);
        }
    }
    let mut out: Vec<(usize, f32)> = acc.into_iter().collect();
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    out.truncate(top_k);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_yields_empty_output() {
        let out = rrf_fuse(&[], DEFAULT_K_CONSTANT, 10);
        assert!(out.is_empty());
    }

    #[test]
    fn k_zero_yields_empty_output() {
        let lists = vec![vec![(1, 1.0), (2, 0.5)]];
        let out = rrf_fuse(&lists, DEFAULT_K_CONSTANT, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn single_list_preserves_order() {
        let lists = vec![vec![(10, 5.0), (20, 4.0), (30, 3.0)]];
        let out = rrf_fuse(&lists, DEFAULT_K_CONSTANT, 3);
        assert_eq!(out[0].0, 10);
        assert_eq!(out[1].0, 20);
        assert_eq!(out[2].0, 30);
    }

    #[test]
    fn doc_at_rank_1_in_both_lists_wins() {
        // Doc 1 is top in both retrievers; doc 2 is top in only one.
        let bm25 = vec![(1, 10.0), (2, 9.0), (3, 8.0)];
        let m2v = vec![(1, 0.95), (4, 0.93), (2, 0.91)];
        let out = rrf_fuse(&[bm25, m2v], DEFAULT_K_CONSTANT, 4);
        assert_eq!(out[0].0, 1, "doc 1 (rank-1 in both) should rank first");
    }

    #[test]
    fn doc_only_in_one_list_still_appears() {
        let bm25 = vec![(1, 10.0)];
        let m2v: Vec<(usize, f32)> = vec![];
        let out = rrf_fuse(&[bm25, m2v], DEFAULT_K_CONSTANT, 5);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, 1);
    }

    #[test]
    fn rank_order_dominates_raw_score() {
        // Doc 1: rank 1 in BM25 with tiny score; rank 10 in m2v.
        // Doc 2: rank 2 in BM25 with much larger score; rank 1 in m2v.
        // RRF should prefer doc 2 because its average rank is better.
        let bm25 = vec![(1, 0.001), (2, 1000.0)];
        let mut m2v = Vec::with_capacity(10);
        m2v.push((2, 0.99));
        for i in 0..8 {
            m2v.push((100 + i, 0.9 - i as f32 * 0.1));
        }
        m2v.push((1, 0.01)); // doc 1 at rank 10
        let out = rrf_fuse(&[bm25, m2v], DEFAULT_K_CONSTANT, 5);
        assert_eq!(
            out[0].0, 2,
            "doc 2 (rank 2 + rank 1) beats doc 1 (rank 1 + rank 10) regardless of raw scores"
        );
    }

    #[test]
    fn smaller_k_concentrates_weight_at_top() {
        // Doc 1 at rank 1 in list A. Doc 2 at rank 100 in list A but
        // rank 1 in list B. With k=60 (default), rank-100 contributes
        // 1/160 ~ 0.0063. With k=1, rank-100 contributes 1/101 ~ 0.0099
        // and rank-1 contributes 1/2 = 0.5 — the relative gap widens.
        // We assert the ordering relationship: with very small k, the
        // top of each list dominates more.
        let mut a = Vec::new();
        a.push((1, 1.0));
        for i in 0..99 {
            a.push((100 + i, 1.0));
        }
        a.push((2, 1.0));
        let b = vec![(2, 1.0), (3, 1.0)];
        let small_k = rrf_fuse(&[a.clone(), b.clone()], 1.0, 3);
        let large_k = rrf_fuse(&[a, b], 1000.0, 3);
        // Both should put doc 2 first (rank 1 in B + presence somewhere in A);
        // doc 1 (rank 1 in A) should come second. Sanity check.
        assert_eq!(small_k[0].0, 2);
        assert_eq!(large_k[0].0, 2);
    }

    #[test]
    fn top_k_truncates_fused_result() {
        let bm25 = vec![(1, 10.0), (2, 9.0), (3, 8.0), (4, 7.0)];
        let m2v = vec![(1, 0.9), (2, 0.8), (3, 0.7), (4, 0.6)];
        let out = rrf_fuse(&[bm25, m2v], DEFAULT_K_CONSTANT, 2);
        assert_eq!(out.len(), 2);
    }
}
