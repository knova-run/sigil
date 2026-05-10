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

#[test]
fn include_git_history_lifts_commit_message_markers() {
    // Real-world archaeology: developers write `Why:` / `Decision:` in
    // commit bodies more often than in code comments. The opt-in flag
    // should surface those rows with `source: "commit_message"` while
    // leaving the inline-source rows byte-stable (no `source` key on the
    // wire — `skip_serializing_if = None`).
    let tmp = TempDir::new().unwrap();
    fs::create_dir_all(tmp.path()).unwrap();
    // bootstrap git
    let init = Command::new("git")
        .args(["init", "-q"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(init.status.success());
    Command::new("git")
        .args(["config", "commit.gpgSign", "false"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    // An inline-source decision marker
    fs::write(tmp.path().join("auth.py"), "# DECISION: keep JWT auth\n").unwrap();

    // Commit with a body that contains a `Why:` and a `Decision:` line
    Command::new("git")
        .args(["add", "."])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let commit = Command::new("git")
        .args([
            "commit",
            "-q",
            "-m",
            "feat: add auth module",
            "-m",
            "Why: legacy /v1 callers rely on the JWT shape\n\nDecision: keep the bearer-token path stable through v2",
        ])
        .env("GIT_AUTHOR_EMAIL", "test@x.com")
        .env("GIT_COMMITTER_EMAIL", "test@x.com")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_COMMITTER_NAME", "Test")
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(commit.status.success(), "commit failed: {}", String::from_utf8_lossy(&commit.stderr));

    // 1. Without --include-git-history: only the inline row, no `source` key
    let plain = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args(["decisions", "--root"])
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(plain.status.success());
    let plain_rows: Vec<serde_json::Value> = String::from_utf8_lossy(&plain.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(plain_rows.len(), 1);
    assert!(
        plain_rows[0].get("source").is_none(),
        "inline rows must not carry a `source` key (byte-stable old output): {plain_rows:?}"
    );
    assert_eq!(plain_rows[0]["marker"], "DECISION");

    // 2. With --include-git-history: inline + commit-message rows
    let extended = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args(["decisions", "--include-git-history", "--root"])
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(
        extended.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&extended.stderr)
    );
    let rows: Vec<serde_json::Value> = String::from_utf8_lossy(&extended.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    // We expect at least: 1 inline DECISION + 1 commit-message Why + 1 commit-message Decision
    let commit_rows: Vec<&serde_json::Value> = rows
        .iter()
        .filter(|r| r.get("source").and_then(|v| v.as_str()) == Some("commit_message"))
        .collect();
    let why_count = commit_rows.iter().filter(|r| r["marker"] == "Why").count();
    let decision_count = commit_rows
        .iter()
        .filter(|r| r["marker"] == "Decision")
        .count();
    assert!(why_count >= 1, "expected a Why: row from commit body, got {commit_rows:?}");
    assert!(
        decision_count >= 1,
        "expected a Decision: row from commit body, got {commit_rows:?}"
    );
    // And the inline row still appears, still without a `source` key
    let inline_rows: Vec<&serde_json::Value> = rows
        .iter()
        .filter(|r| r.get("source").is_none())
        .collect();
    assert!(
        inline_rows.iter().any(|r| r["marker"] == "DECISION"),
        "inline DECISION row should still be present: {inline_rows:?}"
    );
}
