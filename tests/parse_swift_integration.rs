//! End-to-end integration test for Swift parsing.
//!
//! Drives `sigil index --files tests/fixtures/swift/sample.swift --stdout` and
//! verifies the resulting JSONL contains the expected entities (imports,
//! top-level function, struct + method + property, class + method,
//! protocol → interface, enum, extension, top-level `let` constant) with
//! the correct kinds and visibility wiring.

use std::process::Command;

fn manifest_dir() -> String {
    env!("CARGO_MANIFEST_DIR").to_string()
}

fn fixture() -> String {
    format!("{}/tests/fixtures/swift/sample.swift", manifest_dir())
}

fn run_sigil() -> Vec<serde_json::Value> {
    let output = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .arg("index")
        .arg("--root")
        .arg(format!("{}/tests/fixtures/swift", manifest_dir()))
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
    entities
        .iter()
        .map(|e| e["name"].as_str().unwrap_or("<no name>"))
        .collect()
}

fn find<'a>(entities: &'a [serde_json::Value], name: &str) -> &'a serde_json::Value {
    entities
        .iter()
        .find(|e| e["name"].as_str() == Some(name))
        .unwrap_or_else(|| panic!("entity not found: {name}\nhave: {:?}", names(entities)))
}

/// Single test (per task) asserting that all major Swift node kinds parse
/// into the expected symbols. Keeping the assertions in one test mirrors
/// the brief; the per-feature granularity is covered by the parser unit
/// tests in `src/parser/swift.rs`.
#[test]
fn extracts_representative_swift_snippet() {
    let entities = run_sigil();

    // Imports: 2 import entities + 2 import references.
    let imports: Vec<_> = entities
        .iter()
        .filter(|e| e["kind"].as_str() == Some("import") && e.get("line_start").is_some())
        .collect();
    assert_eq!(
        imports.len(),
        2,
        "expected 2 import entities, got {:?}",
        names(&entities)
    );
    assert!(imports.iter().any(|e| e["name"].as_str() == Some("Foundation")));
    assert!(imports.iter().any(|e| e["name"].as_str() == Some("UIKit")));

    // Top-level function — default Swift visibility is `internal`. Internal
    // is serialized as-is (only `private` is elided by `is_none_or_private`).
    let standalone = find(&entities, "standalone");
    assert_eq!(standalone["kind"].as_str(), Some("function"));
    assert_eq!(standalone["visibility"].as_str(), Some("internal"));

    // Struct with method + properties.
    let point = find(&entities, "Point");
    assert_eq!(point["kind"].as_str(), Some("class"));
    let mag = find(&entities, "Point.magnitude");
    assert_eq!(mag["kind"].as_str(), Some("method"));
    assert_eq!(mag["parent"].as_str(), Some("Point"));
    let px = find(&entities, "Point.x");
    assert_eq!(px["kind"].as_str(), Some("property"));
    let py = find(&entities, "Point.y");
    assert_eq!(py["kind"].as_str(), Some("property"));

    // Class with method.
    let person = find(&entities, "Person");
    assert_eq!(person["kind"].as_str(), Some("class"));
    let greet = find(&entities, "Person.greet");
    assert_eq!(greet["kind"].as_str(), Some("method"));
    assert_eq!(greet["visibility"].as_str(), Some("public"));

    // Private member visibility is elided from JSON output (see
    // `is_none_or_private` in src/entity.rs). Verifying the field is absent
    // confirms the modifier was parsed as `private`.
    let helper = find(&entities, "Person.helper");
    assert!(
        helper.get("visibility").is_none() || helper["visibility"].is_null(),
        "expected private visibility to be elided, got {:?}",
        helper["visibility"]
    );

    // Protocol — surfaces as `interface`.
    let greeter = find(&entities, "Greeter");
    assert_eq!(greeter["kind"].as_str(), Some("interface"));

    // Enum + cases. Enum surfaces as `class`; its cases are `constant` entries
    // under the enum's name.
    let dir = find(&entities, "Direction");
    assert_eq!(dir["kind"].as_str(), Some("class"));
    let north = find(&entities, "Direction.north");
    assert_eq!(north["kind"].as_str(), Some("constant"));
    assert_eq!(north["parent"].as_str(), Some("Direction"));

    // Extension — surfaces as `class` with the extended type's name.
    // (There will be two entities named "Person" — the class and the extension.)
    let person_entities: Vec<_> = entities
        .iter()
        .filter(|e| e["name"].as_str() == Some("Person") && e["kind"].as_str() == Some("class"))
        .collect();
    assert!(
        person_entities.len() >= 2,
        "expected at least 2 Person class entities (class + extension), found {}",
        person_entities.len()
    );
    let describe = find(&entities, "Person.describe");
    assert_eq!(describe["kind"].as_str(), Some("method"));

    // Top-level `let MAX_RETRIES: Int = 3` → constant with RHS sig.
    let max_retries = find(&entities, "MAX_RETRIES");
    assert_eq!(max_retries["kind"].as_str(), Some("constant"));
    let sig = max_retries["sig"].as_str().unwrap_or("");
    assert!(
        sig.contains('3'),
        "expected MAX_RETRIES sig to include literal 3, got {sig:?}"
    );

    // fileprivate maps to internal (three-bucket schema).
    let fp = find(&entities, "filePrivateHelper");
    assert_eq!(fp["visibility"].as_str(), Some("internal"));

    // Explicit internal stays internal.
    let ih = find(&entities, "internalHelper");
    assert_eq!(ih["visibility"].as_str(), Some("internal"));
}
