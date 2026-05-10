//! Integration tests for `sigil decisions` — extract architectural
//! decision markers from source-file comments.
//!
//! The MVP scans for `# DECISION:`, `# WHY:`, `# RATIONALE:`, `# TRADEOFF:`
//! anchors in line-style comments across the supported languages and emits
//! one JSONL row per match.

use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn run_decisions(root: &std::path::Path) -> (String, String, bool) {
    let output = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .arg("decisions")
        .arg("--root")
        .arg(root)
        .output()
        .expect("failed to run sigil");
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    )
}

#[test]
fn does_not_panic_on_non_ascii_comment_prefixes() {
    // Real-world regression: ' / sigil — Structural code fingerprinting'
    // (the em-dash starts at byte 9) caused byte-index slicing to panic.
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("doc.rs"),
        "/// sigil — Structural code fingerprinting\n// DECISION: keep it pure-rust\n",
    )
    .unwrap();
    let (stdout, stderr, ok) = run_decisions(tmp.path());
    assert!(ok, "must not panic on em-dash prefix; stderr: {stderr}");
    let rows: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(
        rows.iter().any(|r| r["marker"] == "DECISION"),
        "expected DECISION row to still extract, got {rows:?}"
    );
}

#[test]
fn recognizes_adr_and_rejected_markers_repowise_compatible() {
    // Repowise's MARKER_RE recognizes WHY|DECISION|TRADEOFF|ADR|RATIONALE|REJECTED.
    // Our extractor must accept the same set so the same source code produces
    // the same set of decision rows under either tool.
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("auth.py"),
        "# ADR: split auth into its own service\n# REJECTED: in-process auth\n",
    )
    .unwrap();
    let (stdout, stderr, ok) = run_decisions(tmp.path());
    assert!(ok, "stderr: {stderr}");
    let markers: Vec<String> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["marker"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert!(markers.contains(&"ADR".to_string()), "expected ADR in {markers:?}");
    assert!(
        markers.contains(&"REJECTED".to_string()),
        "expected REJECTED in {markers:?}"
    );
}

#[test]
fn ignores_non_marker_comments_and_skips_dependency_dirs() {
    let tmp = TempDir::new().unwrap();
    // ordinary comments — no marker, must be skipped
    fs::write(
        tmp.path().join("noise.py"),
        "# just a regular comment\n# TODO: not a decision\nx = 1\n",
    )
    .unwrap();
    // a real marker
    fs::write(
        tmp.path().join("real.py"),
        "# WHY: legacy v1 callers depend on this shape\n",
    )
    .unwrap();
    // anything under node_modules / target / .git must be skipped wholesale
    let buried = tmp.path().join("node_modules");
    fs::create_dir(&buried).unwrap();
    fs::write(
        buried.join("ignored.js"),
        "// DECISION: should not be reported\n",
    )
    .unwrap();
    let target = tmp.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::write(
        target.join("ignored.rs"),
        "// RATIONALE: should not be reported\n",
    )
    .unwrap();

    let (stdout, stderr, ok) = run_decisions(tmp.path());
    assert!(ok, "stderr: {stderr}");
    let rows: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(rows.len(), 1, "expected exactly one decision row, got {rows:?}");
    assert_eq!(rows[0]["marker"], "WHY");
}

#[test]
fn extracts_marker_from_rust_double_slash_comment() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("main.rs"),
        "// TRADEOFF: accepted eventual consistency for write throughput\n\
         fn main() {}\n",
    )
    .unwrap();
    let (stdout, stderr, ok) = run_decisions(tmp.path());
    assert!(ok, "stderr: {stderr}");
    let row: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap())
        .expect("first line should be JSON");
    assert_eq!(row["marker"], "TRADEOFF");
    assert_eq!(
        row["text"], "accepted eventual consistency for write throughput"
    );
}

#[test]
fn extracts_decision_marker_from_python_comment() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("auth.py");
    fs::write(
        &path,
        "# DECISION: JWT chosen over sessions for stateless k8s scaling\n\
         def authenticate():\n    pass\n",
    )
    .unwrap();
    let (stdout, stderr, ok) = run_decisions(tmp.path());
    assert!(ok, "expected success, stderr: {stderr}");
    let lines: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("each line should be JSON"))
        .collect();
    assert!(
        !lines.is_empty(),
        "expected at least one decision row, got: {stdout}"
    );
    let row = &lines[0];
    assert_eq!(row["marker"], "DECISION");
    assert_eq!(
        row["text"], "JWT chosen over sessions for stateless k8s scaling"
    );
    assert_eq!(row["line"], 1);
    assert!(
        row["file"].as_str().unwrap().ends_with("auth.py"),
        "expected auth.py file, got {}",
        row["file"]
    );
}
