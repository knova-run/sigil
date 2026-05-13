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

// ---------------------------------------------------------------------------
// Phase 2 — union-load: Backend::load reads .sigil-workspace/ transparently
// and returns one Index covering every member. File paths get prefixed with
// `<member-name>/` so cross-repo same-name files don't collide.
// ---------------------------------------------------------------------------

#[test]
fn workspace_callers_returns_refs_from_multiple_repos() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let provider = tmp.path().join("provider");
    let consumer = tmp.path().join("consumer");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&provider);
    init_repo(&consumer);

    // provider/.sigil/entities.jsonl defines the function "Greet"
    let prov_ent = "{\"file\":\"src/lib.rs\",\"name\":\"Greet\",\"kind\":\"function\",\
        \"line_start\":1,\"line_end\":3,\"struct_hash\":\"a\"}\n";
    write_fake_sigil(&provider, prov_ent, "");

    // consumer/.sigil/refs.jsonl has a call to "Greet"
    let cons_ent = "{\"file\":\"src/main.rs\",\"name\":\"main\",\"kind\":\"function\",\
        \"line_start\":1,\"line_end\":5,\"struct_hash\":\"b\"}\n";
    let cons_ref = "{\"file\":\"src/main.rs\",\"caller\":\"main\",\"name\":\"Greet\",\
        \"kind\":\"call\",\"line\":2}\n";
    write_fake_sigil(&consumer, cons_ent, cons_ref);

    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", provider.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", consumer.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);

    // `sigil --root <ws> callers Greet` must surface the cross-repo caller
    let out = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args(["callers", "Greet", "--root", ws.to_str().unwrap(), "--json"])
        .env("SIGIL_NO_AUTO_INDEX", "1") // workspaces must not auto-index
        .output()
        .expect("sigil callers failed to run");
    assert!(
        out.status.success(),
        "callers --root <ws>: status={} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("callers stdout must be JSON; got: {stdout:?}\nerr: {e}"));
    let refs = parsed.as_array().expect("callers JSON is an array");
    assert_eq!(refs.len(), 1, "expected one cross-repo caller; got {refs:?}");
    let r = &refs[0];
    assert_eq!(r["caller"].as_str(), Some("main"));
    assert_eq!(r["name"].as_str(), Some("Greet"));

    // File path is prefixed with the member name so cross-repo same-named
    // files don't collide
    assert_eq!(
        r["file"].as_str(),
        Some("consumer/src/main.rs"),
        "file should be prefixed with member name; got {r:?}"
    );
}

#[test]
fn workspace_load_skips_disabled_member() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&a);
    init_repo(&b);

    let ent = |file: &str, name: &str| format!(
        "{{\"file\":\"{file}\",\"name\":\"{name}\",\"kind\":\"function\",\
        \"line_start\":1,\"line_end\":2,\"struct_hash\":\"x\"}}\n"
    );
    write_fake_sigil(&a, &ent("src/lib.rs", "alpha"), "");
    write_fake_sigil(&b, &ent("src/lib.rs", "beta"), "");

    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", a.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", b.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "disable", "b", "--root", ws.to_str().unwrap()]);

    let out = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args(["symbols", "src/lib.rs", "--root", ws.to_str().unwrap(), "--json"])
        .env("SIGIL_NO_AUTO_INDEX", "1")
        .output()
        .unwrap();
    // The query API uses prefixed paths in workspace mode; `symbols
    // src/lib.rs` against a workspace finds nothing (no member is at that
    // bare relative path). Use the prefixed form instead.
    let _ = out;

    let alpha = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args(["symbols", "a/src/lib.rs", "--root", ws.to_str().unwrap(), "--json"])
        .env("SIGIL_NO_AUTO_INDEX", "1")
        .output()
        .unwrap();
    assert!(alpha.status.success(), "alpha lookup: {}", String::from_utf8_lossy(&alpha.stderr));
    let alpha_json: serde_json::Value = serde_json::from_str(
        &String::from_utf8_lossy(&alpha.stdout)
    ).unwrap();
    assert!(alpha_json.as_array().unwrap().iter().any(|e| e["name"] == "alpha"),
        "enabled member 'a' should be in the workspace index");

    let beta = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args(["symbols", "b/src/lib.rs", "--root", ws.to_str().unwrap(), "--json"])
        .env("SIGIL_NO_AUTO_INDEX", "1")
        .output()
        .unwrap();
    // disabled members are absent — sigil symbols returns either empty or
    // an error (no entities at that file path)
    let beta_stdout = String::from_utf8_lossy(&beta.stdout).to_string();
    if let Ok(beta_json) = serde_json::from_str::<serde_json::Value>(&beta_stdout) {
        assert!(beta_json.as_array().map(|a| a.is_empty()).unwrap_or(true),
            "disabled member must be absent; got {beta_json:?}");
    }
}

#[test]
fn workspace_external_sentinels_keep_synthetic_file_marker() {
    // `external:<modpath>` entities have `file = "<external>"`. The
    // union-load must NOT prefix them with the member name — they're
    // synthetic, not real source files.
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&repo);

    let ents = concat!(
        "{\"file\":\"<external>\",\"name\":\"external:requests.get\",\"kind\":\"external\",",
            "\"line_start\":0,\"line_end\":0,\"struct_hash\":\"0\"}\n",
        "{\"file\":\"src/app.py\",\"name\":\"handler\",\"kind\":\"function\",",
            "\"line_start\":1,\"line_end\":3,\"struct_hash\":\"a\"}\n",
    );
    write_fake_sigil(&repo, ents, "");

    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", repo.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);

    // Real file gets prefixed; external sentinel does not
    let real = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args(["symbols", "repo/src/app.py", "--root", ws.to_str().unwrap(), "--json"])
        .env("SIGIL_NO_AUTO_INDEX", "1")
        .output()
        .unwrap();
    let real_json: serde_json::Value = serde_json::from_str(
        &String::from_utf8_lossy(&real.stdout)
    ).unwrap();
    assert!(real_json.as_array().unwrap().iter().any(|e| e["name"] == "handler"),
        "real file should be prefixed-locatable; got {real_json:?}");

    // External sentinel survives at `<external>` (NOT `repo/<external>`)
    let ext = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args(["symbols", "<external>", "--root", ws.to_str().unwrap(), "--json"])
        .env("SIGIL_NO_AUTO_INDEX", "1")
        .output()
        .unwrap();
    let ext_json: serde_json::Value = serde_json::from_str(
        &String::from_utf8_lossy(&ext.stdout)
    ).unwrap();
    assert!(
        ext_json.as_array().unwrap().iter().any(|e| e["name"] == "external:requests.get"),
        "external sentinel must keep its <external> file marker; got {ext_json:?}"
    );
}

#[test]
fn workspace_load_includes_cross_repo_refs() {
    // Phase 3 will fill cross_repo_refs.jsonl automatically. For Phase 2,
    // just verify that hand-written rows are loaded and surfaced via the
    // standard query API.
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let provider = tmp.path().join("provider");
    let consumer = tmp.path().join("consumer");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&provider);
    init_repo(&consumer);

    let prov_ent = "{\"file\":\"lib.py\",\"name\":\"run\",\"kind\":\"function\",\
        \"line_start\":1,\"line_end\":2,\"struct_hash\":\"a\"}\n";
    let cons_ent = "{\"file\":\"main.py\",\"name\":\"main\",\"kind\":\"function\",\
        \"line_start\":1,\"line_end\":3,\"struct_hash\":\"b\"}\n";
    write_fake_sigil(&provider, prov_ent, "");
    write_fake_sigil(&consumer, cons_ent, "");

    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", provider.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", consumer.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);

    // Hand-write a cross-repo ref (already prefixed with `consumer/`)
    let cross = ws.join(".sigil-workspace/cross_repo_refs.jsonl");
    fs::write(&cross,
        "{\"file\":\"consumer/main.py\",\"caller\":\"main\",\"name\":\"run\",\
        \"kind\":\"call\",\"line\":2,\"confidence\":0.4}\n"
    ).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args(["callers", "run", "--root", ws.to_str().unwrap(), "--json"])
        .env("SIGIL_NO_AUTO_INDEX", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "callers: {}", String::from_utf8_lossy(&out.stderr));
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let refs = parsed.as_array().unwrap();
    assert_eq!(refs.len(), 1, "expected the hand-written cross-repo ref; got {refs:?}");
    assert_eq!(refs[0]["confidence"].as_f64(), Some(0.4));
}

// ---------------------------------------------------------------------------
// Phase 3 — cross-repo resolution at index time. `workspace index` walks
// each member's external sentinels and writes resolved bindings into
// `.sigil-workspace/cross_repo_refs.jsonl`. Confidence policy:
//   * Single match, direct package-deps edge: 0.6
//   * Single match, no dep evidence:           0.4
//   * Multiple matches (one provider or many): 0.3 each
//   * Cap: 10 emissions per sentinel
// ---------------------------------------------------------------------------

#[test]
fn workspace_index_writes_cross_repo_refs_at_0_4_without_deps() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let consumer = tmp.path().join("consumer");
    let provider = tmp.path().join("provider");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&consumer);
    init_repo(&provider);

    // consumer has an unresolved import sentinel for `external:Greet`
    let consumer_ents = "{\"file\":\"<external>\",\"name\":\"external:Greet\",\
        \"kind\":\"external\",\"line_start\":0,\"line_end\":0,\"struct_hash\":\"e\"}\n";
    write_fake_sigil(&consumer, consumer_ents, "");

    // provider defines `Greet` exactly once
    let provider_ents = "{\"file\":\"src/lib.rs\",\"name\":\"Greet\",\
        \"kind\":\"function\",\"line_start\":1,\"line_end\":3,\"struct_hash\":\"g\"}\n";
    write_fake_sigil(&provider, provider_ents, "");

    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", consumer.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", provider.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    let out = sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);
    assert!(out.status.success(), "index: {}", String::from_utf8_lossy(&out.stderr));

    let cross_path = ws.join(".sigil-workspace/cross_repo_refs.jsonl");
    let text = fs::read_to_string(&cross_path).unwrap();
    let rows: Vec<serde_json::Value> = text
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("each line must be JSON"))
        .collect();
    assert_eq!(rows.len(), 1, "expected one cross-repo ref; got {rows:?}");
    let r = &rows[0];
    assert_eq!(r["name"].as_str(), Some("Greet"));
    assert_eq!(r["kind"].as_str(), Some("cross_repo_call"));
    assert_eq!(r["confidence"].as_f64(), Some(0.4),
        "no package-deps evidence → 0.4 tier; got {r:?}");
    // callee_id points at the provider's resolved file::symbol
    assert_eq!(
        r["callee_id"].as_str(),
        Some("provider/src/lib.rs::Greet"),
        "callee_id should pin the provider file + symbol; got {r:?}"
    );
}

#[test]
fn workspace_index_emits_ambiguous_match_at_0_3() {
    // Two providers both define `run`. Per the permissive emission policy,
    // each candidate is emitted at 0.3 (one tier below the 0.4 unambiguous
    // single-match tier).
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let consumer = tmp.path().join("consumer");
    let p1 = tmp.path().join("p1");
    let p2 = tmp.path().join("p2");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&consumer);
    init_repo(&p1);
    init_repo(&p2);

    write_fake_sigil(&consumer,
        "{\"file\":\"<external>\",\"name\":\"external:utils.run\",\"kind\":\"external\",\
        \"line_start\":0,\"line_end\":0,\"struct_hash\":\"e\"}\n",
        "");
    let provider_ent = |file: &str| format!(
        "{{\"file\":\"{file}\",\"name\":\"run\",\"kind\":\"function\",\
        \"line_start\":1,\"line_end\":2,\"struct_hash\":\"r\"}}\n"
    );
    write_fake_sigil(&p1, &provider_ent("lib.py"), "");
    write_fake_sigil(&p2, &provider_ent("utils.py"), "");

    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", consumer.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", p1.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", p2.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);

    let text = fs::read_to_string(ws.join(".sigil-workspace/cross_repo_refs.jsonl")).unwrap();
    let rows: Vec<serde_json::Value> = text
        .lines().filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(rows.len(), 2, "both providers should be emitted; got {rows:?}");
    for r in &rows {
        assert_eq!(r["confidence"].as_f64(), Some(0.3),
            "ambiguous match must demote to 0.3; got {r:?}");
    }
    let callee_ids: Vec<&str> = rows.iter().map(|r| r["callee_id"].as_str().unwrap()).collect();
    assert!(callee_ids.contains(&"p1/lib.py::run"));
    assert!(callee_ids.contains(&"p2/utils.py::run"));
}

#[test]
fn workspace_index_caps_cross_repo_emissions_at_10() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let consumer = tmp.path().join("consumer");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&consumer);
    write_fake_sigil(&consumer,
        "{\"file\":\"<external>\",\"name\":\"external:init\",\"kind\":\"external\",\
        \"line_start\":0,\"line_end\":0,\"struct_hash\":\"e\"}\n",
        "");
    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", consumer.to_str().unwrap(), "--root", ws.to_str().unwrap()]);

    // 11 providers all defining `init`
    for i in 0..11 {
        let p = tmp.path().join(format!("p{i:02}"));
        init_repo(&p);
        write_fake_sigil(&p,
            "{\"file\":\"src/x.py\",\"name\":\"init\",\"kind\":\"function\",\
            \"line_start\":1,\"line_end\":2,\"struct_hash\":\"i\"}\n",
            "");
        sigil(&["workspace", "add", p.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    }
    sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);

    let text = fs::read_to_string(ws.join(".sigil-workspace/cross_repo_refs.jsonl")).unwrap();
    let rows: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(rows.len(), 10, "cap is 10 per sentinel; got {} rows", rows.len());
}

#[test]
fn workspace_index_skips_cross_repo_when_only_one_member() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let solo = tmp.path().join("solo");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&solo);
    write_fake_sigil(&solo,
        "{\"file\":\"<external>\",\"name\":\"external:foo\",\"kind\":\"external\",\
        \"line_start\":0,\"line_end\":0,\"struct_hash\":\"e\"}\n",
        "");
    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", solo.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);

    let text = fs::read_to_string(ws.join(".sigil-workspace/cross_repo_refs.jsonl")).unwrap();
    assert_eq!(text, "", "single-member workspace has nothing to resolve");
}

#[test]
fn workspace_index_truncates_stale_cross_repo_refs_when_external_removed() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let consumer = tmp.path().join("consumer");
    let provider = tmp.path().join("provider");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&consumer);
    init_repo(&provider);

    let ext_ent = "{\"file\":\"<external>\",\"name\":\"external:Greet\",\"kind\":\"external\",\
        \"line_start\":0,\"line_end\":0,\"struct_hash\":\"e\"}\n";
    write_fake_sigil(&consumer, ext_ent, "");
    write_fake_sigil(&provider,
        "{\"file\":\"src/lib.rs\",\"name\":\"Greet\",\"kind\":\"function\",\
        \"line_start\":1,\"line_end\":3,\"struct_hash\":\"g\"}\n",
        "");
    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", consumer.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", provider.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);
    let first = fs::read_to_string(ws.join(".sigil-workspace/cross_repo_refs.jsonl")).unwrap();
    assert!(!first.is_empty(), "should have one ref after first index");

    // Consumer no longer imports the external — re-index. The stale row
    // must be evicted (resolver overwrites the file).
    write_fake_sigil(&consumer, "", "");
    sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);
    let after = fs::read_to_string(ws.join(".sigil-workspace/cross_repo_refs.jsonl")).unwrap();
    assert_eq!(after, "", "stale cross-repo refs must be cleared on re-index");
}

#[test]
fn workspace_index_upgrades_to_0_6_with_direct_npm_dep_edge() {
    // consumer's package.json declares provider as a direct dependency by
    // its canonical npm name (`@org/shared`). Per the locked design, that
    // direct edge bumps single-match cross-repo confidence from 0.4 → 0.6.
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let consumer = tmp.path().join("consumer");
    let provider = tmp.path().join("provider");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&consumer);
    init_repo(&provider);

    // Provider canonical name = "@org/shared"
    fs::write(
        provider.join("package.json"),
        r#"{"name": "@org/shared", "version": "1.0.0"}"#,
    ).unwrap();
    write_fake_sigil(&provider,
        "{\"file\":\"index.js\",\"name\":\"helper\",\"kind\":\"function\",\
        \"line_start\":1,\"line_end\":2,\"struct_hash\":\"h\"}\n",
        "");

    // Consumer depends on @org/shared directly
    fs::write(
        consumer.join("package.json"),
        r#"{"name": "@org/consumer", "dependencies": {"@org/shared": "^1.0.0"}}"#,
    ).unwrap();
    // External modpath must align with the provider's canonical name —
    // real CommonJS/ESM imports look like `external:@org/shared.helper`
    // after sigil's tier-2 alias rewrite turns `lib.helper()` into
    // `<pkg-canonical>.helper`. A bare `external:helper` is not a
    // realistic shape for an npm-resolved binding.
    write_fake_sigil(&consumer,
        "{\"file\":\"<external>\",\"name\":\"external:@org/shared.helper\",\"kind\":\"external\",\
        \"line_start\":0,\"line_end\":0,\"struct_hash\":\"e\"}\n",
        "");

    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", consumer.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", provider.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);

    let text = fs::read_to_string(ws.join(".sigil-workspace/cross_repo_refs.jsonl")).unwrap();
    let rows: Vec<serde_json::Value> = text
        .lines().filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap()).collect();
    // Two emissions: (1) module-level dep edge at 0.6, (2) the specific
    // `helper` symbol binding at 0.6. Both correctly carry the direct
    // npm dep boost.
    assert!(!rows.is_empty(), "expected cross-repo refs; got nothing");
    for r in &rows {
        assert_eq!(r["confidence"].as_f64(), Some(0.6),
            "direct npm dep edge should bump every binding to 0.6; got {r:?}");
    }
    let call_row = rows.iter().find(|r| r["kind"].as_str() == Some("cross_repo_call"))
        .expect("expected one cross_repo_call binding");
    assert_eq!(call_row["name"].as_str(), Some("helper"));
    assert_eq!(call_row["callee_id"].as_str(), Some("provider/index.js::helper"));
}

// ---------------------------------------------------------------------------
// Phase 4 — incremental stamp-based refresh. `workspace index` short-circuits
// when every member's stamp matches and members.json is unchanged. --full
// forces full refresh.
// ---------------------------------------------------------------------------

fn file_mtime(path: &std::path::Path) -> std::time::SystemTime {
    fs::metadata(path).unwrap().modified().unwrap()
}

#[test]
fn workspace_index_is_noop_when_nothing_changed() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&repo);
    write_fake_sigil(&repo, "{}\n", "");

    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", repo.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);

    let stamp_path = ws.join(".sigil-workspace/manifest.json");
    let cross_path = ws.join(".sigil-workspace/cross_repo_refs.jsonl");
    let stamp_mtime_before = file_mtime(&stamp_path);
    let cross_mtime_before = file_mtime(&cross_path);

    // Sleep enough that any new write would tick the mtime past the
    // filesystem resolution floor (~1s on HFS+, ~10ms on APFS).
    std::thread::sleep(std::time::Duration::from_millis(1100));

    let out = sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);
    assert!(out.status.success(), "index: {}", String::from_utf8_lossy(&out.stderr));

    assert_eq!(file_mtime(&stamp_path), stamp_mtime_before,
        "manifest.json must NOT be rewritten on a no-op index");
    assert_eq!(file_mtime(&cross_path), cross_mtime_before,
        "cross_repo_refs.jsonl must NOT be rewritten on a no-op index");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no changes") || stderr.contains("up to date"),
        "stderr should announce the no-op skip; got: {stderr}");
}

#[test]
fn workspace_index_reruns_when_member_jsonl_changed() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let consumer = tmp.path().join("consumer");
    let provider = tmp.path().join("provider");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&consumer);
    init_repo(&provider);
    write_fake_sigil(&consumer,
        "{\"file\":\"<external>\",\"name\":\"external:Foo\",\"kind\":\"external\",\
        \"line_start\":0,\"line_end\":0,\"struct_hash\":\"e\"}\n",
        "");
    write_fake_sigil(&provider,
        "{\"file\":\"a.rs\",\"name\":\"Foo\",\"kind\":\"function\",\
        \"line_start\":1,\"line_end\":2,\"struct_hash\":\"f\"}\n",
        "");

    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", consumer.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", provider.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);

    let cross_path = ws.join(".sigil-workspace/cross_repo_refs.jsonl");
    let cross_before = fs::read_to_string(&cross_path).unwrap();
    assert!(cross_before.contains("Foo"), "initial cross-repo refs missing Foo");

    std::thread::sleep(std::time::Duration::from_millis(1100));

    // Provider now defines an additional symbol — entities.jsonl size grows
    write_fake_sigil(&provider,
        "{\"file\":\"a.rs\",\"name\":\"Foo\",\"kind\":\"function\",\
        \"line_start\":1,\"line_end\":2,\"struct_hash\":\"f\"}\n\
        {\"file\":\"b.rs\",\"name\":\"Foo\",\"kind\":\"function\",\
        \"line_start\":1,\"line_end\":2,\"struct_hash\":\"f2\"}\n",
        "");

    let out = sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);
    assert!(out.status.success(), "index: {}", String::from_utf8_lossy(&out.stderr));

    let cross_after = fs::read_to_string(&cross_path).unwrap();
    assert_ne!(cross_after, cross_before,
        "cross_repo_refs must re-run when a member's stamp changed; before={cross_before:?} after={cross_after:?}");
    let line_count = cross_after.lines().filter(|l| !l.is_empty()).count();
    assert_eq!(line_count, 2, "two matches now (a.rs + b.rs); got {line_count}");
}

#[test]
fn workspace_index_full_flag_forces_rebuild_even_when_unchanged() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&repo);
    write_fake_sigil(&repo, "{}\n", "");

    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", repo.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);

    let stamp_path = ws.join(".sigil-workspace/manifest.json");
    let mtime_before = file_mtime(&stamp_path);

    std::thread::sleep(std::time::Duration::from_millis(1100));

    let out = sigil(&["workspace", "index", "--root", ws.to_str().unwrap(), "--full"]);
    assert!(out.status.success(), "index --full: {}", String::from_utf8_lossy(&out.stderr));

    assert!(file_mtime(&stamp_path) > mtime_before,
        "--full must rewrite manifest.json even when stamps are unchanged");
}

#[test]
fn workspace_index_reruns_when_membership_changes() {
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
    sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);

    let stamp_path = ws.join(".sigil-workspace/manifest.json");
    let mtime_before = file_mtime(&stamp_path);

    std::thread::sleep(std::time::Duration::from_millis(1100));

    // Add a second member — membership changed, so the next index must re-run
    sigil(&["workspace", "add", b.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    let out = sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);
    assert!(out.status.success(), "index: {}", String::from_utf8_lossy(&out.stderr));

    assert!(file_mtime(&stamp_path) > mtime_before,
        "membership change must trigger a re-run");
    let v: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&stamp_path).unwrap()
    ).unwrap();
    assert_eq!(v["members"].as_object().unwrap().len(), 2);
}

// ---------------------------------------------------------------------------
// Phase 5 — DuckDB workspace backend. SIGIL_BACKEND=db engages the DuckDB
// path against the workspace; auto-engage covers ≥5 MB merged JSONL. Schema
// is the same union view the in-memory backend builds, materialised once
// per stamp set.
// ---------------------------------------------------------------------------

#[cfg(feature = "db")]
#[test]
fn workspace_callers_returns_same_rows_via_duckdb() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let provider = tmp.path().join("provider");
    let consumer = tmp.path().join("consumer");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&provider);
    init_repo(&consumer);

    write_fake_sigil(&provider,
        "{\"file\":\"src/lib.rs\",\"name\":\"Greet\",\"kind\":\"function\",\
        \"line_start\":1,\"line_end\":3,\"struct_hash\":\"a\"}\n",
        "");
    write_fake_sigil(&consumer,
        "{\"file\":\"src/main.rs\",\"name\":\"main\",\"kind\":\"function\",\
        \"line_start\":1,\"line_end\":5,\"struct_hash\":\"b\"}\n",
        "{\"file\":\"src/main.rs\",\"caller\":\"main\",\"name\":\"Greet\",\
        \"kind\":\"call\",\"line\":2}\n");

    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", provider.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", consumer.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);

    // In-memory result
    let mem = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args(["callers", "Greet", "--root", ws.to_str().unwrap(), "--json"])
        .env("SIGIL_BACKEND", "memory")
        .env("SIGIL_NO_AUTO_INDEX", "1")
        .output()
        .unwrap();
    assert!(mem.status.success(), "memory: {}", String::from_utf8_lossy(&mem.stderr));
    let mem_json: serde_json::Value = serde_json::from_slice(&mem.stdout).unwrap();

    // DuckDB result
    let db = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args(["callers", "Greet", "--root", ws.to_str().unwrap(), "--json"])
        .env("SIGIL_BACKEND", "db")
        .env("SIGIL_NO_AUTO_INDEX", "1")
        .output()
        .unwrap();
    assert!(db.status.success(), "duckdb: {}", String::from_utf8_lossy(&db.stderr));
    let db_json: serde_json::Value = serde_json::from_slice(&db.stdout).unwrap();

    let normalize = |v: &serde_json::Value| -> Vec<(String, String, String)> {
        v.as_array().unwrap().iter()
            .map(|r| (
                r["file"].as_str().unwrap_or("").to_string(),
                r["caller"].as_str().unwrap_or("").to_string(),
                r["name"].as_str().unwrap_or("").to_string(),
            ))
            .collect()
    };
    let mut mem_rows = normalize(&mem_json);
    let mut db_rows = normalize(&db_json);
    mem_rows.sort();
    db_rows.sort();
    assert_eq!(mem_rows, db_rows,
        "DuckDB workspace must return the same rows as in-memory");
    assert!(!mem_rows.is_empty(), "expected at least one caller; got empty");
    assert_eq!(mem_rows[0].0, "consumer/src/main.rs",
        "file should be workspace-prefixed in both backends; got {:?}", mem_rows[0]);
}

#[cfg(feature = "db")]
#[test]
fn workspace_duckdb_auto_engages_above_threshold() {
    // SIGIL_AUTO_ENGAGE_THRESHOLD_MB=0 forces DuckDB engagement even on
    // a tiny fixture, exercising the auto-engage path end-to-end.
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&repo);
    write_fake_sigil(&repo,
        "{\"file\":\"x.py\",\"name\":\"FooBar\",\"kind\":\"function\",\
        \"line_start\":1,\"line_end\":2,\"struct_hash\":\"x\"}\n",
        "");
    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", repo.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);

    let out = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args(["symbols", "repo/x.py", "--root", ws.to_str().unwrap(), "--json"])
        .env("SIGIL_AUTO_ENGAGE_THRESHOLD_MB", "0")
        .env("SIGIL_NO_AUTO_INDEX", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "auto-engage at 0MB: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v.as_array().unwrap().iter().any(|e| e["name"] == "FooBar"),
        "DuckDB auto-engaged workspace must surface entities; got {v:?}");

    // The materialised DuckDB lives under .sigil-workspace/, not .sigil/
    let db_path = ws.join(".sigil-workspace/index.duckdb");
    assert!(db_path.exists(), "workspace DuckDB file should be created at {}", db_path.display());
}

// ---------------------------------------------------------------------------
// Phase 6 — bulk-add from external manifests. `sigil workspace add
// --from-manifest <file>` parses Cargo.toml `[workspace] members`,
// pnpm-workspace.yaml `packages`, or package.json `workspaces`, expands
// globs relative to the manifest dir, and adds each as a member. Dry-run
// by default; `--apply` actually mutates members.json.
// ---------------------------------------------------------------------------

#[test]
fn workspace_bulk_add_from_cargo_workspace_manifest() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let mono = tmp.path().join("monorepo");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&mono).unwrap();

    // Cargo workspace with two crate members
    let crate_a = mono.join("crates").join("core");
    let crate_b = mono.join("crates").join("cli");
    init_repo(&crate_a);
    init_repo(&crate_b);

    let cargo_toml = mono.join("Cargo.toml");
    fs::write(&cargo_toml, "[workspace]\nmembers = [\"crates/core\", \"crates/cli\"]\n").unwrap();

    sigil(&["workspace", "init", ws.to_str().unwrap()]);

    // Dry-run: must NOT mutate members.json
    let dry = sigil(&[
        "workspace", "add",
        "--from-manifest", cargo_toml.to_str().unwrap(),
        "--root", ws.to_str().unwrap(),
    ]);
    assert!(dry.status.success(), "dry-run: {}", String::from_utf8_lossy(&dry.stderr));
    let dry_stdout = String::from_utf8_lossy(&dry.stdout).to_string();
    assert!(
        dry_stdout.contains("would add") || dry_stdout.contains("dry-run"),
        "dry-run output should announce itself; got: {dry_stdout}"
    );
    let after_dry: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(ws.join(".sigil-workspace/members.json")).unwrap()
    ).unwrap();
    assert_eq!(after_dry["members"].as_array().unwrap().len(), 0,
        "dry-run must not mutate members.json");

    // --apply: actually adds both members
    let apply = sigil(&[
        "workspace", "add",
        "--from-manifest", cargo_toml.to_str().unwrap(),
        "--root", ws.to_str().unwrap(),
        "--apply",
    ]);
    assert!(apply.status.success(), "apply: {}", String::from_utf8_lossy(&apply.stderr));

    let after_apply: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(ws.join(".sigil-workspace/members.json")).unwrap()
    ).unwrap();
    let names: Vec<&str> = after_apply["members"]
        .as_array().unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"core"), "expected 'core' in {names:?}");
    assert!(names.contains(&"cli"), "expected 'cli' in {names:?}");
}

#[test]
fn workspace_bulk_add_from_cargo_glob_pattern() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let mono = tmp.path().join("monorepo");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&mono).unwrap();

    // `crates/*` glob — three child crates
    for name in &["alpha", "beta", "gamma"] {
        init_repo(&mono.join("crates").join(name));
    }
    fs::write(mono.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/*\"]\n").unwrap();

    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    let out = sigil(&[
        "workspace", "add",
        "--from-manifest", mono.join("Cargo.toml").to_str().unwrap(),
        "--root", ws.to_str().unwrap(),
        "--apply",
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let v: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(ws.join(".sigil-workspace/members.json")).unwrap()
    ).unwrap();
    let names: Vec<&str> = v["members"].as_array().unwrap().iter()
        .map(|m| m["name"].as_str().unwrap()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(sorted, vec!["alpha", "beta", "gamma"],
        "glob should expand to all three crates; got {names:?}");
}

#[test]
fn workspace_bulk_add_from_pnpm_workspace_yaml() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let mono = tmp.path().join("mono");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&mono).unwrap();
    for name in &["web", "mobile", "shared"] {
        init_repo(&mono.join("apps").join(name));
    }
    fs::write(
        mono.join("pnpm-workspace.yaml"),
        "packages:\n  - \"apps/*\"\n",
    ).unwrap();

    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    let out = sigil(&[
        "workspace", "add",
        "--from-manifest", mono.join("pnpm-workspace.yaml").to_str().unwrap(),
        "--root", ws.to_str().unwrap(),
        "--apply",
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(ws.join(".sigil-workspace/members.json")).unwrap()
    ).unwrap();
    assert_eq!(v["members"].as_array().unwrap().len(), 3);
}

#[test]
fn workspace_bulk_add_skips_already_registered_members() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let mono = tmp.path().join("mono");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&mono).unwrap();
    let crate_a = mono.join("a");
    init_repo(&crate_a);
    fs::write(mono.join("Cargo.toml"), "[workspace]\nmembers = [\"a\"]\n").unwrap();

    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    // Pre-register `a` manually
    sigil(&["workspace", "add", crate_a.to_str().unwrap(), "--root", ws.to_str().unwrap()]);

    // Bulk add against the same manifest — `a` is already a member; the
    // dry-run preview should call that out, and --apply should be a no-op
    // for already-registered paths.
    let dry = sigil(&[
        "workspace", "add",
        "--from-manifest", mono.join("Cargo.toml").to_str().unwrap(),
        "--root", ws.to_str().unwrap(),
    ]);
    assert!(dry.status.success(), "{}", String::from_utf8_lossy(&dry.stderr));
    let stdout = String::from_utf8_lossy(&dry.stdout).to_string();
    assert!(
        stdout.contains("already") || stdout.contains("skip"),
        "preview should call out already-registered members; got: {stdout}"
    );

    let apply = sigil(&[
        "workspace", "add",
        "--from-manifest", mono.join("Cargo.toml").to_str().unwrap(),
        "--root", ws.to_str().unwrap(),
        "--apply",
    ]);
    assert!(apply.status.success());
    let v: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(ws.join(".sigil-workspace/members.json")).unwrap()
    ).unwrap();
    assert_eq!(v["members"].as_array().unwrap().len(), 1,
        "idempotent: re-applying must not duplicate");
}

// ---------------------------------------------------------------------------
// Gaps closed vs repowise: primary repo, git SHA stamp, co-change wiring,
// cross-repo contracts. See WORKSPACE_INDEXING_PLAN.md and the comparison
// audit in the v0.6.x changelog.
// ---------------------------------------------------------------------------

#[test]
fn workspace_first_added_member_is_primary_by_default() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&a);
    init_repo(&b);

    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", a.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", b.to_str().unwrap(), "--root", ws.to_str().unwrap()]);

    let v: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(ws.join(".sigil-workspace/members.json")).unwrap()
    ).unwrap();
    let members = v["members"].as_array().unwrap();
    // First added is primary; subsequent are not.
    assert_eq!(members[0]["name"].as_str(), Some("a"));
    assert_eq!(members[0]["is_primary"].as_bool(), Some(true),
        "first added should be primary; got {:?}", members[0]);
    assert!(members[1].get("is_primary").is_none(),
        "non-primary members should not serialize the flag");
}

#[test]
fn workspace_set_default_flips_primary() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&a);
    init_repo(&b);
    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", a.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", b.to_str().unwrap(), "--root", ws.to_str().unwrap()]);

    let out = sigil(&["workspace", "set-default", "b", "--root", ws.to_str().unwrap()]);
    assert!(out.status.success(), "set-default: {}", String::from_utf8_lossy(&out.stderr));

    let v: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(ws.join(".sigil-workspace/members.json")).unwrap()
    ).unwrap();
    let by_name: std::collections::HashMap<&str, &serde_json::Value> = v["members"]
        .as_array().unwrap().iter()
        .map(|m| (m["name"].as_str().unwrap(), m))
        .collect();
    assert!(by_name["a"].get("is_primary").is_none(),
        "old primary's flag must be cleared; got {:?}", by_name["a"]);
    assert_eq!(by_name["b"]["is_primary"].as_bool(), Some(true),
        "new primary must be marked; got {:?}", by_name["b"]);
}

#[test]
fn workspace_remove_primary_promotes_next_member() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&a);
    init_repo(&b);
    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", a.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", b.to_str().unwrap(), "--root", ws.to_str().unwrap()]);

    sigil(&["workspace", "remove", "a", "--root", ws.to_str().unwrap()]);

    let v: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(ws.join(".sigil-workspace/members.json")).unwrap()
    ).unwrap();
    let members = v["members"].as_array().unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0]["name"].as_str(), Some("b"));
    assert_eq!(members[0]["is_primary"].as_bool(), Some(true),
        "removing the primary must promote the next remaining member");
}

#[test]
fn workspace_index_writes_cross_repo_contract_links() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let api = tmp.path().join("api");
    let client = tmp.path().join("client");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&api);
    init_repo(&client);

    // API repo declares a FastAPI route — provider for GET /users
    fs::write(
        api.join("app.py"),
        "from fastapi import FastAPI\n\
         app = FastAPI()\n\
         @app.get('/users')\n\
         def list_users():\n\
             return []\n",
    ).unwrap();

    // Client repo calls /users with axios — consumer for the same path
    fs::write(
        client.join("client.js"),
        "import axios from 'axios';\n\
         axios.get('http://api.example.com/users');\n",
    ).unwrap();

    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", api.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", client.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    let out = sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);
    assert!(out.status.success(), "index: {}", String::from_utf8_lossy(&out.stderr));

    let path = ws.join(".sigil-workspace/contract_links.jsonl");
    assert!(path.exists(), "contract_links.jsonl must exist at {}", path.display());
    let text = fs::read_to_string(&path).unwrap();
    let rows: Vec<serde_json::Value> = text.lines().filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap()).collect();
    assert!(!rows.is_empty(), "expected at least one contract link");

    // Must have a link with api as provider and client as consumer for
    // contract_id `http::GET::/users`
    let link = rows.iter().find(|r| r["contract_id"].as_str() == Some("http::GET::/users"));
    let link = link.unwrap_or_else(|| panic!("missing GET /users link; rows={rows:?}"));
    assert_eq!(link["provider_repo"].as_str(), Some("api"));
    assert_eq!(link["consumer_repo"].as_str(), Some("client"));
    assert_eq!(link["contract_type"].as_str(), Some("http"));
}

#[test]
fn workspace_index_writes_co_changes_jsonl() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&a);
    init_repo(&b);

    // Make a coordinated commit in both repos within the same time
    // window so the co-change miner finds at least one edge.
    fs::write(a.join("a.txt"), "x").unwrap();
    git(&["add", "."], &a);
    git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "a"], &a);
    fs::write(b.join("b.txt"), "y").unwrap();
    git(&["add", "."], &b);
    git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "b"], &b);

    write_fake_sigil(&a, "{}\n", "");
    write_fake_sigil(&b, "{}\n", "");

    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", a.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", b.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    let out = sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);
    assert!(out.status.success(), "index: {}", String::from_utf8_lossy(&out.stderr));

    let path = ws.join(".sigil-workspace/co_changes.jsonl");
    assert!(path.exists(), "co_changes.jsonl must exist");
    let text = fs::read_to_string(&path).unwrap();
    // We don't assert exact count — git timing is OS-dependent. But for
    // two repos with commits seconds apart, we should get at least one edge.
    let rows: Vec<serde_json::Value> = text.lines().filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("co_changes row must be JSON"))
        .collect();
    assert!(!rows.is_empty(), "expected at least one co-change edge; got empty");
    for r in &rows {
        assert!(r.get("source_repo").is_some());
        assert!(r.get("target_repo").is_some());
        assert!(r.get("strength").is_some());
        assert!(r.get("last_date").is_some(), "schema parity with repowise");
    }
}

#[test]
fn workspace_index_captures_git_sha_per_member() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let a = tmp.path().join("a");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&a);
    // Make a real commit so `git rev-parse HEAD` resolves.
    fs::write(a.join("README.md"), "hello").unwrap();
    git(&["add", "."], &a);
    git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "init"], &a);
    write_fake_sigil(&a, "{}\n", "");

    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", a.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "index", "--root", ws.to_str().unwrap()]);

    let v: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(ws.join(".sigil-workspace/manifest.json")).unwrap()
    ).unwrap();
    let stamp = &v["members"]["a"];
    let sha = stamp["last_commit_sha"].as_str().expect("last_commit_sha must be present");
    assert_eq!(sha.len(), 40, "SHA must be 40-char hex; got {sha}");
    assert!(sha.chars().all(|c| c.is_ascii_hexdigit()), "SHA must be hex; got {sha}");
}

#[test]
fn workspace_install_writes_post_commit_hooks_into_each_member() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&a);
    init_repo(&b);

    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", a.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", b.to_str().unwrap(), "--root", ws.to_str().unwrap()]);

    let out = sigil(&["workspace", "install", "--root", ws.to_str().unwrap()]);
    assert!(out.status.success(), "install: {}", String::from_utf8_lossy(&out.stderr));

    for repo in [&a, &b] {
        let hook = repo.join(".git/hooks/post-commit");
        assert!(hook.exists(), "post-commit hook missing in {}", repo.display());
        let content = fs::read_to_string(&hook).unwrap();
        assert!(content.contains("sigil workspace index"),
            "hook should invoke workspace index; got: {content}");
        assert!(content.contains(ws.to_str().unwrap()),
            "hook should reference the workspace root; got: {content}");
        // Verify executable bit (best-effort on Unix)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&hook).unwrap().permissions().mode();
            assert!(mode & 0o100 != 0, "post-commit hook must be executable; mode={mode:o}");
        }
    }

    // Re-running install is idempotent
    let again = sigil(&["workspace", "install", "--root", ws.to_str().unwrap()]);
    assert!(again.status.success(), "re-install must succeed");
}

#[test]
fn workspace_uninstall_removes_workspace_managed_hooks() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let a = tmp.path().join("a");
    fs::create_dir_all(&ws).unwrap();
    init_repo(&a);
    sigil(&["workspace", "init", ws.to_str().unwrap()]);
    sigil(&["workspace", "add", a.to_str().unwrap(), "--root", ws.to_str().unwrap()]);
    sigil(&["workspace", "install", "--root", ws.to_str().unwrap()]);

    let hook = a.join(".git/hooks/post-commit");
    assert!(hook.exists());

    let out = sigil(&["workspace", "uninstall", "--root", ws.to_str().unwrap()]);
    assert!(out.status.success(), "uninstall: {}", String::from_utf8_lossy(&out.stderr));

    // Sigil-managed line removed; if hook becomes empty, file may stay
    // (with an empty body) or be removed — either is fine.
    if hook.exists() {
        let content = fs::read_to_string(&hook).unwrap();
        assert!(!content.contains("sigil workspace index"),
            "uninstall must drop the sigil line; got: {content}");
    }
}
