//! Persisted Model2Vec embedding index.
//!
//! On first `sigil semantic --m2v` query against a workspace, we encode
//! every searchable entity's `name + qualified_name + sig + doc` text
//! and persist the resulting vectors at:
//!
//!   `.sigil/embeddings.bin`       — flat little-endian f32 row-major matrix
//!   `.sigil/embeddings.meta.json` — schema v2:
//!                                   { schema_version, model_name, dim,
//!                                     n_entities, entries: [{key,
//!                                     text_hash}, ...] }
//!
//! Subsequent queries skip the corpus encoding entirely: load the matrix
//! (~3.5 MB for sigil-on-sigil — scales with `entity_count × 256 dim
//! × 4 bytes`), encode just the query (~µs), score by cosine similarity
//! against every row (~ms). Per-query latency drops from ~100 ms
//! (in-memory rebuild) to single-digit ms.
//!
//! Staleness is detected by comparing the persisted entry keys against
//! the keys derived from the current `entities.jsonl`. A mismatch triggers
//! a full rebuild. Mismatch can be caused by any of: re-indexing surfaced
//! new entities, model changed, model dim changed. Within an unchanged
//! key set, `build_incremental` further compares per-entry `text_hash`
//! so individual unchanged entities skip re-encoding.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 2;
pub const META_FILENAME: &str = "embeddings.meta.json";
pub const VECTORS_FILENAME: &str = "embeddings.bin";

/// One row of metadata per cached vector.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    /// `<file>:<line_start>:<name>` — uniquely identifies an entity in
    /// the workspace.
    pub key: String,
    /// BLAKE3-16 of the `entity_text` that was encoded into the vector.
    /// Lets `build_incremental` reuse the cached vector when the entity's
    /// indexed text (name + qualified_name + sig + doc) hasn't changed.
    pub text_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub schema_version: u32,
    pub model_name: String,
    pub dim: usize,
    pub n_entities: usize,
    pub entries: Vec<Entry>,
}

impl Meta {
    pub fn entity_keys(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.key.clone()).collect()
    }
}

/// 16-hex-char BLAKE3 of a string — same truncation sigil uses for its
/// struct/body/sig hashes. Cheap to compute, ~zero collision risk for
/// realistic per-entity text.
pub fn text_hash(text: &str) -> String {
    let h = blake3::hash(text.as_bytes());
    hex_truncated(h.as_bytes(), 16)
}

fn hex_truncated(bytes: &[u8], n_chars: usize) -> String {
    let need = (n_chars + 1) / 2;
    let mut s = String::with_capacity(n_chars);
    for b in &bytes[..need.min(bytes.len())] {
        s.push_str(&format!("{:02x}", b));
    }
    s.truncate(n_chars);
    s
}

pub struct M2vIndex {
    pub meta: Meta,
    /// Row-major `n_entities × dim` matrix. L2-normalized at build time.
    pub vectors: Vec<f32>,
}

impl M2vIndex {
    /// In-memory ctor for tests / fresh builds. `vectors` must have length
    /// `entries.len() × dim` and be L2-normalized row-wise by the caller.
    pub fn new(
        model_name: String,
        dim: usize,
        entries: Vec<Entry>,
        vectors: Vec<f32>,
    ) -> Self {
        let meta = Meta {
            schema_version: SCHEMA_VERSION,
            model_name,
            dim,
            n_entities: entries.len(),
            entries,
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
    /// either file is missing OR the schema version doesn't match —
    /// caller should then rebuild from scratch.
    pub fn load_from(sigil_dir: &Path) -> Result<Option<Self>> {
        let meta_path = sigil_dir.join(META_FILENAME);
        let vectors_path = sigil_dir.join(VECTORS_FILENAME);
        if !meta_path.exists() || !vectors_path.exists() {
            return Ok(None);
        }
        // Two-phase parse: peek the schema_version first so legacy v1
        // files (different shape — no `entries` array) don't error out;
        // they're treated as "no cache" and rebuilt.
        let raw_text = std::fs::read_to_string(&meta_path)
            .with_context(|| format!("read {}", meta_path.display()))?;
        let probe: serde_json::Value = serde_json::from_str(&raw_text)
            .with_context(|| format!("parse {} as JSON", meta_path.display()))?;
        let version = probe.get("schema_version").and_then(|v| v.as_u64()).unwrap_or(0);
        if version != SCHEMA_VERSION as u64 {
            return Ok(None);
        }
        let meta: Meta = serde_json::from_str(&raw_text)
            .with_context(|| format!("parse {}", meta_path.display()))?;
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
        if self.meta.n_entities != expected_keys.len() {
            return true;
        }
        self.meta
            .entries
            .iter()
            .zip(expected_keys)
            .any(|(e, k)| e.key != *k)
    }

    /// Map from `entity_key` → (row index in vectors, cached text_hash).
    /// Used by build_incremental to look up reusable vectors.
    fn key_to_row_and_hash(&self) -> std::collections::HashMap<&str, (usize, &str)> {
        self.meta
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.key.as_str(), (i, e.text_hash.as_str())))
            .collect()
    }

    /// Slice the vector row for entity `i`.
    fn row(&self, i: usize) -> &[f32] {
        let d = self.meta.dim;
        &self.vectors[i * d..(i + 1) * d]
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

/// Summary of what `build_incremental` did. Useful for logs / progress
/// summaries and for incremental-eval tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BuildStats {
    pub cached: usize,
    pub encoded: usize,
    pub total: usize,
}

/// Phase callback for streaming progress during an incremental build.
/// Called once per processed entity with cumulative counts.
pub type ProgressFn<'a> = &'a mut dyn FnMut(BuildStats);

/// Build (or rebuild) a corpus embedding index, reusing vectors from
/// `old` when (key, text_hash) match. Encoder is supplied by the caller
/// (so tests can stub it; production passes `Model2Vec::encode`).
///
/// Output order matches the order of `entities`. Caller is responsible
/// for persisting the result via `write_to`.
pub fn build_incremental(
    model_name: &str,
    dim: usize,
    old: Option<&M2vIndex>,
    entities: &[(String, String)],
    mut encode: impl FnMut(&str) -> Vec<f32>,
    mut on_progress: Option<ProgressFn>,
) -> (M2vIndex, BuildStats) {
    let lookup = old.map(|o| o.key_to_row_and_hash());
    let mut entries = Vec::with_capacity(entities.len());
    let mut vectors = Vec::with_capacity(entities.len() * dim);
    let mut stats = BuildStats { cached: 0, encoded: 0, total: entities.len() };
    for (key, text) in entities {
        let th = text_hash(text);
        let reused = lookup
            .as_ref()
            .and_then(|m| m.get(key.as_str()))
            .filter(|(_, h)| **h == th);
        match reused {
            Some(&(row, _)) => {
                vectors.extend_from_slice(old.unwrap().row(row));
                stats.cached += 1;
            }
            None => {
                let v = encode(text);
                debug_assert_eq!(v.len(), dim);
                vectors.extend(v);
                stats.encoded += 1;
            }
        }
        entries.push(Entry { key: key.clone(), text_hash: th });
        if let Some(cb) = on_progress.as_deref_mut() {
            cb(stats);
        }
    }
    (
        M2vIndex::new(model_name.to_string(), dim, entries, vectors),
        stats,
    )
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
                Entry { key: "a.rs:1:foo".to_string(), text_hash: "h_foo".to_string() },
                Entry { key: "a.rs:10:bar".to_string(), text_hash: "h_bar".to_string() },
                Entry { key: "b.rs:5:baz".to_string(), text_hash: "h_baz".to_string() },
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
        assert_eq!(loaded.meta.entity_keys(), idx.meta.entity_keys());
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
            br#"{"schema_version":99,"model_name":"x","dim":4,"n_entities":0,"entries":[]}"#,
        )
        .unwrap();
        std::fs::write(dir.join(VECTORS_FILENAME), b"").unwrap();
        assert!(M2vIndex::load_from(&dir).unwrap().is_none());
    }

    #[test]
    fn load_returns_none_for_v1_schema() {
        // Old v1 file shape: { entity_keys: [...] } without per-entry text_hash.
        // We trigger a full rebuild rather than maintain a migration path —
        // it costs ~2 s once and gets us out of the legacy format.
        let dir = tmp_sigil_dir();
        std::fs::write(
            dir.join(META_FILENAME),
            br#"{"schema_version":1,"model_name":"x","dim":4,"n_entities":0,"entity_keys":[]}"#,
        )
        .unwrap();
        std::fs::write(dir.join(VECTORS_FILENAME), b"").unwrap();
        assert!(M2vIndex::load_from(&dir).unwrap().is_none());
    }

    #[test]
    fn stale_when_entity_keys_differ() {
        let idx = sample_index();
        let same = idx.meta.entity_keys();
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
        let keys = idx.meta.entity_keys();
        assert!(idx.is_stale_for(&keys, "different-model", 4));
    }

    #[test]
    fn stale_when_dim_changes() {
        let idx = sample_index();
        let keys = idx.meta.entity_keys();
        assert!(idx.is_stale_for(&keys, "test-model", 999));
    }

    #[test]
    fn stale_when_entity_count_grows() {
        let idx = sample_index();
        let mut more = idx.meta.entity_keys();
        more.push("c.rs:1:new".to_string());
        assert!(idx.is_stale_for(&more, "test-model", 4));
    }

    #[test]
    fn text_hash_is_deterministic_and_distinguishes_inputs() {
        assert_eq!(text_hash("hello"), text_hash("hello"));
        assert_ne!(text_hash("hello"), text_hash("hello world"));
        assert_eq!(text_hash("hello").len(), 16);
    }

    // --- build_incremental --------------------------------------------

    /// Tiny fake encoder for tests — deterministic per-text vector.
    /// Hash bytes into the first `dim` slots, then L2-normalise.
    fn fake_encode(text: &str, dim: usize) -> Vec<f32> {
        let bytes = blake3::hash(text.as_bytes());
        let mut v: Vec<f32> = bytes.as_bytes().iter().take(dim).map(|b| *b as f32).collect();
        while v.len() < dim {
            v.push(0.0);
        }
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v { *x /= norm; }
        }
        v
    }

    fn build_via_encoder(
        old: Option<&M2vIndex>,
        entities: &[(String, String)],
        dim: usize,
    ) -> (M2vIndex, BuildStats) {
        build_incremental(
            "test-model",
            dim,
            old,
            entities,
            |t| fake_encode(t, dim),
            None,
        )
    }

    #[test]
    fn incremental_full_reuse_when_nothing_changed() {
        let entities = vec![
            ("a.rs:1:foo".to_string(), "doc for foo".to_string()),
            ("a.rs:10:bar".to_string(), "doc for bar".to_string()),
        ];
        let (old, _) = build_via_encoder(None, &entities, 4);
        let (new, stats) = build_via_encoder(Some(&old), &entities, 4);
        assert_eq!(stats.cached, 2);
        assert_eq!(stats.encoded, 0);
        assert_eq!(old.vectors, new.vectors);
    }

    #[test]
    fn incremental_encodes_only_changed_entities() {
        let v1 = vec![
            ("a.rs:1:foo".to_string(), "doc for foo".to_string()),
            ("a.rs:10:bar".to_string(), "doc for bar".to_string()),
        ];
        let (old, _) = build_via_encoder(None, &v1, 4);
        let v2 = vec![
            ("a.rs:1:foo".to_string(), "doc for foo".to_string()),         // unchanged
            ("a.rs:10:bar".to_string(), "EDITED doc for bar".to_string()), // changed
        ];
        let (_, stats) = build_via_encoder(Some(&old), &v2, 4);
        assert_eq!(stats.cached, 1, "foo should be reused");
        assert_eq!(stats.encoded, 1, "bar should be re-encoded");
    }

    #[test]
    fn incremental_encodes_new_entities_only() {
        let v1 = vec![("a.rs:1:foo".to_string(), "doc for foo".to_string())];
        let (old, _) = build_via_encoder(None, &v1, 4);
        let v2 = vec![
            ("a.rs:1:foo".to_string(), "doc for foo".to_string()),         // unchanged
            ("b.rs:1:new_helper".to_string(), "doc for helper".to_string()), // new
        ];
        let (_, stats) = build_via_encoder(Some(&old), &v2, 4);
        assert_eq!(stats.cached, 1);
        assert_eq!(stats.encoded, 1);
    }

    #[test]
    fn on_progress_callback_is_invoked_per_entity() {
        let entities = vec![
            ("a.rs:1:foo".to_string(), "doc foo".to_string()),
            ("a.rs:10:bar".to_string(), "doc bar".to_string()),
            ("a.rs:20:baz".to_string(), "doc baz".to_string()),
        ];
        let mut calls: Vec<BuildStats> = Vec::new();
        let mut cb = |s: BuildStats| calls.push(s);
        let (_, stats) = build_incremental(
            "test-model",
            4,
            None,
            &entities,
            |t| fake_encode(t, 4),
            Some(&mut cb),
        );
        assert_eq!(calls.len(), 3, "callback should fire once per entity");
        // Last invocation reports cumulative totals.
        assert_eq!(calls.last().unwrap().total, 3);
        assert_eq!(calls.last().unwrap().encoded + calls.last().unwrap().cached, 3);
        assert_eq!(stats.total, 3);
    }

    #[test]
    fn on_progress_distinguishes_cached_vs_encoded() {
        let v1 = vec![
            ("a.rs:1:foo".to_string(), "doc foo".to_string()),
            ("a.rs:10:bar".to_string(), "doc bar".to_string()),
        ];
        let (old, _) = build_via_encoder(None, &v1, 4);
        let v2 = vec![
            ("a.rs:1:foo".to_string(), "doc foo".to_string()),         // cache hit
            ("a.rs:10:bar".to_string(), "EDITED bar".to_string()),     // re-encode
            ("a.rs:20:new".to_string(), "doc new".to_string()),        // fresh encode
        ];
        let mut last: Option<BuildStats> = None;
        let mut cb = |s: BuildStats| last = Some(s);
        let _ = build_incremental(
            "test-model",
            4,
            Some(&old),
            &v2,
            |t| fake_encode(t, 4),
            Some(&mut cb),
        );
        let final_stats = last.expect("callback fired");
        assert_eq!(final_stats.cached, 1, "foo (unchanged) should be cached");
        assert_eq!(final_stats.encoded, 2, "bar (edited) + new (fresh) should encode");
        assert_eq!(final_stats.total, 3);
    }

    #[test]
    fn incremental_handles_deletion_via_layout_rebuild() {
        let v1 = vec![
            ("a.rs:1:foo".to_string(), "doc for foo".to_string()),
            ("a.rs:10:bar".to_string(), "doc for bar".to_string()),
            ("a.rs:20:baz".to_string(), "doc for baz".to_string()),
        ];
        let (old, _) = build_via_encoder(None, &v1, 4);
        // Drop bar; foo and baz remain identical.
        let v2 = vec![
            ("a.rs:1:foo".to_string(), "doc for foo".to_string()),
            ("a.rs:20:baz".to_string(), "doc for baz".to_string()),
        ];
        let (new, stats) = build_via_encoder(Some(&old), &v2, 4);
        assert_eq!(stats.cached, 2);
        assert_eq!(stats.encoded, 0);
        assert_eq!(new.meta.n_entities, 2);
        assert_eq!(new.meta.entries[0].key, "a.rs:1:foo");
        assert_eq!(new.meta.entries[1].key, "a.rs:20:baz");
        // Vectors should be the same per-entity even though the row index
        // for baz is now 1 (was 2 in the old layout).
        assert_eq!(&new.row(0), &old.row(0));
        assert_eq!(&new.row(1), &old.row(2));
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
