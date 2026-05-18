//! `sigil semantic <query>` handler.
//!
//! Loads `.sigil/entities.jsonl`, filters to source-code kinds (skipping
//! markdown/JSON/external chunks), builds a BM25 index over
//! `name + sig + doc` per entity, then ranks the corpus against the user's
//! query. Output mirrors `sigil search`'s JSON shape with an added `score`
//! field so downstream consumers (the eval harness, agent integrations) can
//! reason about ranking confidence.

use crate::entity::Entity;
use crate::semantic::bm25::Index;
use crate::semantic::m2v::{cosine_sim, default_model_dir, Model2Vec};
use crate::semantic::m2v_index::{build_incremental, entity_key, sigil_dir, BuildStats, M2vIndex};
use anyhow::{anyhow, Context, Result};
use std::fs::File;
use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::path::Path;
use std::time::{Duration, Instant};

const M2V_MODEL_NAME: &str = "potion-code-16M";

/// Refresh `.sigil/embeddings.{bin,meta.json}` for the given entity set
/// using the (key, text_hash) cache to skip re-encoding entities whose
/// `name + qualified_name + sig + doc` text didn't change.
///
/// Returns:
///   - `Ok(true)`  — embeddings written / refreshed
///   - `Ok(false)` — silently skipped (model not installed)
///   - `Err(...)`  — actual error (caller surfaces to stderr)
///
/// Called from `sigil index` at the end of the indexing pass so
/// subsequent `sigil semantic --m2v` queries hit a warm cache.
pub fn refresh_embeddings(
    root: &Path,
    all_entities: &[Entity],
    verbose: bool,
) -> Result<bool> {
    let Some(model_dir) = default_model_dir() else {
        return Ok(false);
    };
    if !model_dir.join("tokenizer.json").exists()
        || !model_dir.join("model.safetensors").exists()
    {
        // Model not installed — skip silently. Users who want m2v will
        // get a clear error message from `sigil semantic --m2v` later.
        return Ok(false);
    }
    // Filter to the same searchable kinds used at query time. Anything
    // we wouldn't return as a hit shouldn't waste a row in the matrix.
    let entities: Vec<&Entity> = all_entities
        .iter()
        .filter(|e| {
            SEARCHABLE_KINDS.contains(&e.kind.as_str()) && e.file != "<external>"
        })
        .collect();
    if entities.is_empty() {
        return Ok(false);
    }
    let model = Model2Vec::from_dir(&model_dir).context("load potion-code-16M")?;
    let dim = model.dim();
    let docs: Vec<(String, String)> = entities
        .iter()
        .map(|e| (entity_key(&e.file, e.line_start, &e.name), entity_text(e, true)))
        .collect();
    let sigil_d = sigil_dir(root);
    let old = M2vIndex::load_from(&sigil_d)?;

    // Progress writer: only active when `verbose`. Throttles to one
    // update per 200 ms (plus a final line at completion) so big
    // corpora don't spam stderr with thousands of lines. TTY gets
    // \r-overwrite for a single live line; piped output gets one line
    // per tick so agents can parse `embed: N/M cached=… encoded=…`
    // out of the stream.
    let stderr_tty = std::io::stderr().is_terminal();
    let mut last_print = Instant::now()
        .checked_sub(Duration::from_millis(500))
        .unwrap_or_else(Instant::now);
    let mut wrote_tty_line = false;
    let mut cb = |s: BuildStats| {
        if !verbose {
            return;
        }
        let done = s.cached + s.encoded;
        let is_last = done == s.total;
        if !is_last && last_print.elapsed() < Duration::from_millis(200) {
            return;
        }
        last_print = Instant::now();
        let pct = if s.total > 0 {
            100.0 * done as f64 / s.total as f64
        } else {
            0.0
        };
        if stderr_tty {
            // Pad to overwrite any leftover characters from a longer
            // previous line. 80 cols is comfortable for the message.
            eprint!(
                "\rembed: {done}/{total} ({pct:5.1}%) cached={cached} encoded={encoded}            ",
                total = s.total,
                cached = s.cached,
                encoded = s.encoded,
            );
            let _ = std::io::stderr().flush();
            wrote_tty_line = true;
            if is_last {
                eprintln!();
            }
        } else {
            eprintln!(
                "embed: {done}/{total} cached={cached} encoded={encoded}",
                total = s.total,
                cached = s.cached,
                encoded = s.encoded,
            );
        }
    };

    let (built, stats) = build_incremental(
        M2V_MODEL_NAME,
        dim,
        old.as_ref(),
        &docs,
        |t| model.encode(t),
        Some(&mut cb),
    );
    // If we printed TTY progress but the last tick didn't land at total
    // (e.g. throttle skipped the last update), close the line.
    if verbose && stderr_tty && wrote_tty_line {
        // We already trail with \n on is_last path; nothing further needed.
    }
    built
        .write_to(&sigil_d)
        .with_context(|| format!("persist embeddings under {}", sigil_d.display()))?;
    if verbose {
        eprintln!(
            "Embedded {} entities (encoded {}, cached {}) → .sigil/embeddings.bin",
            stats.total, stats.encoded, stats.cached
        );
    }
    Ok(true)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retriever {
    Bm25,
    M2v,
    /// Reciprocal Rank Fusion of BM25 + Model2Vec.
    Fuse,
}

const SEARCHABLE_KINDS: &[&str] = &[
    "function",
    "method",
    "class",
    "struct",
    "impl",
    "enum",
    "interface",
    "module",
    "constant",
    "trait",
    "type_alias",
];

pub struct SemanticOptions {
    pub query: String,
    pub limit: usize,
    pub json: bool,
    pub pretty: bool,
    /// When true (default), the entity's `doc` field is part of the
    /// indexed text. When false, doc is excluded — the retriever scores
    /// against `name + qualified_name + sig` only.
    pub include_doc: bool,
    pub retriever: Retriever,
    /// Apply Spike-4 rerank signals (test-file / vendored / kind / rank).
    pub rerank: bool,
}

pub fn run(root: &Path, opts: SemanticOptions) -> Result<()> {
    let entities = load_searchable_entities(root)?;
    if entities.is_empty() {
        if opts.json {
            println!("[]");
        } else {
            eprintln!("sigil: no source-code entities indexed under {}. Run `sigil index` first.", root.display());
        }
        return Ok(());
    }

    // When rerank is active, over-fetch candidates so the rerank
    // multipliers can demote bad hits without losing good ones.
    let candidates_k = if opts.rerank {
        opts.limit * crate::semantic::rerank::CANDIDATE_OVER_FETCH
    } else {
        opts.limit
    };
    let raw_hits: Vec<(usize, f32)> = match opts.retriever {
        Retriever::Bm25 => rank_by_bm25(&entities, &opts.query, candidates_k, opts.include_doc),
        Retriever::M2v => rank_by_m2v(root, &entities, &opts.query, candidates_k, opts.include_doc)?,
        Retriever::Fuse => {
            // RRF fuses each retriever's full candidate-K list. Pull
            // a wider net from each side than we'd give back to the
            // user, so RRF has more positional information to work
            // with — semble does the same.
            let bm25_hits = rank_by_bm25(&entities, &opts.query, candidates_k, opts.include_doc);
            let m2v_hits = rank_by_m2v(root, &entities, &opts.query, candidates_k, opts.include_doc)?;
            crate::semantic::rrf::rrf_fuse(
                &[bm25_hits, m2v_hits],
                crate::semantic::rrf::DEFAULT_K_CONSTANT,
                candidates_k,
            )
        }
    };
    let hits = if opts.rerank {
        let cfg = crate::semantic::rerank::RerankConfig::default();
        crate::semantic::rerank::rerank(&entities, raw_hits, &cfg, opts.limit)
    } else {
        raw_hits
    };

    let rows: Vec<serde_json::Value> = hits
        .into_iter()
        .map(|(i, score)| {
            let e = &entities[i];
            let mut obj = serde_json::Map::new();
            obj.insert("file".into(), serde_json::Value::String(e.file.clone()));
            obj.insert("name".into(), serde_json::Value::String(e.name.clone()));
            obj.insert("kind".into(), serde_json::Value::String(e.kind.clone()));
            obj.insert("line".into(), serde_json::json!(e.line_start));
            if e.line_end != e.line_start {
                obj.insert("line_end".into(), serde_json::json!(e.line_end));
            }
            if let Some(sig) = &e.sig {
                obj.insert("sig".into(), serde_json::Value::String(sig.clone()));
            }
            if let Some(parent) = &e.parent {
                obj.insert("parent".into(), serde_json::Value::String(parent.clone()));
            }
            obj.insert("score".into(), serde_json::json!(round3(score)));
            serde_json::Value::Object(obj)
        })
        .collect();

    if opts.json {
        let s = if opts.pretty {
            serde_json::to_string_pretty(&rows)?
        } else {
            serde_json::to_string(&rows)?
        };
        println!("{s}");
    } else {
        print_text(&rows);
    }
    Ok(())
}

fn load_searchable_entities(root: &Path) -> Result<Vec<Entity>> {
    let path = root.join(".sigil").join("entities.jsonl");
    let f = File::open(&path).with_context(|| {
        format!(
            "missing {} — run `sigil index` first",
            path.display()
        )
    })?;
    let mut out = Vec::new();
    for line in BufReader::new(f).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let entity: Entity = serde_json::from_str(&line)
            .with_context(|| format!("parse entity line: {line}"))?;
        if !SEARCHABLE_KINDS.contains(&entity.kind.as_str()) {
            continue;
        }
        if entity.file == "<external>" {
            continue;
        }
        out.push(entity);
    }
    Ok(out)
}

fn entity_text(e: &Entity, include_doc: bool) -> String {
    // `name + sig + doc` is the minimum-viable representation. Body text
    // would require re-reading the source file — a Spike-1.1 follow-up.
    let mut parts: Vec<&str> = vec![e.name.as_str()];
    if let Some(qn) = &e.qualified_name {
        parts.push(qn);
    }
    if let Some(sig) = &e.sig {
        parts.push(sig);
    }
    if include_doc {
        if let Some(doc) = &e.doc {
            parts.push(doc);
        }
    }
    parts.join(" ")
}

fn round3(x: f32) -> f32 {
    (x * 1000.0).round() / 1000.0
}

fn rank_by_bm25(
    entities: &[Entity],
    query: &str,
    k: usize,
    include_doc: bool,
) -> Vec<(usize, f32)> {
    let docs: Vec<(String, String)> = entities
        .iter()
        .enumerate()
        .map(|(i, e)| (i.to_string(), entity_text(e, include_doc)))
        .collect();
    Index::build(docs)
        .search(query, k)
        .into_iter()
        .map(|(id, score)| (id.parse::<usize>().unwrap(), score))
        .collect()
}

fn rank_by_m2v(
    root: &Path,
    entities: &[Entity],
    query: &str,
    k: usize,
    include_doc: bool,
) -> Result<Vec<(usize, f32)>> {
    let dir = default_model_dir().ok_or_else(|| {
        anyhow!("could not resolve user cache dir for the m2v model")
    })?;
    if !dir.join("tokenizer.json").exists() || !dir.join("model.safetensors").exists() {
        return Err(anyhow!(
            "potion-code-16M not found at {}. Download it manually for now:\n  \
             curl -sL https://huggingface.co/minishlab/potion-code-16M/resolve/main/{{config.json,tokenizer.json,model.safetensors}} -o '{}/#1'\n  \
             (a `sigil semantic download-model` command is on the roadmap)",
            dir.display(),
            dir.display(),
        ));
    }
    let model = Model2Vec::from_dir(&dir).context("load potion-code-16M")?;
    let query_vec = model.encode(query);

    // --no-doc stays on the in-memory (uncached) path — it's a
    // measurement-time flag, not a production retriever shape. Production
    // m2v always uses the persisted full-text index.
    if !include_doc {
        let mut scores: Vec<(usize, f32)> = Vec::with_capacity(entities.len());
        for (i, e) in entities.iter().enumerate() {
            let v = model.encode(&entity_text(e, false));
            scores.push((i, cosine_sim(&query_vec, &v)));
        }
        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scores.truncate(k);
        return Ok(scores);
    }

    // Production path: persist embeddings under .sigil/, reusing rows
    // whose (key, text_hash) match the previous build. First call is
    // slow (encode every entity); subsequent calls only re-encode the
    // entities whose name/sig/doc actually changed.
    let sigil_d = sigil_dir(root);
    let old = M2vIndex::load_from(&sigil_d)?;
    // Cache hit: identical entity set + model + dim AND every entity's
    // text_hash matches → return early without rebuilding.
    let docs: Vec<(String, String)> = entities
        .iter()
        .map(|e| (entity_key(&e.file, e.line_start, &e.name), entity_text(e, true)))
        .collect();
    if let Some(ref old_idx) = old {
        let expected_keys: Vec<String> = docs.iter().map(|(k, _)| k.clone()).collect();
        if !old_idx.is_stale_for(&expected_keys, M2V_MODEL_NAME, model.dim()) {
            // Same key set; verify text_hashes too.
            let all_match = old_idx.meta.entries.iter().zip(&docs).all(|(e, (_, t))| {
                e.text_hash == crate::semantic::m2v_index::text_hash(t)
            });
            if all_match {
                return Ok(old_idx.search(&query_vec, k));
            }
        }
    }
    let (built, _stats) = build_incremental(
        M2V_MODEL_NAME,
        model.dim(),
        old.as_ref(),
        &docs,
        |t| model.encode(t),
        None,
    );
    built
        .write_to(&sigil_d)
        .with_context(|| format!("persist embeddings under {}", sigil_d.display()))?;
    Ok(built.search(&query_vec, k))
}

fn print_text(rows: &[serde_json::Value]) {
    if rows.is_empty() {
        println!("sigil: 0 semantic matches.");
        return;
    }
    for row in rows {
        let file = row.get("file").and_then(|v| v.as_str()).unwrap_or("?");
        let name = row.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let kind = row.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
        let line = row.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
        let score = row.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
        println!("{score:>6.2}  {file}:{line}  [{kind}] {name}");
    }
}
