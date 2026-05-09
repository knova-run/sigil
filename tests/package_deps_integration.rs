//! Integration tests for `sigil package-deps` — extract package
//! dependency edges from manifest files.
//!
//! Output format: JSONL, one row per dependency edge with fields
//! `{ source_repo, manifest, dependency, version_spec, kind }` where
//! `kind` is the manifest format ("go", "npm", "pip", "cargo", "maven").

use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn run_package_deps(root: &std::path::Path) -> (String, String, bool) {
    let output = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .arg("package-deps")
        .arg("--root")
        .arg(root)
        .output()
        .expect("failed to run sigil");
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    )
}

fn parse_lines(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("line should be JSON"))
        .collect()
}

#[test]
fn extracts_npm_dependencies_from_package_json() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("package.json"),
        r#"{
            "name": "myapp",
            "dependencies": { "react": "^18.0.0", "@org/shared-lib": "1.2.3" },
            "devDependencies": { "typescript": "5.4.0" }
        }"#,
    )
    .unwrap();
    let (stdout, stderr, ok) = run_package_deps(tmp.path());
    assert!(ok, "stderr: {stderr}");
    let rows = parse_lines(&stdout);
    let names: Vec<&str> = rows.iter().map(|r| r["dependency"].as_str().unwrap()).collect();
    assert!(names.contains(&"react"), "expected react in {names:?}");
    assert!(names.contains(&"@org/shared-lib"), "expected scoped pkg in {names:?}");
    assert!(names.contains(&"typescript"), "expected typescript dev-dep in {names:?}");
    let react = rows.iter().find(|r| r["dependency"] == "react").unwrap();
    assert_eq!(react["kind"], "npm");
    assert_eq!(react["version_spec"], "^18.0.0");
}

#[test]
fn extracts_go_module_require_edges() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("go.mod"),
        "module example.com/myapp\n\
         \n\
         go 1.22\n\
         \n\
         require (\n\
         \tgithub.com/suprsend/go-dataproxy v0.0.0-20260427190217-5987cddd473a\n\
         \tgithub.com/aws/aws-sdk-go v1.50.0\n\
         )\n\
         \n\
         require github.com/stretchr/testify v1.8.4 // indirect\n",
    )
    .unwrap();
    let (stdout, stderr, ok) = run_package_deps(tmp.path());
    assert!(ok, "stderr: {stderr}");
    let rows = parse_lines(&stdout);
    let names: Vec<&str> = rows.iter().map(|r| r["dependency"].as_str().unwrap()).collect();
    assert!(
        names.contains(&"github.com/suprsend/go-dataproxy"),
        "expected suprsend/go-dataproxy in {names:?}"
    );
    assert!(
        names.contains(&"github.com/aws/aws-sdk-go"),
        "expected aws-sdk-go in {names:?}"
    );
    let dataproxy_row = rows
        .iter()
        .find(|r| r["dependency"] == "github.com/suprsend/go-dataproxy")
        .unwrap();
    assert_eq!(dataproxy_row["kind"], "go");
    assert_eq!(
        dataproxy_row["version_spec"],
        "v0.0.0-20260427190217-5987cddd473a"
    );
}
