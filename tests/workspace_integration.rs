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

#[test]
fn workspace_resolve_finds_external_in_sibling_repo() {
    // Issue #30 MVP — focus repo emits an `external:utils.run` sentinel
    // (a stand-in for an unresolved import). A sibling repo has a real
    // entity definition for `run` in utils.py. The resolve command
    // emits a 0.4 cross-repo resolution row.
    let tmp = TempDir::new().unwrap();
    let focus = tmp.path().join("app");
    let provider = tmp.path().join("shared");
    init_repo(&focus);
    init_repo(&provider);

    fs::create_dir_all(focus.join(".sigil")).unwrap();
    fs::write(
        focus.join(".sigil/entities.jsonl"),
        "{\"file\":\"<external>\",\"name\":\"external:utils.run\",\"kind\":\"external\",\"line_start\":0,\"line_end\":0,\"struct_hash\":\"0\"}\n",
    ).unwrap();

    fs::create_dir_all(provider.join(".sigil")).unwrap();
    fs::write(
        provider.join(".sigil/entities.jsonl"),
        concat!(
            "{\"file\":\"utils.py\",\"name\":\"run\",\"kind\":\"function\",\"line_start\":1,\"line_end\":2,\"struct_hash\":\"1\"}\n",
            "{\"file\":\"other.py\",\"name\":\"noise\",\"kind\":\"function\",\"line_start\":1,\"line_end\":2,\"struct_hash\":\"2\"}\n",
        ),
    ).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .arg("workspace")
        .arg("resolve")
        .arg("--root").arg(tmp.path())
        .arg("--focus").arg(&focus)
        .output()
        .expect("failed to run sigil");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let rows: Vec<serde_json::Value> = stdout.lines().filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("JSON"))
        .collect();
    assert_eq!(rows.len(), 1, "expected exactly 1 resolution; got {rows:?}");
    let r = &rows[0];
    assert_eq!(r["external_modpath"].as_str(), Some("utils.run"));
    assert_eq!(r["provider_repo"].as_str(), Some("shared"));
    assert_eq!(r["provider_file"].as_str(), Some("utils.py"));
    assert_eq!(r["provider_symbol"].as_str(), Some("run"));
    assert_eq!(r["confidence"].as_f64(), Some(0.4));
}
