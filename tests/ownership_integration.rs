//! Integration tests for `sigil ownership` — git-blame-derived ownership
//! percentages per file.

use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn git_with_email(args: &[&str], cwd: &std::path::Path, email: &str) {
    let output = Command::new("git")
        .args(args)
        .env("GIT_AUTHOR_EMAIL", email)
        .env("GIT_COMMITTER_EMAIL", email)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_COMMITTER_NAME", "Test")
        .current_dir(cwd)
        .output()
        .expect("git failed");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

fn init_repo(path: &std::path::Path) {
    fs::create_dir_all(path).unwrap();
    git_with_email(&["init", "-q"], path, "init@x.com");
    git_with_email(&["config", "commit.gpgSign", "false"], path, "init@x.com");
}

fn commit(repo: &std::path::Path, file: &str, contents: &str, msg: &str, email: &str) {
    fs::write(repo.join(file), contents).unwrap();
    git_with_email(&["add", file], repo, email);
    git_with_email(&["commit", "-q", "-m", msg], repo, email);
}

#[test]
fn ownership_aggregates_authors_per_file() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    // alice writes 3 commits to lib.py, bob writes 1.
    commit(tmp.path(), "lib.py", "x = 1\n", "init", "alice@x.com");
    commit(tmp.path(), "lib.py", "x = 1\ny = 2\n", "tweak", "alice@x.com");
    commit(tmp.path(), "lib.py", "x = 1\ny = 2\nz = 3\n", "tweak", "alice@x.com");
    commit(tmp.path(), "lib.py", "x = 1\ny = 2\nz = 3\nq = 4\n", "tweak", "bob@x.com");

    let output = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .arg("ownership")
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
    let row = rows
        .iter()
        .find(|r| r["file"].as_str().unwrap().ends_with("lib.py"))
        .unwrap_or_else(|| panic!("expected lib.py in {rows:?}"));
    assert_eq!(row["primary_owner"], "alice@x.com");
    assert_eq!(row["author_count"].as_u64().unwrap(), 2);
    let pct = row["ownership_pct"].as_f64().unwrap();
    assert!(
        (pct - 75.0).abs() < 0.01,
        "expected alice's 3/4 = 75% ownership, got {pct}"
    );
}
