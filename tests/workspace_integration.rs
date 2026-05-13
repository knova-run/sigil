//! Integration tests for `sigil workspace` — coordinator over multiple
//! git repos under a parent directory.

use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn sigil(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args(args)
        .output()
        .expect("failed to run sigil")
}

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

// ---------------------------------------------------------------------------
// Phase 1 — explicit-membership commands: init / add / remove / enable /
// disable / list / index. See WORKSPACE_INDEXING_PLAN.md.
// ---------------------------------------------------------------------------

#[test]
fn workspace_init_creates_empty_members_json() {
    let tmp = TempDir::new().unwrap();
    let out = sigil(&[
        "workspace", "init",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let members_path = tmp.path().join(".sigil-workspace/members.json");
    assert!(members_path.exists(), "members.json should exist at {}", members_path.display());

    let text = fs::read_to_string(&members_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).expect("members.json should be valid JSON");
    assert_eq!(v["version"].as_i64(), Some(1));
    assert!(v["members"].is_array(), "members must be array, got {:?}", v["members"]);
    assert_eq!(v["members"].as_array().unwrap().len(), 0, "fresh init has no members");
}

#[test]
fn workspace_init_errors_when_already_initialized() {
    let tmp = TempDir::new().unwrap();
    let p = tmp.path().to_str().unwrap();

    let first = sigil(&["workspace", "init", p]);
    assert!(first.status.success(), "first init must succeed");

    let second = sigil(&["workspace", "init", p]);
    assert!(!second.status.success(), "re-init without --force must fail");
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("already initialized"),
        "stderr should explain why; got: {stderr}"
    );

    // --force re-runs cleanly without clobbering members.json
    let members_before = fs::read_to_string(tmp.path().join(".sigil-workspace/members.json")).unwrap();
    let forced = sigil(&["workspace", "init", p, "--force"]);
    assert!(
        forced.status.success(),
        "--force should succeed; stderr: {}",
        String::from_utf8_lossy(&forced.stderr)
    );
    let members_after = fs::read_to_string(tmp.path().join(".sigil-workspace/members.json")).unwrap();
    assert_eq!(members_before, members_after, "--force must not clobber members.json");
}

#[test]
fn workspace_add_creates_members_json_with_canonical_path() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let repo = tmp.path().join("repo-a");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&repo);

    // bootstrap workspace
    let init_out = sigil(&["workspace", "init", ws.to_str().unwrap()]);
    assert!(init_out.status.success(), "init: {}", String::from_utf8_lossy(&init_out.stderr));

    // add the repo by its absolute path
    let add_out = sigil(&[
        "workspace", "add",
        repo.to_str().unwrap(),
        "--root", ws.to_str().unwrap(),
    ]);
    assert!(
        add_out.status.success(),
        "add: stderr={}",
        String::from_utf8_lossy(&add_out.stderr)
    );

    // members.json now lists one entry whose path is canonical-absolute
    let text = fs::read_to_string(ws.join(".sigil-workspace/members.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    let members = v["members"].as_array().expect("array");
    assert_eq!(members.len(), 1, "expected one member; got {members:?}");
    let m = &members[0];
    assert_eq!(m["name"].as_str(), Some("repo-a"));

    // canonical path: absolute, no `.` / `..` segments
    let stored = m["path"].as_str().unwrap();
    assert!(std::path::Path::new(stored).is_absolute(), "path must be absolute: {stored}");
    assert!(!stored.contains("/./") && !stored.ends_with("/."), "path must be canonical");

    // added_at is set to a non-empty timestamp
    assert!(m["added_at"].as_str().map(|s| !s.is_empty()).unwrap_or(false),
        "added_at must be present: {m:?}");

    // disabled flag omitted when false (per plan: smaller diffs)
    assert!(m.get("disabled").is_none(), "disabled should be omitted when false; got {m:?}");
}

#[test]
fn workspace_add_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&repo);
    sigil(&["workspace", "init", ws.to_str().unwrap()]);

    let first = sigil(&[
        "workspace", "add", repo.to_str().unwrap(),
        "--root", ws.to_str().unwrap(),
        "--description", "first add",
    ]);
    assert!(first.status.success());

    // Second add with a *different* description must NOT overwrite the
    // existing entry (idempotent + non-destructive on canonical path).
    let second = sigil(&[
        "workspace", "add", repo.to_str().unwrap(),
        "--root", ws.to_str().unwrap(),
        "--description", "second add (should be ignored)",
    ]);
    assert!(second.status.success(), "re-add must succeed silently");

    let text = fs::read_to_string(ws.join(".sigil-workspace/members.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let members = v["members"].as_array().unwrap();
    assert_eq!(members.len(), 1, "re-add must not duplicate; got {members:?}");
    assert_eq!(
        members[0]["description"].as_str(),
        Some("first add"),
        "description must be preserved across re-add",
    );
}

#[test]
fn workspace_add_with_alias_overrides_name() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let repo = tmp.path().join("ugly-internal-name");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&repo);
    sigil(&["workspace", "init", ws.to_str().unwrap()]);

    let out = sigil(&[
        "workspace", "add", repo.to_str().unwrap(),
        "--root", ws.to_str().unwrap(),
        "--as", "pretty-name",
    ]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));

    let v: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(ws.join(".sigil-workspace/members.json")).unwrap()
    ).unwrap();
    assert_eq!(v["members"][0]["name"].as_str(), Some("pretty-name"));
}

#[test]
fn workspace_add_collision_appends_numeric_suffix() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let repo_x = tmp.path().join("x").join("frontend");
    let repo_y = tmp.path().join("y").join("frontend");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&repo_x);
    init_repo(&repo_y);
    sigil(&["workspace", "init", ws.to_str().unwrap()]);

    let a = sigil(&[
        "workspace", "add", repo_x.to_str().unwrap(),
        "--root", ws.to_str().unwrap(),
    ]);
    assert!(a.status.success());
    let b = sigil(&[
        "workspace", "add", repo_y.to_str().unwrap(),
        "--root", ws.to_str().unwrap(),
    ]);
    assert!(b.status.success());

    let v: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(ws.join(".sigil-workspace/members.json")).unwrap()
    ).unwrap();
    let names: Vec<&str> = v["members"]
        .as_array().unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"frontend"), "first add keeps base name; got {names:?}");
    assert!(names.contains(&"frontend-2"), "collision gets numeric suffix; got {names:?}");
}

#[test]
fn workspace_remove_drops_member_by_name_or_path() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let repo_a = tmp.path().join("repo-a");
    let repo_b = tmp.path().join("repo-b");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&repo_a);
    init_repo(&repo_b);
    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", repo_a.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", repo_b.to_str().unwrap(), "--root", ws.to_str().unwrap()]);

    // Remove by name
    let r1 = sigil(&["workspace", "remove", "repo-a", "--root", ws.to_str().unwrap()]);
    assert!(r1.status.success(), "remove by name: {}", String::from_utf8_lossy(&r1.stderr));

    // Remove by path
    let r2 = sigil(&["workspace", "remove", repo_b.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    assert!(r2.status.success(), "remove by path: {}", String::from_utf8_lossy(&r2.stderr));

    let v: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(ws.join(".sigil-workspace/members.json")).unwrap()
    ).unwrap();
    assert_eq!(v["members"].as_array().unwrap().len(), 0, "both members should be gone");
}

#[test]
fn workspace_remove_warns_when_member_absent() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    sigil(&["workspace", "init", ws.to_str().unwrap()]);

    let out = sigil(&["workspace", "remove", "ghost", "--root", ws.to_str().unwrap()]);
    assert!(out.status.success(), "removing an absent member must NOT error");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.to_lowercase().contains("not"), "should warn the member is absent; got: {stderr}");
}

#[test]
fn workspace_enable_disable_round_trip() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&repo);
    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", repo.to_str().unwrap(), "--root", ws.to_str().unwrap()]);

    let read = || {
        let s = fs::read_to_string(ws.join(".sigil-workspace/members.json")).unwrap();
        serde_json::from_str::<serde_json::Value>(&s).unwrap()
    };

    // Fresh: disabled flag is OMITTED (false-default)
    assert!(read()["members"][0].get("disabled").is_none(),
        "fresh member should not serialize disabled=false");

    // Disable: now disabled=true is present
    let d = sigil(&["workspace", "disable", "repo", "--root", ws.to_str().unwrap()]);
    assert!(d.status.success(), "disable: {}", String::from_utf8_lossy(&d.stderr));
    assert_eq!(read()["members"][0]["disabled"].as_bool(), Some(true));

    // Re-enable: omitted again
    let e = sigil(&["workspace", "enable", "repo", "--root", ws.to_str().unwrap()]);
    assert!(e.status.success(), "enable: {}", String::from_utf8_lossy(&e.stderr));
    assert!(read()["members"][0].get("disabled").is_none(),
        "enabled member should serialize without disabled field");
}

#[test]
fn workspace_list_json_includes_disabled_flag() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let a = tmp.path().join("repo-a");
    let b = tmp.path().join("repo-b");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&a);
    init_repo(&b);
    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", a.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", b.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "disable", "repo-b", "--root", ws.to_str().unwrap()]);

    let out = sigil(&["workspace", "list", "--root", ws.to_str().unwrap(), "--json"]);
    assert!(out.status.success(), "list --json: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();

    let rows: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("each line should be JSON"))
        .collect();
    assert_eq!(rows.len(), 2, "expected 2 rows; got {rows:?}");

    let by_name: std::collections::HashMap<&str, &serde_json::Value> = rows
        .iter()
        .map(|r| (r["name"].as_str().unwrap(), r))
        .collect();
    assert!(by_name["repo-a"].get("disabled").is_none(),
        "enabled member must not serialize disabled field; got {:?}", by_name["repo-a"]);
    assert_eq!(by_name["repo-b"]["disabled"].as_bool(), Some(true),
        "disabled member must show disabled=true; got {:?}", by_name["repo-b"]);
}

/// Helper: write fake `.sigil/` data into a repo so `workspace index`
/// can stamp it without paying for a real parse.
fn write_fake_sigil(repo: &std::path::Path, entities: &str, refs: &str) {
    let sigil_dir = repo.join(".sigil");
    fs::create_dir_all(&sigil_dir).unwrap();
    fs::write(sigil_dir.join("entities.jsonl"), entities).unwrap();
    fs::write(sigil_dir.join("refs.jsonl"), refs).unwrap();
}

#[test]
fn workspace_index_stamps_each_member_jsonl() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let a = tmp.path().join("repo-a");
    let b = tmp.path().join("repo-b");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&a);
    init_repo(&b);
    write_fake_sigil(&a, "{\"name\":\"x\"}\n", "{\"caller\":\"x\"}\n");
    write_fake_sigil(&b, "{\"name\":\"y\"}\n{\"name\":\"z\"}\n", "");

    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", a.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", b.to_str().unwrap(), "--root", ws.to_str().unwrap()]);

    let out = sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);
    assert!(out.status.success(), "index: {}", String::from_utf8_lossy(&out.stderr));

    let manifest_path = ws.join(".sigil-workspace/manifest.json");
    assert!(manifest_path.exists(), "manifest.json must be created");
    let v: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&manifest_path).unwrap()
    ).expect("manifest.json must be valid JSON");

    let members = v["members"].as_object().expect("members object");
    assert_eq!(members.len(), 2, "expected 2 stamped members; got {members:?}");

    // Each stamp records entities_len + refs_len matching the on-disk size
    let stamp_a = &members["repo-a"];
    assert_eq!(stamp_a["entities_len"].as_u64(), Some(13)); // `{"name":"x"}\n` = 13 bytes
    assert_eq!(stamp_a["refs_len"].as_u64(), Some(15));     // `{"caller":"x"}\n` = 15 bytes
    assert!(stamp_a["entities_mtime_ms"].as_i64().is_some());

    let stamp_b = &members["repo-b"];
    assert_eq!(stamp_b["entities_len"].as_u64(), Some(26)); // 13 + 13 bytes
    assert_eq!(stamp_b["refs_len"].as_u64(), Some(0));

    // Phase 1: cross_repo_refs.jsonl should exist and be empty (Phase 3 fills it)
    let cross = ws.join(".sigil-workspace/cross_repo_refs.jsonl");
    assert!(cross.exists(), "cross_repo_refs.jsonl placeholder must exist");
    assert_eq!(fs::read_to_string(&cross).unwrap(), "", "Phase 1 leaves cross-repo refs empty");
}

#[test]
fn workspace_index_skips_disabled_member() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let a = tmp.path().join("repo-a");
    let b = tmp.path().join("repo-b");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&a);
    init_repo(&b);
    write_fake_sigil(&a, "{}\n", "");
    write_fake_sigil(&b, "{}\n", "");

    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", a.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", b.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "disable", "repo-b", "--root", ws.to_str().unwrap()]);

    let out = sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);
    assert!(out.status.success(), "index: {}", String::from_utf8_lossy(&out.stderr));

    let v: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(ws.join(".sigil-workspace/manifest.json")).unwrap()
    ).unwrap();
    let members = v["members"].as_object().unwrap();
    assert_eq!(members.len(), 1, "only enabled member should be stamped; got {members:?}");
    assert!(members.contains_key("repo-a"));
    assert!(!members.contains_key("repo-b"), "disabled member must be skipped");
}

#[test]
fn workspace_index_warns_and_skips_missing_path_without_mutating_members_json() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let alive = tmp.path().join("alive");
    let doomed = tmp.path().join("doomed");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&alive);
    init_repo(&doomed);
    write_fake_sigil(&alive, "{}\n", "");
    write_fake_sigil(&doomed, "{}\n", "");

    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", alive.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", doomed.to_str().unwrap(), "--root", ws.to_str().unwrap()]);

    let members_before = fs::read_to_string(ws.join(".sigil-workspace/members.json")).unwrap();

    // Yank `doomed` from disk
    fs::remove_dir_all(&doomed).unwrap();

    let out = sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);
    assert!(out.status.success(), "index must finish even if a member vanished; stderr: {}",
        String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no longer exists") || stderr.to_lowercase().contains("skipping"),
        "stderr must warn about the missing path; got: {stderr}");

    // members.json must be UNCHANGED — index is not allowed to mutate it
    let members_after = fs::read_to_string(ws.join(".sigil-workspace/members.json")).unwrap();
    assert_eq!(members_before, members_after,
        "workspace index must never silently mutate members.json");

    // Only the alive member is in the stamp manifest
    let v: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(ws.join(".sigil-workspace/manifest.json")).unwrap()
    ).unwrap();
    let stamped = v["members"].as_object().unwrap();
    assert_eq!(stamped.len(), 1);
    assert!(stamped.contains_key("alive"));
}

#[test]
fn workspace_index_errors_when_uninitialized() {
    let tmp = TempDir::new().unwrap();
    let out = sigil(&["workspace", "index", "--root", tmp.path().to_str().unwrap()]);
    assert!(!out.status.success(), "index without init must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not initialized") || stderr.contains("init"),
        "should hint at running `workspace init`; got: {stderr}");
}

#[test]
fn workspace_index_errors_when_no_enabled_members() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&repo);
    write_fake_sigil(&repo, "{}\n", "");

    sigil(&["workspace", "init", ws.to_str().unwrap()]);

    // No members at all
    let empty = sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);
    assert!(!empty.status.success(), "index with empty members.json must fail");

    // Add then disable; result: zero ENABLED members
    sigil(&["workspace", "add", repo.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "disable", "repo", "--root", ws.to_str().unwrap()]);
    let all_disabled = sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);
    assert!(!all_disabled.status.success(), "index with all-disabled must fail");
    let stderr = String::from_utf8_lossy(&all_disabled.stderr);
    assert!(stderr.to_lowercase().contains("disabled") || stderr.to_lowercase().contains("no enabled"),
        "stderr should explain why; got: {stderr}");
}

#[test]
fn workspace_index_drops_stamp_for_removed_member() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&a);
    init_repo(&b);
    write_fake_sigil(&a, "{}\n", "");
    write_fake_sigil(&b, "{}\n", "");

    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", a.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", b.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);

    let before: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(ws.join(".sigil-workspace/manifest.json")).unwrap()
    ).unwrap();
    assert_eq!(before["members"].as_object().unwrap().len(), 2);

    sigil(&["workspace", "remove", "b", "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);

    let after: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(ws.join(".sigil-workspace/manifest.json")).unwrap()
    ).unwrap();
    let stamped = after["members"].as_object().unwrap();
    assert_eq!(stamped.len(), 1, "removed member's stamp should be dropped; got {stamped:?}");
    assert!(stamped.contains_key("a"));
    assert!(!stamped.contains_key("b"));
}
