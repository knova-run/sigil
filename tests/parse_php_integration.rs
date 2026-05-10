//! End-to-end integration test for PHP parsing.
//!
//! Drives `sigil index --files tests/fixtures/php/sample.php --stdout` and
//! verifies the resulting JSONL contains the expected entities (namespace,
//! use, top-level function, class with method + property + class const,
//! interface, trait, enum, top-level const) with the correct kinds and
//! visibility wiring.

use std::process::Command;

fn manifest_dir() -> String {
    env!("CARGO_MANIFEST_DIR").to_string()
}

fn fixture() -> String {
    format!("{}/tests/fixtures/php/sample.php", manifest_dir())
}

fn run_sigil() -> Vec<serde_json::Value> {
    let output = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .arg("index")
        .arg("--root")
        .arg(format!("{}/tests/fixtures/php", manifest_dir()))
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
fn extracts_namespace_and_uses() {
    let entities = run_sigil();
    let ns = find(&entities, "App\\Service");
    assert_eq!(ns["kind"].as_str(), Some("module"));

    // Imports are emitted as entities (with line_start/line_end) as well
    // as references. Filter to entities here.
    let imports: Vec<_> = entities
        .iter()
        .filter(|e| e["kind"].as_str() == Some("import") && e.get("line_start").is_some())
        .collect();
    assert_eq!(imports.len(), 2, "expected exactly 2 import entities");
    assert!(imports.iter().any(|e| e["name"].as_str() == Some("App\\Util\\Logger")));
    // The aliased import keeps its original name in the entity output;
    // the `as C` alias is tracked at the parser level (see
    // src/parser/php.rs unit tests).
    assert!(imports.iter().any(|e| e["name"].as_str() == Some("App\\Util\\Cache")));
}

#[test]
fn extracts_top_level_function() {
    let entities = run_sigil();
    let f = find(&entities, "standalone");
    assert_eq!(f["kind"].as_str(), Some("function"));
    assert_eq!(f["visibility"].as_str(), Some("public"));
    let start = f["line_start"].as_u64().unwrap();
    let end = f["line_end"].as_u64().unwrap();
    assert!(start >= 40 && end >= start + 1, "got [{start}, {end}]");
}

#[test]
fn extracts_class_with_method_and_property() {
    let entities = run_sigil();
    let person = find(&entities, "Person");
    assert_eq!(person["kind"].as_str(), Some("class"));
    assert_eq!(person["visibility"].as_str(), Some("public"));

    let greet = find(&entities, "Person::greet");
    assert_eq!(greet["kind"].as_str(), Some("method"));
    assert_eq!(greet["parent"].as_str(), Some("Person"));
    assert_eq!(greet["visibility"].as_str(), Some("public"));

    // Private visibility is elided from the serialized JSON when the
    // entity layer drops it; verify the field is either absent or null.
    let helper = find(&entities, "Person::helper");
    assert!(
        helper.get("visibility").is_none() || helper["visibility"].is_null(),
        "expected private visibility to be elided, got {:?}",
        helper["visibility"]
    );

    let name_prop = find(&entities, "Person::$name");
    assert_eq!(name_prop["kind"].as_str(), Some("property"));
    assert_eq!(name_prop["parent"].as_str(), Some("Person"));
    assert_eq!(name_prop["visibility"].as_str(), Some("public"));

    let species = find(&entities, "Person::SPECIES");
    assert_eq!(species["kind"].as_str(), Some("constant"));
    let sig = species["sig"].as_str().unwrap_or("");
    assert!(sig.contains("human"), "expected sig to include 'human', got {sig:?}");
}

#[test]
fn extracts_interface() {
    let entities = run_sigil();
    let iface = find(&entities, "Greeter");
    assert_eq!(iface["kind"].as_str(), Some("interface"));
    let g = find(&entities, "Greeter::greet");
    assert_eq!(g["kind"].as_str(), Some("method"));
}

#[test]
fn extracts_trait() {
    let entities = run_sigil();
    // Traits surface as `class` — documented choice in src/parser/php.rs.
    let t = find(&entities, "Helpful");
    assert_eq!(t["kind"].as_str(), Some("class"));
    let h = find(&entities, "Helpful::help");
    assert_eq!(h["kind"].as_str(), Some("method"));
}

#[test]
fn extracts_enum() {
    let entities = run_sigil();
    let e = find(&entities, "Status");
    assert_eq!(e["kind"].as_str(), Some("class"));
}

#[test]
fn extracts_top_level_const() {
    let entities = run_sigil();
    let c = find(&entities, "MAX_RETRIES");
    assert_eq!(c["kind"].as_str(), Some("constant"));
    let sig = c["sig"].as_str().unwrap_or("");
    assert!(sig.contains("3"), "expected sig to include literal 3, got {sig:?}");
}
