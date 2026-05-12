//! Integration coverage for the Go-only MVP shipped in issue #15:
//!
//! * Struct-embedding heritage edges are surfaced on the embedder's
//!   `Entity.heritage` vec when `sigil index --stdout` runs against a Go file.
//! * The 3-tier call resolver tags calls correctly: same-file bare-identifier
//!   calls get `confidence: 0.95` (tier-1, repowise-aligned), calls through a file-local import alias
//!   get `confidence: 0.8` and are emitted twice (raw selector + resolved
//!   `pkg-path/Func` form).
//!
//! The tests stage the Go fixture into a per-test temp dir so the shared
//! `.sigil/` cache under `tests/fixtures/` never gets clobbered by a
//! re-index.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// Copy the Go heritage fixture into a fresh temp dir and return that path.
/// Each test gets its own dir so parallel runs don't trample one another's
/// `.sigil/` output.
fn stage_fixture() -> PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let src = PathBuf::from(format!("{}/tests/fixtures/sample_heritage.go", manifest));
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("sigil-heritage-{pid}-{id}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    std::fs::copy(&src, dir.join("sample_heritage.go")).expect("copy fixture");
    dir
}

fn run_index_in(dir: &PathBuf) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args([
            "index",
            "--root",
            dir.to_str().unwrap(),
            "--stdout",
            "--full",
        ])
        .output()
        .expect("failed to run sigil");
    assert!(
        output.status.success(),
        "sigil failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("invalid utf8")
}

fn parse_entities(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .map(|l| serde_json::from_str(l).expect("invalid json line"))
        .collect()
}

fn parse_refs(stderr: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(stderr)
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn run_index(_extra_args: &[&str]) -> String {
    let dir = stage_fixture();
    run_index_in(&dir)
}

/// `sigil index --stdout` writes entities to stdout and refs to stderr.
/// Run it once and capture both streams for the same invocation.
fn run_index_with_refs() -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let dir = stage_fixture();
    let output = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args([
            "index",
            "--root",
            dir.to_str().unwrap(),
            "--stdout",
            "--full",
        ])
        .output()
        .expect("failed to run sigil");
    assert!(
        output.status.success(),
        "sigil failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let entities = parse_entities(&String::from_utf8_lossy(&output.stdout));
    let refs = parse_refs(&output.stderr);
    (entities, refs)
}

/// Java heritage fixture staging — analogous to `stage_fixture` for Go.
fn stage_java_fixture() -> PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let src = PathBuf::from(format!(
        "{}/tests/fixtures/sample_java_heritage.java",
        manifest
    ));
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("sigil-heritage-java-{pid}-{id}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    std::fs::copy(&src, dir.join("sample_java_heritage.java")).expect("copy fixture");
    dir
}

fn run_index_java() -> Vec<serde_json::Value> {
    let dir = stage_java_fixture();
    let stdout = run_index_in(&dir);
    parse_entities(&stdout)
}

#[test]
fn java_class_extends_emits_extend_heritage_edge() {
    let entities = run_index_java();
    let dog = entities
        .iter()
        .find(|e| e["name"] == "Dog")
        .expect("Dog entity should be emitted");
    let heritage = dog["heritage"]
        .as_array()
        .expect("heritage field should be a JSON array on Dog");
    let extend_edge = heritage
        .iter()
        .find(|h| h["kind"].as_str() == Some("extend"))
        .unwrap_or_else(|| panic!("expected an `extend` edge on Dog; got {heritage:?}"));
    assert_eq!(
        extend_edge["target"].as_str(),
        Some("Animal"),
        "Dog's extend edge should target Animal; got {extend_edge:?}",
    );
}

#[test]
fn java_interface_extends_emits_extend_edges_per_parent() {
    // `interface Pet extends Runnable, Swimmer` — each parent interface
    // should appear as an `extend` edge.
    let entities = run_index_java();
    let pet = entities
        .iter()
        .find(|e| e["name"] == "Pet")
        .expect("Pet interface entity should be emitted");
    let heritage = pet["heritage"]
        .as_array()
        .expect("heritage field should be a JSON array on Pet");
    let mut extends: Vec<&str> = heritage
        .iter()
        .filter(|h| h["kind"].as_str() == Some("extend"))
        .filter_map(|h| h["target"].as_str())
        .collect();
    extends.sort();
    assert_eq!(
        extends,
        vec!["Runnable", "Swimmer"],
        "expected both parent interfaces on Pet's extend edges; got {heritage:?}",
    );
}

#[test]
fn java_class_implements_emits_implement_heritage_edges() {
    // `class Dog extends Animal implements Runnable, Swimmer` should
    // produce two `implement` edges, one per interface.
    let entities = run_index_java();
    let dog = entities
        .iter()
        .find(|e| e["name"] == "Dog")
        .expect("Dog entity should be emitted");
    let heritage = dog["heritage"]
        .as_array()
        .expect("heritage field should be a JSON array on Dog");
    let mut implements: Vec<&str> = heritage
        .iter()
        .filter(|h| h["kind"].as_str() == Some("implement"))
        .filter_map(|h| h["target"].as_str())
        .collect();
    implements.sort();
    assert_eq!(
        implements,
        vec!["Runnable", "Swimmer"],
        "expected both interfaces on Dog's implement edges; got {heritage:?}",
    );
}

/// TypeScript heritage fixture staging.
fn stage_ts_fixture() -> PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let src = PathBuf::from(format!(
        "{}/tests/fixtures/sample_ts_heritage.ts",
        manifest
    ));
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("sigil-heritage-ts-{pid}-{id}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    std::fs::copy(&src, dir.join("sample_ts_heritage.ts")).expect("copy fixture");
    dir
}

fn run_index_ts() -> Vec<serde_json::Value> {
    let dir = stage_ts_fixture();
    let stdout = run_index_in(&dir);
    parse_entities(&stdout)
}

#[test]
fn ts_class_extends_and_implements_emit_heritage_edges() {
    let entities = run_index_ts();
    let dog = entities
        .iter()
        .find(|e| e["name"] == "Dog")
        .expect("Dog entity should be emitted");
    let heritage = dog["heritage"]
        .as_array()
        .expect("heritage field should be a JSON array on Dog");
    let extend_target = heritage
        .iter()
        .find(|h| h["kind"].as_str() == Some("extend"))
        .and_then(|h| h["target"].as_str());
    assert_eq!(extend_target, Some("Animal"), "got {heritage:?}");
    let mut implements: Vec<&str> = heritage
        .iter()
        .filter(|h| h["kind"].as_str() == Some("implement"))
        .filter_map(|h| h["target"].as_str())
        .collect();
    implements.sort();
    assert_eq!(implements, vec!["Runnable", "Swimmer"], "got {heritage:?}");
}

#[test]
fn ts_interface_extends_emits_extend_edges_per_parent() {
    let entities = run_index_ts();
    let pet = entities
        .iter()
        .find(|e| e["name"] == "Pet")
        .expect("Pet interface entity should be emitted");
    let heritage = pet["heritage"]
        .as_array()
        .expect("heritage field should be a JSON array on Pet");
    let mut extends: Vec<&str> = heritage
        .iter()
        .filter(|h| h["kind"].as_str() == Some("extend"))
        .filter_map(|h| h["target"].as_str())
        .collect();
    extends.sort();
    assert_eq!(extends, vec!["Runnable", "Swimmer"], "got {heritage:?}");
}

fn stage_js_fixture() -> PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let src = PathBuf::from(format!(
        "{}/tests/fixtures/sample_js_heritage.js",
        manifest
    ));
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("sigil-heritage-js-{pid}-{id}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    std::fs::copy(&src, dir.join("sample_js_heritage.js")).expect("copy fixture");
    dir
}

#[test]
fn js_class_extends_emits_extend_heritage_edge() {
    let dir = stage_js_fixture();
    let stdout = run_index_in(&dir);
    let entities = parse_entities(&stdout);
    let dog = entities
        .iter()
        .find(|e| e["name"] == "Dog")
        .expect("Dog class entity should be emitted");
    let heritage = dog["heritage"]
        .as_array()
        .expect("heritage field should be a JSON array on Dog");
    let extend_target = heritage
        .iter()
        .find(|h| h["kind"].as_str() == Some("extend"))
        .and_then(|h| h["target"].as_str());
    assert_eq!(
        extend_target,
        Some("Animal"),
        "Dog's extend edge should target Animal; got {heritage:?}",
    );
}

fn stage_py_fixture() -> PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let src = PathBuf::from(format!(
        "{}/tests/fixtures/sample_py_heritage.py",
        manifest
    ));
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("sigil-heritage-py-{pid}-{id}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    std::fs::copy(&src, dir.join("sample_py_heritage.py")).expect("copy fixture");
    dir
}

fn run_index_py() -> Vec<serde_json::Value> {
    let dir = stage_py_fixture();
    let stdout = run_index_in(&dir);
    parse_entities(&stdout)
}

#[test]
fn python_multi_inheritance_emits_extend_edges_per_base() {
    let entities = run_index_py();
    let dog = entities
        .iter()
        .find(|e| e["name"] == "Dog")
        .expect("Dog class entity should be emitted");
    let heritage = dog["heritage"]
        .as_array()
        .expect("heritage field should be a JSON array on Dog");
    let mut extends: Vec<&str> = heritage
        .iter()
        .filter(|h| h["kind"].as_str() == Some("extend"))
        .filter_map(|h| h["target"].as_str())
        .collect();
    extends.sort();
    assert_eq!(extends, vec!["Animal", "Mixin"], "got {heritage:?}");
}

#[test]
fn python_abc_subclass_emits_extend_edge_to_abc() {
    let entities = run_index_py();
    let shape = entities
        .iter()
        .find(|e| e["name"] == "Shape")
        .expect("Shape entity should be emitted");
    let heritage = shape["heritage"]
        .as_array()
        .expect("heritage field should be a JSON array on Shape");
    let extend_target = heritage
        .iter()
        .find(|h| h["kind"].as_str() == Some("extend"))
        .and_then(|h| h["target"].as_str());
    assert_eq!(extend_target, Some("ABC"), "got {heritage:?}");
}

fn stage_rust_fixture() -> PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let src = PathBuf::from(format!(
        "{}/tests/fixtures/sample_rust_heritage.rs",
        manifest
    ));
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("sigil-heritage-rust-{pid}-{id}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    std::fs::copy(&src, dir.join("sample_rust_heritage.rs")).expect("copy fixture");
    dir
}

fn run_index_rust() -> Vec<serde_json::Value> {
    let dir = stage_rust_fixture();
    let stdout = run_index_in(&dir);
    parse_entities(&stdout)
}

#[test]
fn rust_impl_trait_for_type_emits_implement_heritage_edge() {
    // `impl Display for Widget` — the impl entity (kind="impl" after
    // normalize_kind, name="Widget") carries an `implement` edge to
    // `Display`.
    let entities = run_index_rust();
    let imp = entities
        .iter()
        .find(|e| e["name"] == "Widget" && e["kind"].as_str() == Some("impl"))
        .expect("impl entity for Widget should be emitted");
    let heritage = imp["heritage"]
        .as_array()
        .expect("heritage field should be a JSON array on the impl");
    let target = heritage
        .iter()
        .find(|h| h["kind"].as_str() == Some("implement"))
        .and_then(|h| h["target"].as_str());
    assert_eq!(
        target,
        Some("Display"),
        "expected `implement Display` on the impl entity; got {heritage:?}",
    );
}

#[test]
fn rust_trait_super_bound_emits_extend_heritage_edge() {
    // `trait Pretty: Display` — the Pretty trait gets an `extend` edge
    // pointing at Display.
    let entities = run_index_rust();
    let pretty = entities
        .iter()
        .find(|e| e["name"] == "Pretty")
        .expect("Pretty trait entity should be emitted");
    let heritage = pretty["heritage"]
        .as_array()
        .expect("heritage field should be a JSON array on Pretty");
    let target = heritage
        .iter()
        .find(|h| h["kind"].as_str() == Some("extend"))
        .and_then(|h| h["target"].as_str());
    assert_eq!(
        target,
        Some("Display"),
        "expected `extend Display` on Pretty; got {heritage:?}",
    );
}

#[test]
fn struct_embed_emits_heritage_edge() {
    let stdout = run_index(&[]);
    let entities = parse_entities(&stdout);
    let embedder = entities
        .iter()
        .find(|e| e["name"] == "Embedder")
        .expect("Embedder entity should be emitted");
    let heritage = embedder["heritage"]
        .as_array()
        .expect("heritage field should be a JSON array on the embedder");
    assert_eq!(
        heritage.len(),
        1,
        "expected exactly one embed edge on Embedder, got {:?}",
        heritage
    );
    assert_eq!(heritage[0]["kind"].as_str(), Some("embed"));
    assert_eq!(heritage[0]["target"].as_str(), Some("Base"));
}

#[test]
fn pointer_embed_resolves_to_bare_target_name() {
    let stdout = run_index(&[]);
    let entities = parse_entities(&stdout);
    let pe = entities
        .iter()
        .find(|e| e["name"] == "PointerEmbedder")
        .expect("PointerEmbedder entity should be emitted");
    let heritage = pe["heritage"].as_array().expect("heritage array missing");
    assert_eq!(heritage.len(), 1);
    assert_eq!(
        heritage[0]["target"].as_str(),
        Some("Base"),
        "pointer wrapping should be unwrapped to the bare type name, got {:?}",
        heritage[0]
    );
}

#[test]
fn qualified_embed_keeps_selector_form() {
    let stdout = run_index(&[]);
    let entities = parse_entities(&stdout);
    let qe = entities
        .iter()
        .find(|e| e["name"] == "QualifiedEmbedder")
        .expect("QualifiedEmbedder entity should be emitted");
    let heritage = qe["heritage"]
        .as_array()
        .expect("heritage array missing on QualifiedEmbedder");
    assert_eq!(heritage.len(), 1);
    assert_eq!(
        heritage[0]["target"].as_str(),
        Some("js.RawMessage"),
        "qualified embed target should preserve the selector form, got {:?}",
        heritage[0]
    );
}

#[test]
fn non_embedder_struct_has_no_heritage_field_in_json() {
    let stdout = run_index(&[]);
    let entities = parse_entities(&stdout);
    let base = entities
        .iter()
        .find(|e| e["name"] == "Base")
        .expect("Base entity should be emitted");
    // Empty heritage vec is elided by serde (`skip_serializing_if = "Vec::is_empty"`).
    assert!(
        base.get("heritage").is_none(),
        "structs with no heritage should not serialise the field; got {:?}",
        base.get("heritage")
    );
}

#[test]
fn bare_identifier_call_gets_tier1_confidence() {
    let (_, refs) = run_index_with_refs();
    let local_calls: Vec<&serde_json::Value> = refs
        .iter()
        .filter(|r| r["name"].as_str() == Some("Local") && r["kind"].as_str() == Some("call"))
        .collect();
    assert!(
        !local_calls.is_empty(),
        "should have at least one call to Local"
    );
    let confidence = local_calls[0]["confidence"]
        .as_f64()
        .expect("bare-identifier call should serialise tier-1 confidence");
    // Tier-1 confidence post-P5.17 realignment is 0.95 (repowise-compatible),
    // not 1.0. Same-file bare-identifier resolution leaves AST-uncertainty
    // headroom even on a successful local match.
    assert!(
        (confidence - 0.95).abs() < 1e-9,
        "bare-identifier confidence should be 0.95 (tier-1), got {}",
        confidence
    );
}

#[test]
fn aliased_import_call_resolves_with_confidence_zero_eight() {
    let (_, refs) = run_index_with_refs();
    // js.Marshal should appear in BOTH forms: the raw selector and the
    // import-path-qualified form. Both should be confidence 0.8.
    let selector = refs
        .iter()
        .find(|r| r["name"].as_str() == Some("js.Marshal"))
        .expect("raw selector call form should be emitted");
    let resolved = refs
        .iter()
        .find(|r| r["name"].as_str() == Some("encoding/json/Marshal"))
        .expect(
            "aliased call should resolve to the import-path-qualified form (encoding/json/Marshal)",
        );

    assert_eq!(selector["kind"].as_str(), Some("call"));
    assert_eq!(resolved["kind"].as_str(), Some("call"));
    assert_eq!(
        selector["confidence"].as_f64(),
        Some(0.8),
        "selector form confidence should be 0.8"
    );
    assert_eq!(
        resolved["confidence"].as_f64(),
        Some(0.8),
        "resolved form confidence should be 0.8"
    );
}

#[test]
fn default_import_no_alias_still_resolves() {
    // `fmt` has no alias — the local name defaults to the last path segment.
    let (_, refs) = run_index_with_refs();
    let resolved = refs
        .iter()
        .find(|r| r["name"].as_str() == Some("fmt/Println"))
        .expect("fmt.Println should resolve to fmt/Println (default import name)");
    assert_eq!(resolved["confidence"].as_f64(), Some(0.8));
}

#[test]
fn heritage_cli_reports_outgoing_and_incoming_edges() {
    // First build the on-disk index so `sigil heritage` can load it.
    let dir = stage_fixture();
    let dir_str = dir.to_str().unwrap();
    let index_status = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args(["index", "--root", dir_str, "--full"])
        .status()
        .expect("failed to run sigil index");
    assert!(index_status.success(), "sigil index failed");

    // Query Base: should have one incoming edge (Embedder embeds Base) and
    // no outgoing edges.
    let out = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args(["heritage", "Base", "--root", dir_str])
        .output()
        .expect("failed to run sigil heritage");
    assert!(
        out.status.success(),
        "sigil heritage failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("heritage output should be valid JSON");
    assert_eq!(json["symbol"].as_str(), Some("Base"));
    let incoming = json["incoming"].as_array().expect("incoming array");
    let from_names: Vec<&str> = incoming
        .iter()
        .filter_map(|e| e["from"].as_str())
        .collect();
    assert!(
        from_names.iter().any(|n| *n == "Embedder"),
        "expected Embedder among incoming.from, got {:?}",
        from_names
    );
    assert!(
        from_names.iter().any(|n| *n == "PointerEmbedder"),
        "expected PointerEmbedder among incoming.from, got {:?}",
        from_names
    );
    let outgoing = json["outgoing"].as_array().expect("outgoing array");
    assert!(
        outgoing.is_empty(),
        "Base has no outgoing heritage edges, got {:?}",
        outgoing
    );

    // Query Embedder: should have one outgoing embed edge → Base.
    let out = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args(["heritage", "Embedder", "--root", dir_str])
        .output()
        .expect("failed to run sigil heritage");
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    let outgoing = json["outgoing"].as_array().expect("outgoing array");
    assert_eq!(outgoing.len(), 1);
    assert_eq!(outgoing[0]["kind"].as_str(), Some("embed"));
    assert_eq!(outgoing[0]["target"].as_str(), Some("Base"));
}
