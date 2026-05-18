//! Persisted Model2Vec embedding index.
//!
//! On first `sigil semantic --m2v` query against a workspace, we encode
//! every searchable entity's `name + qualified_name + sig + doc` text
//! and persist the resulting vectors at:
//!
//!   `.sigil/embeddings.bin`       — flat little-endian f32 row-major matrix
//!   `.sigil/embeddings.meta.json` — schema_version, model, dim, entity_keys
//!
//! Subsequent queries skip the corpus encoding entirely: load the matrix
//! (~30 MB for sigil-on-sigil), encode just the query (~µs), score by
//! cosine similarity against every row (~ms). Per-query latency drops
//! from ~100 ms (in-memory rebuild) to single-digit ms.
//!
//! Staleness is detected by comparing the persisted `entity_keys` against
//! the keys derived from the current `entities.jsonl`. A mismatch triggers
//! a full rebuild. Mismatch can be caused by any of: re-indexing surfaced
//! new entities, model changed, model dim changed.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 1;
pub const META_FILENAME: &str = "embeddings.meta.json";
pub const VECTORS_FILENAME: &str = "embeddings.bin";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub schema_version: u32,
    pub model_name: String,
    pub dim: usize,
    pub n_entities: usize,
    pub entity_keys: Vec<String>,
}

pub struct M2vIndex {
    pub meta: Meta,
    /// Row-major `n_entities × dim` matrix. L2-normalized at build time.
    pub vectors: Vec<f32>,
}

impl M2vIndex {
    /// In-memory ctor for tests / fresh builds. `vectors` must have length
    /// `n_entities × dim` and be L2-normalized row-wise by the caller.
    pub fn new(
        model_name: String,
        dim: usize,
        entity_keys: Vec<String>,
        vectors: Vec<f32>,
    ) -> Self {
        let meta = Meta {
            schema_version: SCHEMA_VERSION,
            model_name,
            dim,
            n_entities: entity_keys.len(),
            entity_keys,
        };
        debug_assert_eq!(vectors.len(), meta.n_entities * meta.dim);
        Self { meta, vectors }
    }

    /// Persist the index to `<sigil_dir>/embeddings.{bin,meta.json}`.
    pub fn write_to(&self, sigil_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(sigil_dir).with_context(|| {
            format!("create {}", sigil_dir.display())
        })?;
        // Vectors first — meta.json is the commit point. If meta.json
        // is absent or unreadable we treat the cache as cold.
        let vectors_path = sigil_dir.join(VECTORS_FILENAME);
        let mut f = File::create(&vectors_path)
            .with_context(|| format!("create {}", vectors_path.display()))?;
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                self.vectors.as_ptr() as *const u8,
                self.vectors.len() * std::mem::size_of::<f32>(),
            )
        };
        f.write_all(bytes)
            .with_context(|| format!("write {}", vectors_path.display()))?;
        let meta_path = sigil_dir.join(META_FILENAME);
        let meta_json = serde_json::to_vec_pretty(&self.meta)?;
        std::fs::write(&meta_path, meta_json)
            .with_context(|| format!("write {}", meta_path.display()))?;
        Ok(())
    }

    /// Load `<sigil_dir>/embeddings.{bin,meta.json}`. Returns None when
    /// either file is missing — caller should then rebuild.
    pub fn load_from(sigil_dir: &Path) -> Result<Option<Self>> {
        let meta_path = sigil_dir.join(META_FILENAME);
        let vectors_path = sigil_dir.join(VECTORS_FILENAME);
        if !meta_path.exists() || !vectors_path.exists() {
            return Ok(None);
        }
        let meta: Meta = serde_json::from_reader(BufReader::new(
            File::open(&meta_path).with_context(|| format!("open {}", meta_path.display()))?,
        ))
        .with_context(|| format!("parse {}", meta_path.display()))?;
        if meta.schema_version != SCHEMA_VERSION {
            return Ok(None);
        }
        let expected_bytes = meta.n_entities * meta.dim * std::mem::size_of::<f32>();
        let mut buf = Vec::with_capacity(expected_bytes);
        File::open(&vectors_path)
            .with_context(|| format!("open {}", vectors_path.display()))?
            .read_to_end(&mut buf)
            .with_context(|| format!("read {}", vectors_path.display()))?;
        if buf.len() != expected_bytes {
            return Err(anyhow!(
                "{} size mismatch: expected {expected_bytes}, got {}",
                vectors_path.display(),
                buf.len()
            ));
        }
        let mut vectors: Vec<f32> = Vec::with_capacity(meta.n_entities * meta.dim);
        for chunk in buf.chunks_exact(4) {
            vectors.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
        Ok(Some(Self { meta, vectors }))
    }

    /// True if the persisted index does not match the workspace state.
    /// Triggers a full rebuild when entity set, dim, or model changed.
    pub fn is_stale_for(&self, expected_keys: &[String], expected_model: &str, expected_dim: usize) -> bool {
        if self.meta.model_name != expected_model {
            return true;
        }
        if self.meta.dim != expected_dim {
            return true;
        }
        if self.meta.entity_keys.len() != expected_keys.len() {
            return true;
        }
        self.meta.entity_keys.iter().zip(expected_keys).any(|(a, b)| a != b)
    }

    /// Cosine similarity against every row, top-k by score.
    /// Vectors are L2-normalized, so cosine == dot product.
    /// Returns `Vec<(entity_idx, score)>` sorted descending.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(usize, f32)> {
        let dim = self.meta.dim;
        debug_assert_eq!(query.len(), dim);
        let mut scored: Vec<(usize, f32)> = (0..self.meta.n_entities)
            .map(|i| {
                let row = &self.vectors[i * dim..(i + 1) * dim];
                let s = row.iter().zip(query).map(|(a, b)| a * b).sum::<f32>();
                (i, s)
            })
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(k);
        scored
    }
}

/// Compose the cache-key for an entity. Matches the `name`-collision
/// disambiguation we use elsewhere: file + line_start + name uniquely
/// identifies an entity within a workspace.
pub fn entity_key(file: &str, line_start: u32, name: &str) -> String {
    format!("{file}:{line_start}:{name}")
}

/// `.sigil/` directory at the workspace root.
pub fn sigil_dir(root: &Path) -> PathBuf {
    root.join(".sigil")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    fn tmp_sigil_dir() -> PathBuf {
        let id = N.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let d = std::env::temp_dir().join(format!("sigil-m2v-idx-{pid}-{id}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sample_index() -> M2vIndex {
        // 3 entities, dim=4. Vectors are deliberately distinguishable.
        M2vIndex::new(
            "test-model".to_string(),
            4,
            vec![
                "a.rs:1:foo".to_string(),
                "a.rs:10:bar".to_string(),
                "b.rs:5:baz".to_string(),
            ],
            vec![
                1.0, 0.0, 0.0, 0.0, // foo
                0.0, 1.0, 0.0, 0.0, // bar
                0.0, 0.0, 1.0, 0.0, // baz
            ],
        )
    }

    #[test]
    fn write_then_load_round_trips() {
        let dir = tmp_sigil_dir();
        let idx = sample_index();
        idx.write_to(&dir).unwrap();
        let loaded = M2vIndex::load_from(&dir).unwrap().expect("found on disk");
        assert_eq!(loaded.meta.schema_version, SCHEMA_VERSION);
        assert_eq!(loaded.meta.model_name, "test-model");
        assert_eq!(loaded.meta.dim, 4);
        assert_eq!(loaded.meta.entity_keys, idx.meta.entity_keys);
        assert_eq!(loaded.vectors, idx.vectors);
    }

    #[test]
    fn load_returns_none_when_files_missing() {
        let dir = tmp_sigil_dir();
        assert!(M2vIndex::load_from(&dir).unwrap().is_none());
    }

    #[test]
    fn load_returns_none_when_schema_changes() {
        let dir = tmp_sigil_dir();
        // Manually drop a meta.json with a different schema_version.
        std::fs::write(
            dir.join(META_FILENAME),
            br#"{"schema_version":99,"model_name":"x","dim":4,"n_entities":0,"entity_keys":[]}"#,
        )
        .unwrap();
        std::fs::write(dir.join(VECTORS_FILENAME), b"").unwrap();
        assert!(M2vIndex::load_from(&dir).unwrap().is_none());
    }

    #[test]
    fn stale_when_entity_keys_differ() {
        let idx = sample_index();
        let same = idx.meta.entity_keys.clone();
        assert!(
            !idx.is_stale_for(&same, "test-model", 4),
            "identical keys + model + dim should be fresh"
        );
        let mut changed = same.clone();
        changed[0] = "a.rs:1:foo_RENAMED".to_string();
        assert!(idx.is_stale_for(&changed, "test-model", 4));
    }

    #[test]
    fn stale_when_model_changes() {
        let idx = sample_index();
        assert!(idx.is_stale_for(&idx.meta.entity_keys, "different-model", 4));
    }

    #[test]
    fn stale_when_dim_changes() {
        let idx = sample_index();
        assert!(idx.is_stale_for(&idx.meta.entity_keys, "test-model", 999));
    }

    #[test]
    fn stale_when_entity_count_grows() {
        let idx = sample_index();
        let mut more = idx.meta.entity_keys.clone();
        more.push("c.rs:1:new".to_string());
        assert!(idx.is_stale_for(&more, "test-model", 4));
    }

    #[test]
    fn search_returns_topk_by_cosine() {
        let idx = sample_index();
        // Query matches `foo` exactly; expect foo > bar = baz.
        let q = vec![1.0, 0.0, 0.0, 0.0];
        let hits = idx.search(&q, 3);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].0, 0, "foo (row 0) should rank first");
        assert!((hits[0].1 - 1.0).abs() < 1e-6);
    }

    #[test]
    fn search_respects_k_limit() {
        let idx = sample_index();
        let q = vec![1.0, 0.0, 0.0, 0.0];
        assert_eq!(idx.search(&q, 1).len(), 1);
        assert_eq!(idx.search(&q, 2).len(), 2);
    }
}
