//! Integration coverage for `Model2Vec` loading + encoding using the
//! real `potion-code-16M` static-embedding model.
//!
//! Skipped if the model isn't present at the resolved cache dir
//! (`$XDG_CACHE_HOME/sigil/models/potion-code-16M/` on Linux,
//! `~/Library/Caches/sigil/models/potion-code-16M/` on macOS).
//! In CI we'll fetch on demand once the `sigil semantic download-model`
//! command lands; today this test verifies the loader against whichever
//! model the dev has cached locally.

use sigil::semantic::m2v::{cosine_sim, default_model_dir, Model2Vec};
use std::path::PathBuf;

fn model_dir() -> Option<PathBuf> {
    let d = default_model_dir()?;
    if d.join("tokenizer.json").exists() && d.join("model.safetensors").exists() {
        Some(d)
    } else {
        None
    }
}

macro_rules! require_model {
    () => {{
        match model_dir() {
            Some(d) => d,
            None => {
                eprintln!(
                    "skip: potion-code-16M not present at {:?}; run `sigil semantic download-model` once it lands",
                    default_model_dir()
                );
                return;
            }
        }
    }};
}

#[test]
fn loads_real_potion_code_16m() {
    let dir = require_model!();
    let m = Model2Vec::from_dir(&dir).expect("from_dir succeeds");
    // potion-code-16M is documented as 256-dim with ~61k vocab.
    assert_eq!(m.dim(), 256, "expected potion-code-16M dim=256");
    assert!(
        m.vocab_size() > 50_000,
        "expected vocab > 50k, got {}",
        m.vocab_size()
    );
}

#[test]
fn encode_yields_correct_shape_and_normalized() {
    let dir = require_model!();
    let m = Model2Vec::from_dir(&dir).unwrap();
    let v = m.encode("parse a json file from disk");
    assert_eq!(v.len(), m.dim());
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-3, "L2 norm expected ~1.0, got {norm}");
}

#[test]
fn encode_is_deterministic() {
    let dir = require_model!();
    let m = Model2Vec::from_dir(&dir).unwrap();
    let a = m.encode("compile rust binary");
    let b = m.encode("compile rust binary");
    assert_eq!(a, b);
}

#[test]
fn semantically_related_texts_score_higher_than_unrelated() {
    // Hand-picked triad: query close to one phrase, far from the other.
    let dir = require_model!();
    let m = Model2Vec::from_dir(&dir).unwrap();
    let query = m.encode("parse json file");
    let related = m.encode("read a JSON document");
    let unrelated = m.encode("compile rust binary");

    let sim_related = cosine_sim(&query, &related);
    let sim_unrelated = cosine_sim(&query, &unrelated);
    assert!(
        sim_related > sim_unrelated,
        "related ({sim_related}) should outscore unrelated ({sim_unrelated})"
    );
}
