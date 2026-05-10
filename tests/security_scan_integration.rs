//! Integration tests for `sigil security-scan` — regex-based security
//! signal extractor (eval, exec, hardcoded secrets, raw SQL concat,
//! TLS verify=False, weak hashes).

use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn run_scan(root: &std::path::Path) -> (String, String, bool) {
    let output = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .arg("security-scan")
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

fn parse(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn detects_eval_call_high_severity() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("danger.py"),
        "def runner(code):\n    return eval(code)\n",
    )
    .unwrap();
    let (stdout, stderr, ok) = run_scan(tmp.path());
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let row = rows
        .iter()
        .find(|r| r["kind"] == "eval_call")
        .unwrap_or_else(|| panic!("expected eval_call in {rows:?}"));
    assert_eq!(row["severity"], "high");
    assert_eq!(row["line"].as_u64().unwrap(), 2);
}

#[test]
fn detects_hardcoded_secret_pattern() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("config.py"),
        "api_key = \"sk-abc123\"\npassword = 'secret'\n",
    )
    .unwrap();
    let (stdout, stderr, ok) = run_scan(tmp.path());
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let kinds: Vec<&str> = rows.iter().map(|r| r["kind"].as_str().unwrap()).collect();
    assert!(kinds.contains(&"hardcoded_secret"), "expected hardcoded_secret in {kinds:?}");
    assert!(kinds.contains(&"hardcoded_password"), "expected hardcoded_password in {kinds:?}");
}

#[test]
fn ignores_clean_code_and_skipped_dirs() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("safe.py"), "def add(a, b): return a + b\n").unwrap();
    let buried = tmp.path().join("node_modules");
    fs::create_dir(&buried).unwrap();
    fs::write(buried.join("ignored.py"), "x = eval(stuff)\n").unwrap();
    let (stdout, stderr, ok) = run_scan(tmp.path());
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    assert!(rows.is_empty(), "expected zero findings, got {rows:?}");
}
