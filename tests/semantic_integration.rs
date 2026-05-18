//! End-to-end coverage for the `sigil semantic` CLI command (Spike 1).
//!
//! Stages a three-function Rust fixture into a temp dir, runs `sigil index`,
//! then runs `sigil semantic <query> --json`. Asserts that each topic-shaped
//! query ranks the correct entity #1.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn stage_fixture() -> PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let src = PathBuf::from(format!("{}/tests/fixtures/semantic_sample.rs", manifest));
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("sigil-semantic-{pid}-{id}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    std::fs::copy(&src, dir.join("sample.rs")).expect("copy fixture");
    dir
}

fn index(root: &PathBuf) {
    let out = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args(["index", "--root", root.to_str().unwrap(), "--full"])
        .output()
        .expect("run sigil index");
    assert!(
        out.status.success(),
        "sigil index failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn semantic(root: &PathBuf, query: &str, limit: u32) -> Vec<serde_json::Value> {
    semantic_with_flags(root, query, limit, &[])
}

fn semantic_with_flags(
    root: &PathBuf,
    query: &str,
    limit: u32,
    extra: &[&str],
) -> Vec<serde_json::Value> {
    let limit_s = limit.to_string();
    let mut args: Vec<&str> = vec![
        "semantic",
        query,
        "--root",
        root.to_str().unwrap(),
        "--limit",
        &limit_s,
        "--json",
    ];
    args.extend_from_slice(extra);
    let out = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args(&args)
        .output()
        .expect("run sigil semantic");
    assert!(
        out.status.success(),
        "sigil semantic failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("invalid json from sigil semantic: {e}\nstdout: {stdout}"))
}

fn stage_doc_mask_fixture() -> PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let src = PathBuf::from(format!("{}/tests/fixtures/semantic_doc_mask.rs", manifest));
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("sigil-semantic-mask-{pid}-{id}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    std::fs::copy(&src, dir.join("sample.rs")).expect("copy fixture");
    dir
}

fn top_name(hits: &[serde_json::Value]) -> &str {
    hits.first()
        .and_then(|h| h.get("name"))
        .and_then(|n| n.as_str())
        .expect("first hit has name")
}

#[test]
fn semantic_ranks_json_function_first_for_json_query() {
    let dir = stage_fixture();
    index(&dir);
    let hits = semantic(&dir, "parse json file", 5);
    assert!(!hits.is_empty(), "got no hits");
    assert_eq!(top_name(&hits), "parse_json_file");
}

#[test]
fn semantic_ranks_compile_function_first_for_compile_query() {
    let dir = stage_fixture();
    index(&dir);
    let hits = semantic(&dir, "compile rust binary", 5);
    assert!(!hits.is_empty(), "got no hits");
    assert_eq!(top_name(&hits), "compile_rust_binary");
}

#[test]
fn semantic_ranks_http_function_first_for_http_query() {
    let dir = stage_fixture();
    index(&dir);
    let hits = semantic(&dir, "send http request", 5);
    assert!(!hits.is_empty(), "got no hits");
    assert_eq!(top_name(&hits), "send_http_request");
}

#[test]
fn semantic_json_output_carries_required_fields() {
    let dir = stage_fixture();
    index(&dir);
    let hits = semantic(&dir, "json", 5);
    assert!(!hits.is_empty());
    let top = &hits[0];
    for key in ["file", "name", "kind", "line", "score"] {
        assert!(
            top.get(key).is_some(),
            "missing field {key:?} in {top}"
        );
    }
}

#[test]
fn semantic_respects_limit_flag() {
    let dir = stage_fixture();
    index(&dir);
    let hits = semantic(&dir, "json", 1);
    assert_eq!(hits.len(), 1);
}

#[test]
fn semantic_no_doc_flag_excludes_doc_from_indexed_text() {
    let dir = stage_doc_mask_fixture();
    index(&dir);

    // With doc indexed (default): the zebrafish-shaped query lands on
    // lookup_record because only its docstring carries the term.
    let with_doc = semantic(&dir, "zebrafish", 5);
    assert!(!with_doc.is_empty(), "doc-indexed query should hit");
    assert_eq!(top_name(&with_doc), "lookup_record");

    // With --no-doc: the doc is no longer part of the indexed text, so
    // the zebrafish-only query has nothing to match.
    let without_doc = semantic_with_flags(&dir, "zebrafish", 5, &["--no-doc"]);
    assert!(
        without_doc.is_empty(),
        "with --no-doc, zebrafish query should have no hits; got {without_doc:?}"
    );
}

#[test]
fn semantic_no_doc_flag_still_matches_name_and_sig() {
    let dir = stage_doc_mask_fixture();
    index(&dir);

    // Even with --no-doc, a name-shaped query should still resolve.
    let hits = semantic_with_flags(&dir, "lookup record", 5, &["--no-doc"]);
    assert!(!hits.is_empty(), "name-shaped query should hit without doc");
    assert_eq!(top_name(&hits), "lookup_record");
}

// --- Spike 2: Model2Vec embedding retrieval (`--m2v`) -------------------
//
// Gated on potion-code-16M being present at the default model dir.

fn potion_model_present() -> bool {
    match sigil::semantic::m2v::default_model_dir() {
        Some(d) => {
            d.join("tokenizer.json").exists() && d.join("model.safetensors").exists()
        }
        None => false,
    }
}

#[test]
fn semantic_m2v_ranks_topical_match_first() {
    if !potion_model_present() {
        eprintln!("skip: potion-code-16M not present");
        return;
    }
    let dir = stage_fixture();
    index(&dir);

    // Topical query that doesn't share rare identifier tokens with the
    // gold entity's name. Lexical retrieval would struggle; embedding
    // retrieval should find the JSON-parsing function from intent.
    let hits = semantic_with_flags(&dir, "read a document from disk", 3, &["--m2v"]);
    assert!(!hits.is_empty(), "m2v should produce hits");
    assert_eq!(top_name(&hits), "parse_json_file");
}

#[test]
fn semantic_m2v_emits_score_field() {
    if !potion_model_present() {
        eprintln!("skip: potion-code-16M not present");
        return;
    }
    let dir = stage_fixture();
    index(&dir);
    let hits = semantic_with_flags(&dir, "compile rust", 3, &["--m2v"]);
    assert!(!hits.is_empty());
    let top = &hits[0];
    let score = top
        .get("score")
        .and_then(|v| v.as_f64())
        .expect("score field present");
    assert!(score > 0.0, "expected positive cosine sim, got {score}");
    assert!(score <= 1.0001, "cosine sim bounded at 1.0, got {score}");
}
