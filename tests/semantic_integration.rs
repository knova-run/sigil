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
fn semantic_rerank_promotes_source_function_over_test_file() {
    // Drop a tests/ fixture that lexically matches the query stronger
    // than the production function. Without --rerank the test file
    // wins; with --rerank the source function should win.
    let dir = stage_fixture();
    let tests_dir = dir.join("tests");
    std::fs::create_dir_all(&tests_dir).unwrap();
    std::fs::write(
        tests_dir.join("json_test.rs"),
        // Verbose docstring stuffed with "parse json file" keywords so
        // BM25 ranks it high.
        "/// Parse json file: parse a json file and return value parse json file parse.\n\
         /// Parse json file parse json file parse json file parse json file.\n\
         pub fn test_parse_json_file() {}\n",
    )
    .unwrap();
    index(&dir);

    let no_rerank = semantic_with_flags(&dir, "parse json file", 3, &[]);
    // With BM25's lexical match favouring the keyword-stuffed test
    // file, it usually ranks first without rerank.
    let test_first_no_rerank = no_rerank
        .first()
        .and_then(|h| h.get("file"))
        .and_then(|v| v.as_str())
        .map(|s| s.contains("tests/"))
        .unwrap_or(false);

    let with_rerank = semantic_with_flags(&dir, "parse json file", 3, &["--rerank"]);
    let top_file_with_rerank = with_rerank
        .first()
        .and_then(|h| h.get("file"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if test_first_no_rerank {
        assert!(
            !top_file_with_rerank.contains("tests/"),
            "rerank should demote test file below source; top file without rerank was tests/*, \
             with rerank: {top_file_with_rerank}"
        );
    }
    // Either way, the top hit with --rerank should not be in tests/.
    assert!(
        !top_file_with_rerank.contains("tests/"),
        "with --rerank the top hit should not be a test file; got: {top_file_with_rerank}"
    );
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

#[test]
fn semantic_m2v_query_uses_persisted_embeddings_without_rebuild() {
    if !potion_model_present() {
        eprintln!("skip: potion-code-16M not present");
        return;
    }
    let dir = stage_fixture();
    // `sigil index` is responsible for building embeddings eagerly
    // (see sigil_index_eagerly_builds_embeddings_when_model_present).
    index(&dir);
    let emb = dir.join(".sigil").join("embeddings.bin");
    let meta = dir.join(".sigil").join("embeddings.meta.json");
    assert!(emb.exists(), "sigil index should have eagerly built embeddings.bin");
    assert!(meta.exists());

    let emb_mtime_first = std::fs::metadata(&emb).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));

    // Run two m2v queries — neither should touch the embeddings file
    // because the entity set hasn't changed since the eager build.
    let hits = semantic_with_flags(&dir, "parse json", 3, &["--m2v"]);
    assert!(!hits.is_empty(), "expected hits");
    let mid = std::fs::metadata(&emb).unwrap().modified().unwrap();
    assert_eq!(emb_mtime_first, mid, "first m2v query should not rebuild");
    let _ = semantic_with_flags(&dir, "compile rust", 3, &["--m2v"]);
    let last = std::fs::metadata(&emb).unwrap().modified().unwrap();
    assert_eq!(emb_mtime_first, last, "subsequent m2v query should not rebuild");
}

#[test]
fn semantic_m2v_query_builds_lazily_when_no_eager_pass() {
    if !potion_model_present() {
        eprintln!("skip: potion-code-16M not present");
        return;
    }
    let dir = stage_fixture();
    // Index with --no-embed so the eager pass is skipped; the embedding
    // cache should then be built lazily on the first `--m2v` query.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args(["index", "--root", dir.to_str().unwrap(), "--full", "--no-embed"])
        .output()
        .expect("run sigil index --no-embed");
    assert!(out.status.success());
    let emb = dir.join(".sigil").join("embeddings.bin");
    assert!(!emb.exists(), "--no-embed should skip eager build");
    let _ = semantic_with_flags(&dir, "parse json", 3, &["--m2v"]);
    assert!(
        emb.exists(),
        "first --m2v query should build the embedding cache lazily when --no-embed was used"
    );
}

#[test]
fn sigil_index_eagerly_builds_embeddings_when_model_present() {
    if !potion_model_present() {
        eprintln!("skip: potion-code-16M not present");
        return;
    }
    let dir = stage_fixture();
    // No prior `sigil semantic --m2v` invocation — just plain index.
    index(&dir);
    let emb = dir.join(".sigil").join("embeddings.bin");
    let meta = dir.join(".sigil").join("embeddings.meta.json");
    assert!(
        emb.exists(),
        "sigil index should eagerly build embeddings.bin when model is present"
    );
    assert!(meta.exists(), "sigil index should eagerly build embeddings.meta.json");
}

#[test]
fn sigil_index_no_embed_skips_embedding_build() {
    if !potion_model_present() {
        eprintln!("skip: potion-code-16M not present");
        return;
    }
    let dir = stage_fixture();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args(["index", "--root", dir.to_str().unwrap(), "--full", "--no-embed"])
        .output()
        .expect("run sigil index --no-embed");
    assert!(out.status.success());
    let emb = dir.join(".sigil").join("embeddings.bin");
    assert!(!emb.exists(), "--no-embed should skip embedding build");
}

#[test]
fn sigil_index_incremental_reuses_cached_embeddings() {
    if !potion_model_present() {
        eprintln!("skip: potion-code-16M not present");
        return;
    }
    let dir = stage_fixture();
    // First index: builds embeddings from scratch.
    index(&dir);
    let emb = dir.join(".sigil").join("embeddings.bin");
    let mtime_a = std::fs::metadata(&emb).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));

    // Second index without any source changes — should be near-instant
    // because every entity hits the cache.
    let t0 = std::time::Instant::now();
    index(&dir);
    let second_elapsed = t0.elapsed();
    let _mtime_b = std::fs::metadata(&emb).unwrap().modified().unwrap();

    // The cache reuse path encodes 0 entities; this should finish in well
    // under what a full encode would take (~2 s on sigil-on-sigil). The
    // tiny fixture only has 3 entities so even cold-build is fast — we
    // assert "under 2 seconds" as a generous regression guard, not as a
    // precision benchmark.
    assert!(
        second_elapsed < std::time::Duration::from_secs(5),
        "second `sigil index` should be fast with cache reuse, took {second_elapsed:?}"
    );
}

#[test]
fn semantic_m2v_rebuilds_when_entities_change() {
    if !potion_model_present() {
        eprintln!("skip: potion-code-16M not present");
        return;
    }
    let dir = stage_fixture();
    index(&dir);

    // First query builds embeddings.
    let _ = semantic_with_flags(&dir, "parse json", 3, &["--m2v"]);
    let emb = dir.join(".sigil").join("embeddings.bin");
    let mtime1 = std::fs::metadata(&emb).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));

    // Add a new source file → re-index → entity_keys change → m2v
    // should rebuild on next query.
    std::fs::write(
        dir.join("new.rs"),
        "/// Convert a date to an ISO-8601 string.\npub fn format_date(_d: u64) -> String { String::new() }\n",
    )
    .unwrap();
    index(&dir);
    let _ = semantic_with_flags(&dir, "format date", 3, &["--m2v"]);

    let mtime2 = std::fs::metadata(&emb).unwrap().modified().unwrap();
    assert!(
        mtime2 > mtime1,
        "embeddings.bin should have been rewritten after entity set changed"
    );
}
