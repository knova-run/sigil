//! BM25 retrieval over `(doc_id, text)` pairs.
//!
//! Standard Robertson BM25 with k1=1.2, b=0.75 (the defaults Lucene and
//! Elasticsearch ship). Builds once per corpus and queries many times.
//! All scoring is single-threaded; query time scales with the number of
//! distinct query terms × average posting-list length, which is fast for
//! sigil-scale corpora (~30k entities).

use crate::semantic::tokenize::tokenize;
use std::collections::HashMap;

const K1: f32 = 1.2;
const B: f32 = 0.75;

pub struct Index {
    /// term → posting list of (doc_idx, term_frequency)
    postings: HashMap<String, Vec<(usize, u32)>>,
    /// doc_idx → caller-supplied id
    doc_ids: Vec<String>,
    /// doc_idx → token count
    doc_lengths: Vec<u32>,
    avgdl: f32,
    n_docs: usize,
}

impl Index {
    pub fn build<I: IntoIterator<Item = (String, String)>>(docs: I) -> Self {
        let mut postings: HashMap<String, Vec<(usize, u32)>> = HashMap::new();
        let mut doc_ids: Vec<String> = Vec::new();
        let mut doc_lengths: Vec<u32> = Vec::new();
        let mut total_len: u64 = 0;

        for (id, text) in docs {
            let tokens = tokenize(&text);
            let doc_idx = doc_ids.len();
            doc_ids.push(id);
            doc_lengths.push(tokens.len() as u32);
            total_len += tokens.len() as u64;

            let mut tf: HashMap<String, u32> = HashMap::new();
            for tok in tokens {
                *tf.entry(tok).or_insert(0) += 1;
            }
            for (term, freq) in tf {
                postings.entry(term).or_default().push((doc_idx, freq));
            }
        }

        let n_docs = doc_ids.len();
        let avgdl = if n_docs == 0 {
            0.0
        } else {
            total_len as f32 / n_docs as f32
        };

        Self {
            postings,
            doc_ids,
            doc_lengths,
            avgdl,
            n_docs,
        }
    }

    pub fn search(&self, query: &str, k: usize) -> Vec<(String, f32)> {
        if self.n_docs == 0 || k == 0 {
            return Vec::new();
        }
        let query_terms = tokenize(query);
        if query_terms.is_empty() {
            return Vec::new();
        }
        let mut scores: HashMap<usize, f32> = HashMap::new();
        for term in &query_terms {
            let Some(posting) = self.postings.get(term) else {
                continue;
            };
            let df = posting.len() as f32;
            // Robertson IDF (the version that stays non-negative and matches
            // Lucene / sqlite-fts5 BM25).
            let idf = ((self.n_docs as f32 - df + 0.5) / (df + 0.5) + 1.0).ln();
            for &(doc_idx, tf) in posting {
                let dl = self.doc_lengths[doc_idx] as f32;
                let norm = 1.0 - B + B * (dl / self.avgdl);
                let tf = tf as f32;
                let contrib = idf * ((tf * (K1 + 1.0)) / (tf + K1 * norm));
                *scores.entry(doc_idx).or_insert(0.0) += contrib;
            }
        }
        let mut ranked: Vec<(usize, f32)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(k);
        ranked
            .into_iter()
            .map(|(idx, score)| (self.doc_ids[idx].clone(), score))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(id, text)| (id.to_string(), text.to_string()))
            .collect()
    }

    #[test]
    fn empty_corpus_yields_no_hits() {
        let idx = Index::build(docs(&[]));
        assert!(idx.search("anything", 10).is_empty());
    }

    #[test]
    fn doc_containing_term_outranks_doc_without() {
        let idx = Index::build(docs(&[
            ("a", "parse json file"),
            ("b", "compile rust code"),
        ]));
        let hits = idx.search("json", 2);
        assert_eq!(hits[0].0, "a", "doc a contains 'json' and should rank first");
    }

    #[test]
    fn rare_term_outranks_common_term() {
        // "common" appears in all three docs (low IDF). "rare_term" appears
        // only in doc c (high IDF). For the query containing both, doc c
        // wins because it's the only one with the rare signal.
        let idx = Index::build(docs(&[
            ("a", "alpha common common"),
            ("b", "beta common"),
            ("c", "gamma rare common"),
        ]));
        let hits = idx.search("common rare", 3);
        assert_eq!(hits[0].0, "c");
    }

    #[test]
    fn shorter_doc_outranks_longer_doc_same_tf() {
        // Both docs contain "match" exactly once. BM25's length normalization
        // (b=0.75, doc-len in denominator) should rank the shorter doc higher.
        let idx = Index::build(docs(&[
            ("short", "match other"),
            (
                "long",
                "match the the the the the the the the the the the the the",
            ),
        ]));
        let hits = idx.search("match", 2);
        assert_eq!(hits[0].0, "short");
    }

    #[test]
    fn tf_saturates_not_linear() {
        // TF=5 should NOT score 5× TF=1. With k1=1.2, the contribution of
        // tf=5 is (5*(1.2+1))/(5+1.2*(1-b+b*1)) ~ 11/6.2 ~ 1.77, vs tf=1
        // contribution (1*2.2)/(1+1.2) ~ 1.0. Ratio ~1.77×, well under 5×.
        // (Both docs equal length to isolate the TF effect.)
        let idx = Index::build(docs(&[
            ("once", "term filler filler filler filler"),
            ("many", "term term term term term"),
        ]));
        let hits = idx.search("term", 2);
        let many_score = hits.iter().find(|(id, _)| id == "many").unwrap().1;
        let once_score = hits.iter().find(|(id, _)| id == "once").unwrap().1;
        let ratio = many_score / once_score;
        assert!(
            ratio < 3.0,
            "tf=5 should saturate, not dominate 5x. ratio={ratio}"
        );
        assert!(ratio > 1.0, "tf=5 should still outscore tf=1. ratio={ratio}");
    }

    #[test]
    fn multi_term_query_sums_scores() {
        // doc c has both query terms; docs a, b have one each. c should win.
        let idx = Index::build(docs(&[
            ("a", "alpha unrelated"),
            ("b", "beta unrelated"),
            ("c", "alpha beta unrelated"),
        ]));
        let hits = idx.search("alpha beta", 3);
        assert_eq!(hits[0].0, "c");
    }

    #[test]
    fn query_with_no_matching_terms_returns_empty() {
        let idx = Index::build(docs(&[
            ("a", "parse json"),
            ("b", "compile rust"),
        ]));
        let hits = idx.search("xenomorph", 5);
        assert!(hits.is_empty(), "no docs contain 'xenomorph'");
    }

    #[test]
    fn k_limits_result_count() {
        let idx = Index::build(docs(&[
            ("a", "term"),
            ("b", "term"),
            ("c", "term"),
            ("d", "term"),
        ]));
        let hits = idx.search("term", 2);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn identifier_aware_tokenization_matches_camel_case() {
        // Query is plain word "build"; doc contains "buildIndex" identifier.
        // Tokenizer splits both sides, so they match.
        let idx = Index::build(docs(&[
            ("a", "fn buildIndex(root)"),
            ("b", "fn compile_target()"),
        ]));
        let hits = idx.search("build", 2);
        assert_eq!(hits[0].0, "a");
    }
}
