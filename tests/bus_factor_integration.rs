//! Integration tests for `sigil bus-factor` — per-file
//! knowledge-concentration risk derived from git log.

use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn git(args: &[&str], cwd: &std::path::Path, email: &str) {
    let output = Command::new("git")
        .args(args)
        .env("GIT_AUTHOR_EMAIL", email)
        .env("GIT_COMMITTER_EMAIL", email)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_COMMITTER_NAME", "Test")
        .current_dir(cwd)
        .output()
        .expect("git failed");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_repo(path: &std::path::Path) {
    fs::create_dir_all(path).unwrap();
    git(&["init", "-q"], path, "init@x.com");
    git(&["config", "commit.gpgSign", "false"], path, "init@x.com");
}

fn commit(repo: &std::path::Path, file: &str, contents: &str, msg: &str, email: &str) {
    fs::write(repo.join(file), contents).unwrap();
    git(&["add", file], repo, email);
    git(&["commit", "-q", "-m", msg], repo, email);
}

fn run_bus_factor(root: &std::path::Path, extra: &[&str]) -> Vec<serde_json::Value> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sigil"));
    cmd.arg("bus-factor").arg("--root").arg(root);
    cmd.args(extra);
    let output = cmd.output().expect("failed to run sigil");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn high_risk_when_single_author_dominates() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    // alice writes 4 of 4 commits to solo.py — primary_share = 1.0 ⇒ high
    for i in 0..4 {
        commit(
            tmp.path(),
            "solo.py",
            &format!("x = {i}\n"),
            &format!("c{i}"),
            "alice@x.com",
        );
    }
    let rows = run_bus_factor(tmp.path(), &[]);
    let row = rows
        .iter()
        .find(|r| r["path"].as_str().unwrap().ends_with("solo.py"))
        .expect("solo.py row");
    assert_eq!(row["primary_owner"], "alice@x.com");
    assert!((row["primary_share"].as_f64().unwrap() - 1.0).abs() < 0.001);
    assert_eq!(row["risk"], "high");
    // second_share is 0.0 with a single author
    assert!((row["second_share"].as_f64().unwrap() - 0.0).abs() < 0.001);
    // second_owner is elided (skip_serializing_if = None)
    assert!(row.get("second_owner").is_none() || row["second_owner"].is_null());
}

#[test]
fn medium_and_low_bands_emit_second_owner() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    // alice 3, bob 2 ⇒ primary_share = 0.6 ⇒ medium, with bob as second
    for i in 0..3 {
        commit(
            tmp.path(),
            "lib.py",
            &format!("x = {i}\n"),
            &format!("a{i}"),
            "alice@x.com",
        );
    }
    for i in 0..2 {
        commit(
            tmp.path(),
            "lib.py",
            &format!("x = {}\n", i + 100),
            &format!("b{i}"),
            "bob@x.com",
        );
    }
    let rows = run_bus_factor(tmp.path(), &[]);
    let row = rows
        .iter()
        .find(|r| r["path"].as_str().unwrap().ends_with("lib.py"))
        .expect("lib.py row");
    assert_eq!(row["primary_owner"], "alice@x.com");
    assert!((row["primary_share"].as_f64().unwrap() - 0.6).abs() < 0.001);
    assert_eq!(row["risk"], "medium");
    assert_eq!(row["second_owner"], "bob@x.com");
    assert!((row["second_share"].as_f64().unwrap() - 0.4).abs() < 0.001);
}

#[test]
fn custom_threshold_demotes_high_to_medium() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    // Make primary_share = 0.8 exactly. Default threshold 0.8 ⇒ high.
    // With --threshold 0.9, the same row should be medium.
    for i in 0..4 {
        commit(
            tmp.path(),
            "x.py",
            &format!("x = {i}\n"),
            &format!("a{i}"),
            "alice@x.com",
        );
    }
    commit(tmp.path(), "x.py", "x = 999\n", "b0", "bob@x.com");

    let rows = run_bus_factor(tmp.path(), &[]);
    let row = rows
        .iter()
        .find(|r| r["path"].as_str().unwrap().ends_with("x.py"))
        .expect("x.py row at default threshold");
    assert_eq!(row["risk"], "high", "0.8 share at default threshold 0.8 = high");

    let rows = run_bus_factor(tmp.path(), &["--threshold", "0.9"]);
    let row = rows
        .iter()
        .find(|r| r["path"].as_str().unwrap().ends_with("x.py"))
        .expect("x.py row at threshold 0.9");
    assert_eq!(row["risk"], "medium", "0.8 share at threshold 0.9 = medium");
}
