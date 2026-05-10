//! Integration tests for `sigil cochange --workspace <parent-dir>` —
//! cross-repo co-change mining.

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
    git(&["config", "user.email", "test@test.com"], path);
    git(&["config", "user.name", "Test"], path);
    git(&["config", "commit.gpgSign", "false"], path);
}

fn commit_file(repo: &std::path::Path, file: &str, contents: &str, msg: &str) {
    fs::write(repo.join(file), contents).unwrap();
    git(&["add", file], repo);
    git(&["commit", "-q", "-m", msg], repo);
}

#[test]
fn workspace_flag_runs_without_repos_and_emits_empty_output() {
    let tmp = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .arg("cochange")
        .arg("--workspace")
        .arg(tmp.path())
        .output()
        .expect("failed to run sigil");
    assert!(
        output.status.success(),
        "expected success on empty workspace, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    // No repos = no edges. Empty output (or empty array) both fine for the
    // tracer bullet — the contract is "exit 0, no rows".
    let non_empty: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert!(non_empty.is_empty(), "expected no rows, got: {stdout:?}");
}

#[test]
fn detects_cross_repo_pair_for_temporally_correlated_commits() {
    let tmp = TempDir::new().unwrap();
    let backend = tmp.path().join("backend");
    let frontend = tmp.path().join("frontend");
    init_repo(&backend);
    init_repo(&frontend);
    commit_file(&backend, "api.go", "package api", "wire up auth");
    commit_file(&frontend, "client.ts", "// auth client", "wire up auth");

    let output = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .arg("cochange")
        .arg("--workspace")
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
    assert!(
        !rows.is_empty(),
        "expected cross-repo edge for temporally-correlated commits, got: {stdout:?}"
    );
    let row = &rows[0];
    let s_repo = row["source_repo"].as_str().unwrap();
    let t_repo = row["target_repo"].as_str().unwrap();
    let pair = (s_repo, t_repo);
    assert!(
        pair == ("backend", "frontend") || pair == ("frontend", "backend"),
        "expected backend/frontend pair, got {pair:?}"
    );
    // Repowise's CrossRepoCoChange surfaces last_date as ISO yyyy-mm-dd.
    // We expose both forms (last_unix epoch + last_date ISO) so consumers
    // matching the repowise schema don't have to convert.
    let last_date = row["last_date"].as_str().expect("last_date should be present");
    assert!(
        last_date.len() == 10 && &last_date[4..5] == "-" && &last_date[7..8] == "-",
        "expected ISO yyyy-mm-dd last_date, got {last_date}"
    );
}
