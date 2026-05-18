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
use crate::semantic::m2v_index::{entity_key, sigil_dir, M2vIndex};
use anyhow::{anyhow, Context, Result};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

const M2V_MODEL_NAME: &str = "potion-code-16M";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retriever {
    Bm25,
    M2v,
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

    let hits: Vec<(usize, f32)> = match opts.retriever {
        Retriever::Bm25 => {
            let docs: Vec<(String, String)> = entities
                .iter()
                .enumerate()
                .map(|(i, e)| (i.to_string(), entity_text(e, opts.include_doc)))
                .collect();
            let idx = Index::build(docs);
            idx.search(&opts.query, opts.limit)
                .into_iter()
                .map(|(id, score)| (id.parse::<usize>().unwrap(), score))
                .collect()
        }
        Retriever::M2v => rank_by_m2v(root, &entities, &opts.query, opts.limit, opts.include_doc)?,
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

    // Production path: persist embeddings under .sigil/ keyed by the
    // current entity set. First query is slow (encode all entities);
    // subsequent queries load from disk and only encode the query.
    let expected_keys: Vec<String> = entities
        .iter()
        .map(|e| entity_key(&e.file, e.line_start, &e.name))
        .collect();
    let sigil_d = sigil_dir(root);
    let needs_rebuild = match M2vIndex::load_from(&sigil_d)? {
        Some(idx) if !idx.is_stale_for(&expected_keys, M2V_MODEL_NAME, model.dim()) => {
            return Ok(idx.search(&query_vec, k));
        }
        _ => true,
    };
    if needs_rebuild {
        let dim = model.dim();
        let mut vectors: Vec<f32> = Vec::with_capacity(entities.len() * dim);
        for e in entities {
            let v = model.encode(&entity_text(e, true));
            vectors.extend_from_slice(&v);
        }
        let built = M2vIndex::new(
            M2V_MODEL_NAME.to_string(),
            dim,
            expected_keys,
            vectors,
        );
        built
            .write_to(&sigil_d)
            .with_context(|| format!("persist embeddings under {}", sigil_d.display()))?;
        return Ok(built.search(&query_vec, k));
    }
    unreachable!()
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
