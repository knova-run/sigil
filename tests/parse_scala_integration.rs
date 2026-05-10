//! End-to-end integration test for Scala parsing.
//!
//! Drives `sigil index --files tests/fixtures/scala/sample.scala --stdout` and
//! verifies the resulting JSONL contains the expected entities (functions,
//! class, object, trait, val, package, imports) with the correct kinds and
//! visibility wiring.

use std::process::Command;

fn manifest_dir() -> String {
    env!("CARGO_MANIFEST_DIR").to_string()
}

fn fixture() -> String {
    format!("{}/tests/fixtures/scala/sample.scala", manifest_dir())
}

fn run_sigil() -> Vec<serde_json::Value> {
    let output = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .arg("index")
        .arg("--root")
        .arg(format!("{}/tests/fixtures/scala", manifest_dir()))
        .arg("--files")
        .arg(fixture())
        .arg("--stdout")
        .arg("--full")
        .output()
        .expect("failed to spawn sigil");

    assert!(
        output.status.success(),
        "sigil exited non-zero: stderr=\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8(output.stdout)
        .expect("invalid utf8 from sigil")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad json: {l}: {e}")))
        .collect()
}

fn names(entities: &[serde_json::Value]) -> Vec<&str> {
    entities.iter().map(|e| e["name"].as_str().unwrap()).collect()
}

fn find<'a>(entities: &'a [serde_json::Value], name: &str) -> &'a serde_json::Value {
    entities
        .iter()
        .find(|e| e["name"].as_str() == Some(name))
        .unwrap_or_else(|| panic!("entity not found: {name}\nhave: {:?}", names(entities)))
}

#[test]
fn extracts_package_and_imports() {
    let entities = run_sigil();
    let pkg = find(&entities, "com.example.app");
    assert_eq!(pkg["kind"].as_str(), Some("module"));

    // Filter to Entity rows (have `line_start`), not Reference rows (only `line`).
    let imports: Vec<_> = entities
        .iter()
        .filter(|e| e["kind"].as_str() == Some("import") && e.get("line_start").is_some())
        .collect();
    // `import scala.collection.immutable.List` + `import scala.util.{Try, Success}` → 3
    assert_eq!(imports.len(), 3, "expected exactly 3 import entities");
    assert!(imports
        .iter()
        .any(|e| e["name"].as_str() == Some("scala.collection.immutable.List")));
    assert!(imports
        .iter()
        .any(|e| e["name"].as_str() == Some("scala.util.Try")));
    assert!(imports
        .iter()
        .any(|e| e["name"].as_str() == Some("scala.util.Success")));
}

#[test]
fn extracts_top_level_def_with_parameter() {
    let entities = run_sigil();
    let f = find(&entities, "standalone");
    assert_eq!(f["kind"].as_str(), Some("function"));
    assert_eq!(f["visibility"].as_str(), Some("public"));
    let start = f["line_start"].as_u64().unwrap();
    assert!(start >= 25, "got start={start}");
}

#[test]
fn extracts_class_with_method() {
    let entities = run_sigil();
    let person = find(&entities, "Person");
    assert_eq!(person["kind"].as_str(), Some("class"));
    assert_eq!(person["visibility"].as_str(), Some("public"));

    let greet = find(&entities, "Person.greet");
    assert_eq!(greet["kind"].as_str(), Some("method"));
    assert_eq!(greet["parent"].as_str(), Some("Person"));

    // Private visibility is elided from JSON (Entity serde skips `private`
    // and `None` — see `is_none_or_private` in src/entity.rs). Verifying the
    // field is absent confirms the modifier was parsed as `private`.
    let helper = find(&entities, "Person.helper");
    assert!(
        helper.get("visibility").is_none() || helper["visibility"].is_null(),
        "expected private visibility to be elided, got {:?}",
        helper["visibility"]
    );
}

#[test]
fn extracts_object_with_method() {
    let entities = run_sigil();
    let obj = find(&entities, "Singleton");
    assert_eq!(obj["kind"].as_str(), Some("object"));
    let work = find(&entities, "Singleton.work");
    assert_eq!(work["kind"].as_str(), Some("method"));
}

#[test]
fn extracts_trait() {
    let entities = run_sigil();
    let iface = find(&entities, "Greeter");
    assert_eq!(iface["kind"].as_str(), Some("interface"));
}

#[test]
fn extracts_top_level_val_constant() {
    let entities = run_sigil();
    let c = find(&entities, "MAX_RETRIES");
    assert_eq!(c["kind"].as_str(), Some("constant"));
    let sig = c["sig"].as_str().unwrap_or("");
    assert!(sig.contains("3"), "expected sig to include literal 3, got {sig:?}");
}

#[test]
fn extracts_sealed_class() {
    let entities = run_sigil();
    let s = find(&entities, "Shape");
    assert_eq!(s["kind"].as_str(), Some("class"));
}
