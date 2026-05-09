//! Integration tests for `sigil workspace` — coordinator over multiple
//! git repos under a parent directory.

use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn git(args: &[&str], cwd: &std::path::Path) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git failed to run");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_repo(path: &std::path::Path) {
    fs::create_dir_all(path).unwrap();
    git(&["init", "-q"], path);
}

#[test]
fn workspace_scan_lists_child_git_repos() {
    let tmp = TempDir::new().unwrap();
    init_repo(&tmp.path().join("backend"));
    init_repo(&tmp.path().join("frontend"));
    // A non-git dir should not be listed
    fs::create_dir(tmp.path().join("notes")).unwrap();
    fs::write(tmp.path().join("notes/README.md"), "no").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .arg("workspace")
        .arg("scan")
        .arg("--root")
        .arg(tmp.path())
        .output()
        .expect("failed to run sigil");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let rows: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("each line should be JSON"))
        .collect();
    let names: Vec<&str> = rows.iter().map(|r| r["repo"].as_str().unwrap()).collect();
    assert!(names.contains(&"backend"), "expected backend in {names:?}");
    assert!(names.contains(&"frontend"), "expected frontend in {names:?}");
    assert!(
        !names.contains(&"notes"),
        "non-git dir should not be listed, got {names:?}"
    );
}
