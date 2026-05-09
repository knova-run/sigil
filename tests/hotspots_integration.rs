//! Integration tests for `sigil hotspots` — git churn × line-count
//! hotspot ranking.

use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn git(args: &[&str], cwd: &std::path::Path) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git failed");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

fn init_repo(path: &std::path::Path) {
    fs::create_dir_all(path).unwrap();
    git(&["init", "-q"], path);
    git(&["config", "user.email", "test@test.com"], path);
    git(&["config", "user.name", "Test"], path);
    git(&["config", "commit.gpgSign", "false"], path);
}

fn commit(repo: &std::path::Path, file: &str, contents: &str, msg: &str) {
    fs::write(repo.join(file), contents).unwrap();
    git(&["add", file], repo);
    git(&["commit", "-q", "-m", msg], repo);
}

#[test]
fn hotspots_ranks_files_by_churn_times_size() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    // hot.py changes 3 times, cold.py changes once. hot.py is also longer.
    commit(tmp.path(), "hot.py", "x = 1\ny = 2\nz = 3\n", "init hot");
    commit(tmp.path(), "hot.py", "x = 1\ny = 2\nz = 3\nq = 4\n", "tweak hot");
    commit(tmp.path(), "hot.py", "x = 1\ny = 2\nz = 3\nq = 4\nr = 5\n", "tweak hot again");
    commit(tmp.path(), "cold.py", "a = 1\n", "init cold");

    let output = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .arg("hotspots")
        .arg("--root")
        .arg(tmp.path())
        .output()
        .expect("failed");
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let rows: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let hot = rows.iter().find(|r| r["file"].as_str().unwrap().ends_with("hot.py")).unwrap();
    let cold = rows.iter().find(|r| r["file"].as_str().unwrap().ends_with("cold.py")).unwrap();
    assert_eq!(hot["churn"].as_u64().unwrap(), 3);
    assert_eq!(cold["churn"].as_u64().unwrap(), 1);
    let hot_score = hot["hotspot_score"].as_f64().unwrap();
    let cold_score = cold["hotspot_score"].as_f64().unwrap();
    assert!(hot_score > cold_score, "expected hot > cold ({hot_score} vs {cold_score})");
}
