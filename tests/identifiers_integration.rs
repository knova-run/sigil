//! Integration tests for `sigil identifiers <text>`.
//!
//! Exercise the public CLI surface: deterministic identifier extraction
//! from arbitrary natural-language text. Used by callers that need to
//! join a question against indexed entity names (e.g. wiki retrieval
//! pipelines that want to promote symbols matching identifiers in the
//! user's question).

use std::process::Command;

fn run_identifiers(text: &str, extra: &[&str]) -> (String, bool) {
    let output = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .arg("identifiers")
        .arg(text)
        .args(extra)
        .output()
        .expect("failed to run sigil");
    let stdout = String::from_utf8(output.stdout).expect("invalid utf8");
    (stdout, output.status.success())
}

fn parse(out: &str) -> Vec<String> {
    serde_json::from_str(out).expect("output should be JSON array of strings")
}

#[test]
fn extracts_camelcase_identifier_from_text() {
    let (out, ok) = run_identifiers("How does NearestCentroid work?", &[]);
    assert!(ok, "expected exit success, got: {}", out);
    assert!(parse(&out).iter().any(|s| s == "NearestCentroid"));
}

#[test]
fn extracts_snake_case_identifier_from_text() {
    let (out, ok) = run_identifiers(
        "explain _local_reachability_density and how it works",
        &[],
    );
    assert!(ok, "expected exit success, got: {}", out);
    assert!(
        parse(&out).iter().any(|s| s == "_local_reachability_density"),
        "expected _local_reachability_density in {}",
        out
    );
}

#[test]
fn rejects_pure_lowercase_english_words() {
    let (out, ok) = run_identifiers(
        "what does the method on the class do",
        &[],
    );
    assert!(ok, "expected exit success, got: {}", out);
    let ids = parse(&out);
    // Bare English words should NOT be promoted as identifiers — they
    // match too broadly against entity names (every code base has a
    // `class` token somewhere) and add noise rather than signal.
    for word in &["method", "class", "what", "does", "the"] {
        assert!(
            !ids.iter().any(|s| s == word),
            "expected {word} to be filtered out, got {ids:?}"
        );
    }
}

#[test]
fn extracts_dotted_path_and_its_segments() {
    let (out, ok) = run_identifiers(
        "what does BaseLabelPropagation.fit do",
        &[],
    );
    assert!(ok, "expected exit success, got: {}", out);
    let ids = parse(&out);
    // Both the full dotted path and its segments are useful matches —
    // the leaf may be all the index has indexed.
    assert!(
        ids.iter().any(|s| s == "BaseLabelPropagation.fit"),
        "expected BaseLabelPropagation.fit in {ids:?}"
    );
    assert!(
        ids.iter().any(|s| s == "BaseLabelPropagation"),
        "expected BaseLabelPropagation segment in {ids:?}"
    );
}
