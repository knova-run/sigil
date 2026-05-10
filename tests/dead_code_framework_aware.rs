//! Integration tests for framework-aware dead-code detection.
//!
//! Each test seeds a temp directory with:
//!   - source file(s) that match a framework pattern, AND/OR a symbol
//!     with a dynamic-name suffix
//!   - a minimal `.sigil/entities.jsonl` + `.sigil/refs.jsonl` that
//!     declares the symbols and (sometimes) one ref
//!
//! Then it invokes the `sigil dead-code` binary and asserts on the
//! JSONL output. Indexing is bypassed deliberately — the seed JSONL is
//! the canonical input for the dead-code analyzer.

use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

/// Write a single-entry entities.jsonl + refs.jsonl pair.
fn seed_sigil(root: &Path, entities: &[serde_json::Value], refs: &[serde_json::Value]) {
    let sigil_dir = root.join(".sigil");
    fs::create_dir_all(&sigil_dir).unwrap();
    let mut e_buf = String::new();
    for e in entities {
        e_buf.push_str(&serde_json::to_string(e).unwrap());
        e_buf.push('\n');
    }
    fs::write(sigil_dir.join("entities.jsonl"), e_buf).unwrap();
    let mut r_buf = String::new();
    for r in refs {
        r_buf.push_str(&serde_json::to_string(r).unwrap());
        r_buf.push('\n');
    }
    fs::write(sigil_dir.join("refs.jsonl"), r_buf).unwrap();
}

fn entity(file: &str, name: &str, kind: &str, visibility: Option<&str>) -> serde_json::Value {
    let mut v = serde_json::json!({
        "file": file,
        "name": name,
        "kind": kind,
        "line_start": 1,
        "line_end": 5,
        "struct_hash": "0000000000000000",
    });
    if let Some(vis) = visibility {
        v["visibility"] = serde_json::Value::String(vis.to_string());
    }
    v
}

fn run_dead_code(root: &Path, args: &[&str]) -> (String, String, bool) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sigil"));
    cmd.arg("dead-code").arg("--root").arg(root);
    for a in args {
        cmd.arg(a);
    }
    let out = cmd.output().expect("failed to run sigil dead-code");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.success(),
    )
}

fn parse(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

// ──────────────────────────────────────────────────────────────────────
// Flask routes — handler with no callers must be EXCLUDED.
// ──────────────────────────────────────────────────────────────────────
#[test]
fn flask_route_handler_is_not_flagged() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("api.py"),
        "from flask import Flask\napp = Flask(__name__)\n\n@app.route(\"/health\")\ndef health():\n    return \"ok\"\n",
    )
    .unwrap();
    seed_sigil(
        tmp.path(),
        &[entity("api.py", "health", "function", Some("public"))],
        &[],
    );
    let (stdout, stderr, ok) = run_dead_code(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let names: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get("name").and_then(|v| v.as_str()).map(String::from))
        .collect();
    assert!(
        !names.contains(&"health".to_string()),
        "flask route handler should not be flagged as dead, got rows: {rows:?}",
    );
}

// ──────────────────────────────────────────────────────────────────────
// FastAPI routes — same exclusion behaviour, different decorator shape.
// ──────────────────────────────────────────────────────────────────────
#[test]
fn fastapi_route_handler_is_not_flagged() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("users.py"),
        "from fastapi import APIRouter\nrouter = APIRouter()\n\n@router.get(\"/users\")\nasync def list_users():\n    return []\n",
    )
    .unwrap();
    seed_sigil(
        tmp.path(),
        &[entity("users.py", "list_users", "function", Some("public"))],
        &[],
    );
    let (stdout, stderr, ok) = run_dead_code(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let names: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get("name").and_then(|v| v.as_str()).map(String::from))
        .collect();
    assert!(
        !names.contains(&"list_users".to_string()),
        "fastapi route handler should not be flagged as dead, got rows: {rows:?}",
    );
}

// ──────────────────────────────────────────────────────────────────────
// Go chi router — exported handlers used via r.Get(...) must be excluded.
// ──────────────────────────────────────────────────────────────────────
#[test]
fn go_chi_route_handler_is_not_flagged() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("routes.go"),
        "package routes\n\nimport \"github.com/go-chi/chi/v5\"\n\nfunc Wire(r chi.Router) {\n    r.Get(\"/users\", ListUsers)\n}\n\nfunc ListUsers(w http.ResponseWriter, r *http.Request) {}\n",
    )
    .unwrap();
    seed_sigil(
        tmp.path(),
        &[
            entity("routes.go", "Wire", "function", Some("public")),
            entity("routes.go", "ListUsers", "function", Some("public")),
        ],
        &[],
    );
    let (stdout, stderr, ok) = run_dead_code(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    // Neither symbol should appear — the file is a framework entry point.
    let names: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get("name").and_then(|v| v.as_str()).map(String::from))
        .collect();
    assert!(
        names.is_empty(),
        "go chi handlers should not be flagged, got: {names:?}",
    );
    // The file itself also should not be flagged as a dead file.
    let files: Vec<String> = rows
        .iter()
        .filter(|r| r.get("kind").and_then(|v| v.as_str()) == Some("file"))
        .map(|r| r["file"].as_str().unwrap().to_string())
        .collect();
    assert!(
        !files.contains(&"routes.go".to_string()),
        "framework file should not be flagged as dead",
    );
}

// ──────────────────────────────────────────────────────────────────────
// Dynamic-name match — `*Handler` / `*Plugin` etc. get downgraded to
// low confidence (default-hidden); revealed by --include-low-confidence.
// ──────────────────────────────────────────────────────────────────────
#[test]
fn handler_suffix_export_is_downgraded_to_low_confidence() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("plugins.py"),
        "class AuthHandler:\n    def __init__(self):\n        pass\n",
    )
    .unwrap();
    seed_sigil(
        tmp.path(),
        &[entity("plugins.py", "AuthHandler", "class", Some("public"))],
        &[],
    );

    // Default run: AuthHandler is dynamic-name → 0.50 → hidden.
    let (stdout, stderr, ok) = run_dead_code(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let names: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get("name").and_then(|v| v.as_str()).map(String::from))
        .collect();
    assert!(
        !names.contains(&"AuthHandler".to_string()),
        "AuthHandler should be hidden at default confidence threshold, got: {rows:?}",
    );

    // With --include-low-confidence the candidate surfaces, tagged with
    // its dynamic-name suffix and confidence 0.50.
    let (stdout, stderr, ok) = run_dead_code(tmp.path(), &["--include-low-confidence"]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let row = rows
        .iter()
        .find(|r| r.get("name").and_then(|v| v.as_str()) == Some("AuthHandler"))
        .unwrap_or_else(|| panic!("expected AuthHandler with --include-low-confidence: {rows:?}"));
    assert_eq!(row["dynamic_name_match"], "Handler");
    assert!((row["confidence"].as_f64().unwrap() - 0.50).abs() < 1e-9);
}

// ──────────────────────────────────────────────────────────────────────
// Confidence-tier classification — an exported symbol with no callers
// (and no dynamic-name suffix) lands at 0.85.
// ──────────────────────────────────────────────────────────────────────
#[test]
fn exported_orphan_function_lands_at_0_85() {
    let tmp = TempDir::new().unwrap();
    // Plain non-framework file — also seed a referenced symbol so the
    // file itself isn't the top-tier 1.00 candidate.
    fs::write(
        tmp.path().join("lib.py"),
        "def used():\n    return 1\n\ndef orphan_export():\n    return 2\n",
    )
    .unwrap();
    seed_sigil(
        tmp.path(),
        &[
            entity("lib.py", "used", "function", Some("public")),
            entity("lib.py", "orphan_export", "function", Some("public")),
        ],
        &[serde_json::json!({
            "file": "other.py",
            "caller": "main",
            "name": "used",
            "kind": "call",
            "line": 1,
        })],
    );
    let (stdout, stderr, ok) = run_dead_code(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let row = rows
        .iter()
        .find(|r| r.get("name").and_then(|v| v.as_str()) == Some("orphan_export"))
        .unwrap_or_else(|| panic!("expected orphan_export row: {rows:?}"));
    assert!((row["confidence"].as_f64().unwrap() - 0.85).abs() < 1e-9);
    assert_eq!(row["entity_kind"], "function");
}

// ──────────────────────────────────────────────────────────────────────
// --safe-only filter — keeps ≥ 0.70 candidates and drops the rest.
// ──────────────────────────────────────────────────────────────────────
#[test]
fn safe_only_drops_below_threshold() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("svc.py"),
        "class AuthHandler: pass\ndef helper_fn(): pass\n",
    )
    .unwrap();
    seed_sigil(
        tmp.path(),
        &[
            entity("svc.py", "AuthHandler", "class", Some("public")),
            entity("svc.py", "helper_fn", "function", None),
        ],
        &[],
    );

    // With --safe-only + --include-low-confidence: even though we ask
    // for low-confidence inclusion, --safe-only takes precedence and
    // strips anything < 0.70.
    let (stdout, stderr, ok) =
        run_dead_code(tmp.path(), &["--include-low-confidence", "--safe-only"]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    for r in &rows {
        let c = r["confidence"].as_f64().unwrap();
        assert!(c >= 0.70, "--safe-only let through confidence {c}: {r:?}");
    }
    // The Handler-suffixed export must be gone.
    let names: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get("name").and_then(|v| v.as_str()).map(String::from))
        .collect();
    assert!(
        !names.contains(&"AuthHandler".to_string()),
        "--safe-only should drop AuthHandler (0.50): {rows:?}",
    );
}

// ──────────────────────────────────────────────────────────────────────
// JSON shape backward-compatibility — the new fields are all optional;
// when not populated, they must be absent from the JSON output.
// ──────────────────────────────────────────────────────────────────────
#[test]
fn optional_fields_omitted_when_none() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("plain.py"), "def lone(): pass\n").unwrap();
    seed_sigil(
        tmp.path(),
        &[entity("plain.py", "lone", "function", Some("public"))],
        &[],
    );
    let (stdout, stderr, ok) = run_dead_code(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let row = rows.iter().find(|r| r["kind"] == "symbol").unwrap();
    // Field present:
    assert!(row.get("confidence").is_some());
    assert!(row.get("file").is_some());
    // Optional fields not populated → must be absent in JSON.
    assert!(
        row.get("framework_excluded").is_none(),
        "framework_excluded should be omitted: {row:?}",
    );
    assert!(
        row.get("dynamic_name_match").is_none(),
        "dynamic_name_match should be omitted: {row:?}",
    );
}
