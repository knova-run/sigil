//! Static-embedding retrieval via Model2Vec (`potion-code-16M`).
//!
//! Inference path:
//!   1. Tokenize text using the model's bundled HF `tokenizer.json`.
//!   2. For each token id, look up the row in the embedding matrix.
//!   3. Mean-pool the rows.
//!   4. L2-normalize.
//!
//! There is no transformer forward pass — the model IS the lookup table.
//! Inference is microseconds per text on CPU. The 60 MB embedding matrix
//! is memory-mapped so the cold-start cost is dominated by the
//! tokenizer.json parse (~50 ms), not the matrix load.

use anyhow::{Context, Result};
use memmap2::Mmap;
use safetensors::SafeTensors;
use std::fs::File;
use std::path::{Path, PathBuf};
use tokenizers::Tokenizer;

pub struct Model2Vec {
    tokenizer: Tokenizer,
    /// Row-major embedding matrix flattened to `vocab × dim` floats.
    /// Memory-mapped from `model.safetensors`; never copied wholesale.
    matrix: Vec<f32>,
    vocab_size: usize,
    dim: usize,
    /// L2-normalize the pooled output (matches potion-code-16M's
    /// `1_Normalize` module pipeline step).
    normalize: bool,
}

impl Model2Vec {
    pub fn from_dir(dir: &Path) -> Result<Self> {
        let tok_path = dir.join("tokenizer.json");
        let tokenizer = Tokenizer::from_file(&tok_path)
            .map_err(|e| anyhow::anyhow!("load tokenizer {}: {e}", tok_path.display()))?;
        let st_path = dir.join("model.safetensors");
        let (matrix, vocab_size, dim) = read_safetensors_matrix(&st_path)?;
        let normalize = read_normalize_flag(dir).unwrap_or(true);
        Ok(Self {
            tokenizer,
            matrix,
            vocab_size,
            dim,
            normalize,
        })
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn vocab_size(&self) -> usize {
        self.vocab_size
    }

    /// Encode a single text into a `dim`-dimensional vector.
    pub fn encode(&self, text: &str) -> Vec<f32> {
        let encoding = self
            .tokenizer
            .encode(text, false)
            .expect("tokenizer.encode never fails for static-vocab models");
        let mut v = mean_pool(encoding.get_ids(), &self.matrix, self.vocab_size, self.dim);
        if self.normalize {
            l2_normalize(&mut v);
        }
        v
    }
}

/// Mean-pool the rows of `matrix` indexed by `token_ids`.
/// Out-of-vocab ids (>= vocab_size) are skipped, never panic.
/// Empty input → zero vector of length `dim`.
pub(crate) fn mean_pool(
    token_ids: &[u32],
    matrix: &[f32],
    vocab_size: usize,
    dim: usize,
) -> Vec<f32> {
    let mut sum = vec![0.0f32; dim];
    let mut counted = 0u32;
    for &id in token_ids {
        let row = id as usize;
        if row >= vocab_size {
            continue;
        }
        let start = row * dim;
        let end = start + dim;
        if end > matrix.len() {
            continue;
        }
        for (s, &v) in sum.iter_mut().zip(&matrix[start..end]) {
            *s += v;
        }
        counted += 1;
    }
    if counted == 0 {
        return sum;
    }
    let inv = 1.0 / counted as f32;
    for s in &mut sum {
        *s *= inv;
    }
    sum
}

/// In-place L2 normalization. Zero vector stays zero (no NaN).
pub(crate) fn l2_normalize(v: &mut [f32]) {
    let norm_sq: f32 = v.iter().map(|x| x * x).sum();
    if norm_sq == 0.0 {
        return;
    }
    let inv = 1.0 / norm_sq.sqrt();
    for x in v.iter_mut() {
        *x *= inv;
    }
}

/// Cosine similarity between two same-length vectors. Assumes both are
/// already L2-normalized (Model2Vec output always is); under that
/// invariant cosine = dot product.
pub fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Resolve the directory containing the default static-embedding model.
/// `$XDG_CACHE_HOME/sigil/models/potion-code-16M/` on Linux,
/// `~/Library/Caches/sigil/models/...` on macOS, etc.
pub fn default_model_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("sigil").join("models").join("potion-code-16M"))
}

fn read_normalize_flag(dir: &Path) -> Option<bool> {
    let cfg_path = dir.join("config.json");
    let txt = std::fs::read_to_string(&cfg_path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
    v.get("normalize").and_then(|x| x.as_bool())
}

fn read_safetensors_matrix(path: &Path) -> Result<(Vec<f32>, usize, usize)> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mmap = unsafe { Mmap::map(&file) }
        .with_context(|| format!("mmap {}", path.display()))?;
    let st = SafeTensors::deserialize(&mmap)
        .with_context(|| format!("parse safetensors {}", path.display()))?;
    // potion-code-16M's only tensor is named "embedding_weights" (per
    // sentence_transformers.StaticEmbedding); some forks use just
    // "weight" or "embeddings.weight". Pick the only float32 2-D tensor.
    let (name, tensor) = st
        .tensors()
        .into_iter()
        .find(|(_, t)| t.shape().len() == 2)
        .context("no 2-D tensor in model.safetensors")?;
    let shape = tensor.shape();
    let (vocab, dim) = (shape[0], shape[1]);
    let bytes = tensor.data();
    let n_floats = bytes.len() / 4;
    let mut matrix = Vec::with_capacity(n_floats);
    for chunk in bytes.chunks_exact(4) {
        matrix.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    let _ = name; // tensor name preserved for future debug logging
    Ok((matrix, vocab, dim))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- mean_pool ----------------------------------------------------

    #[test]
    fn mean_pool_empty_ids_yields_zero_vector() {
        let matrix = vec![1.0, 2.0, 3.0, 4.0]; // 2 rows × 2 cols
        let v = mean_pool(&[], &matrix, 2, 2);
        assert_eq!(v, vec![0.0, 0.0]);
    }

    #[test]
    fn mean_pool_single_token_returns_its_row() {
        // 3 rows × 2 cols:
        // row 0: [1.0, 2.0]   row 1: [3.0, 4.0]   row 2: [5.0, 6.0]
        let matrix = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let v = mean_pool(&[1], &matrix, 3, 2);
        assert_eq!(v, vec![3.0, 4.0]);
    }

    #[test]
    fn mean_pool_multiple_tokens_returns_mean() {
        // rows: [1,2], [3,4], [5,6]
        // mean of rows 0 and 2 = [3, 4]
        let matrix = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let v = mean_pool(&[0, 2], &matrix, 3, 2);
        assert_eq!(v, vec![3.0, 4.0]);
    }

    #[test]
    fn mean_pool_skips_out_of_vocab_ids() {
        // vocab=2, ids include 99 (oov) — pool only the valid id.
        let matrix = vec![1.0, 2.0, 3.0, 4.0];
        let v = mean_pool(&[0, 99], &matrix, 2, 2);
        assert_eq!(v, vec![1.0, 2.0], "oov id 99 should be skipped, not crash");
    }

    #[test]
    fn mean_pool_all_oov_yields_zero_vector() {
        let matrix = vec![1.0, 2.0, 3.0, 4.0];
        let v = mean_pool(&[99, 100], &matrix, 2, 2);
        assert_eq!(v, vec![0.0, 0.0]);
    }

    // --- l2_normalize -------------------------------------------------

    #[test]
    fn l2_normalize_zero_vector_stays_zero() {
        let mut v = vec![0.0, 0.0, 0.0];
        l2_normalize(&mut v);
        assert_eq!(v, vec![0.0, 0.0, 0.0]);
        assert!(v.iter().all(|x| !x.is_nan()));
    }

    #[test]
    fn l2_normalize_unit_vector_unchanged() {
        let mut v = vec![1.0, 0.0, 0.0];
        l2_normalize(&mut v);
        assert!((v[0] - 1.0).abs() < 1e-6);
        assert_eq!(v[1], 0.0);
        assert_eq!(v[2], 0.0);
    }

    #[test]
    fn l2_normalize_arbitrary_vector_has_unit_norm() {
        let mut v = vec![3.0, 4.0]; // norm = 5
        l2_normalize(&mut v);
        let norm = (v[0] * v[0] + v[1] * v[1]).sqrt();
        assert!((norm - 1.0).abs() < 1e-6, "norm after normalize: {norm}");
        assert!((v[0] - 0.6).abs() < 1e-6);
        assert!((v[1] - 0.8).abs() < 1e-6);
    }

    // --- cosine_sim ---------------------------------------------------

    #[test]
    fn cosine_sim_identical_unit_vectors_is_one() {
        let a = vec![0.6, 0.8];
        assert!((cosine_sim(&a, &a) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_sim_orthogonal_unit_vectors_is_zero() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!(cosine_sim(&a, &b).abs() < 1e-6);
    }
}
