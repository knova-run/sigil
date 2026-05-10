//! Integration tests for `sigil log --significant <file>`.

use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn git(args: &[&str], cwd: &std::path::Path) {
    let output = Command::new("git")
        .args(args)
        .env("GIT_AUTHOR_EMAIL", "test@x.com")
        .env("GIT_COMMITTER_EMAIL", "test@x.com")
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
    git(&["init", "-q"], path);
    git(&["config", "commit.gpgSign", "false"], path);
}

fn commit(repo: &std::path::Path, file: &str, contents: &str, msg: &str) {
    fs::write(repo.join(file), contents).unwrap();
    git(&["add", file], repo);
    git(&["commit", "-q", "-m", msg], repo);
}

#[test]
fn drops_short_subjects_and_noise_prefixes_keeps_intentful_ones() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    // 1. A noise-prefix commit (chore) — must be filtered.
    commit(
        tmp.path(),
        "auth.py",
        "x = 1\n",
        "chore: bump version to 0.5.0 in Cargo.toml",
    );
    // 2. A too-short subject (under 30 chars) — must be filtered.
    commit(tmp.path(), "auth.py", "x = 2\n", "fix bug");
    // 3. An intent-bearing subject — must survive.
    commit(
        tmp.path(),
        "auth.py",
        "x = 3\n",
        "feat: extract decision markers from commit bodies",
    );
    // 4. A dependabot noise commit — must be filtered.
    commit(
        tmp.path(),
        "auth.py",
        "x = 4\n",
        "dependabot[bot] bump async-trait from 0.1 to 0.2",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .arg("log")
        .arg("--significant")
        .arg("auth.py")
        .arg("--root")
        .arg(tmp.path())
        .output()
        .expect("failed");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows: Vec<serde_json::Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(rows.len(), 1, "expected one survivor, got {rows:?}");
    assert_eq!(
        rows[0]["subject"], "feat: extract decision markers from commit bodies"
    );
    // sha is non-empty + an iso-ish date is present
    assert!(!rows[0]["sha"].as_str().unwrap().is_empty());
    assert!(!rows[0]["date"].as_str().unwrap().is_empty());
    assert_eq!(rows[0]["author"], "test@x.com");
    // paths include auth.py
    let paths: Vec<&str> = rows[0]["paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        paths.iter().any(|p| p.ends_with("auth.py")),
        "expected auth.py in paths, got {paths:?}"
    );
}

#[test]
fn limit_caps_returned_rows() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    for i in 0..5 {
        commit(
            tmp.path(),
            "lib.py",
            &format!("x = {i}\n"),
            &format!("feat: meaningful commit number {i} that's long enough"),
        );
    }
    let output = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .arg("log")
        .arg("--significant")
        .arg("lib.py")
        .arg("--limit")
        .arg("2")
        .arg("--root")
        .arg(tmp.path())
        .output()
        .expect("failed");
    assert!(output.status.success());
    let rows: Vec<serde_json::Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(rows.len(), 2, "expected --limit 2 to cap output");
}
