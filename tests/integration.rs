use std::process::Command;

/// Helper: run sigil index on the fixtures directory and return stdout.
fn run_sigil_index(fixture_dir: &str, extra_args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .arg("index")
        .arg("--root")
        .arg(fixture_dir)
        .arg("--stdout")
        .arg("--full")
        .args(extra_args)
        .output()
        .expect("failed to run sigil");

    assert!(output.status.success(), "sigil failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).expect("invalid utf8")
}

fn fixture_path() -> String {
    let manifest = env!("CARGO_MANIFEST_DIR");
    format!("{}/tests/fixtures", manifest)
}

#[test]
fn indexes_python_fixture() {
    let output = run_sigil_index(&fixture_path(), &["--files", &format!("{}/sample.py", fixture_path())]);
    let entities: Vec<serde_json::Value> = output.lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // Should find imports, class, methods, function
    assert!(entities.len() >= 4, "expected at least 4 entities, got {}", entities.len());

    let names: Vec<&str> = entities.iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"UserService"), "missing UserService class");
    assert!(names.contains(&"standalone_function"), "missing standalone_function");
}

#[test]
fn methods_carry_qualified_name_with_parent_prefix() {
    let output = run_sigil_index(
        &fixture_path(),
        &["--files", &format!("{}/sample.py", fixture_path())],
    );
    let entities: Vec<serde_json::Value> = output
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let get_user = entities
        .iter()
        .find(|e| e["name"] == "UserService.get_user")
        .expect("expected UserService.get_user method in fixture");
    assert_eq!(
        get_user["qualified_name"].as_str(),
        Some("UserService::get_user"),
        "qualified_name should be Class::method (`::` form), got {:?}",
        get_user.get("qualified_name")
    );
}

#[test]
fn indexes_rust_fixture() {
    let output = run_sigil_index(&fixture_path(), &["--files", &format!("{}/sample.rs", fixture_path())]);
    let entities: Vec<serde_json::Value> = output.lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    assert!(entities.len() >= 4, "expected at least 4 entities, got {}", entities.len());

    let names: Vec<&str> = entities.iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"Config"), "missing Config struct");
    assert!(names.contains(&"validate_port"), "missing validate_port fn");

    // Check derive markers on Config
    let config = entities.iter().find(|e| e["name"] == "Config" && e["kind"] == "struct").unwrap();
    let meta = config["meta"].as_array().expect("Config should have meta");
    let meta_strs: Vec<&str> = meta.iter().map(|v| v.as_str().unwrap()).collect();
    assert!(meta_strs.contains(&"Serialize"), "missing Serialize in meta");
    assert!(meta_strs.contains(&"Clone"), "missing Clone in meta");
}

#[test]
fn indexes_go_fixture() {
    let output = run_sigil_index(&fixture_path(), &["--files", &format!("{}/sample.go", fixture_path())]);
    let entities: Vec<serde_json::Value> = output.lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    assert!(entities.len() >= 3, "expected at least 3 entities, got {}", entities.len());

    let names: Vec<&str> = entities.iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"NewConfig"), "missing NewConfig");
}

#[test]
fn deterministic_output() {
    let out1 = run_sigil_index(&fixture_path(), &[]);
    let out2 = run_sigil_index(&fixture_path(), &[]);
    assert_eq!(out1, out2, "output must be deterministic across runs");
}

#[test]
fn struct_hash_always_present() {
    let output = run_sigil_index(&fixture_path(), &[]);
    for line in output.lines() {
        let entity: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(entity["struct_hash"].is_string(), "struct_hash must always be a string: {:?}", entity["name"]);
        assert_eq!(entity["struct_hash"].as_str().unwrap().len(), 16, "struct_hash must be 16 hex chars");
    }
}

#[test]
fn entities_sorted_by_file_then_line() {
    let output = run_sigil_index(&fixture_path(), &[]);
    let entities: Vec<serde_json::Value> = output.lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    for window in entities.windows(2) {
        let a_file = window[0]["file"].as_str().unwrap();
        let b_file = window[1]["file"].as_str().unwrap();
        let a_line = window[0]["line_start"].as_u64().unwrap();
        let b_line = window[1]["line_start"].as_u64().unwrap();

        assert!(
            a_file < b_file || (a_file == b_file && a_line <= b_line),
            "entities not sorted: {}:{} should come before {}:{}",
            a_file, a_line, b_file, b_line
        );
    }
}

#[test]
fn indexes_json_fixture() {
    let output = run_sigil_index(
        &fixture_path(),
        &["--files", &format!("{}/sample.json", fixture_path())],
    );
    let entities: Vec<serde_json::Value> = output.lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // Root keys: name, version, settings, dependencies
    // Nested: theme (under settings), color, font_size (under theme),
    //         debug, tags (under settings), serde, blake3 (under dependencies)
    assert!(entities.len() >= 10, "expected at least 10 entities, got {}", entities.len());

    let names: Vec<&str> = entities.iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"name"), "missing 'name' key");
    assert!(names.contains(&"settings"), "missing 'settings' key");
    assert!(names.contains(&"theme"), "missing 'theme' key");
    assert!(names.contains(&"color"), "missing 'color' key");
    assert!(names.contains(&"dependencies"), "missing 'dependencies' key");

    // Check kinds
    let settings = entities.iter().find(|e| e["name"] == "settings").unwrap();
    assert_eq!(settings["kind"].as_str().unwrap(), "object");

    let tags = entities.iter().find(|e| e["name"] == "tags").unwrap();
    assert_eq!(tags["kind"].as_str().unwrap(), "array");

    let color = entities.iter().find(|e| e["name"] == "color").unwrap();
    assert_eq!(color["kind"].as_str().unwrap(), "property");
    assert_eq!(color["parent"].as_str().unwrap(), "theme");

    // Check signatures
    assert_eq!(settings["sig"].as_str().unwrap(), "\"settings\": object");
    assert_eq!(color["sig"].as_str().unwrap(), "\"color\": string");

    // All struct_hashes must be 16 hex chars
    for entity in &entities {
        let sh = entity["struct_hash"].as_str().unwrap();
        assert_eq!(sh.len(), 16, "struct_hash wrong length for {}", entity["name"]);
    }
}

#[test]
fn indexes_yaml_fixture() {
    let output = run_sigil_index(
        &fixture_path(),
        &["--files", &format!("{}/sample.yaml", fixture_path())],
    );
    let entities: Vec<serde_json::Value> = output.lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // Root keys: name, version, settings, dependencies
    // Nested: theme (under settings), color, font_size (under theme),
    //         debug, tags (under settings), serde, blake3 (under dependencies)
    assert!(entities.len() >= 10, "expected at least 10 entities, got {}", entities.len());

    let names: Vec<&str> = entities.iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"name"), "missing 'name' key");
    assert!(names.contains(&"settings"), "missing 'settings' key");
    assert!(names.contains(&"theme"), "missing 'theme' key");
    assert!(names.contains(&"color"), "missing 'color' key");
    assert!(names.contains(&"dependencies"), "missing 'dependencies' key");

    // Check kinds
    let settings = entities.iter().find(|e| e["name"] == "settings").unwrap();
    assert_eq!(settings["kind"].as_str().unwrap(), "object");

    let tags = entities.iter().find(|e| e["name"] == "tags").unwrap();
    assert_eq!(tags["kind"].as_str().unwrap(), "array");

    let color = entities.iter().find(|e| e["name"] == "color").unwrap();
    assert_eq!(color["kind"].as_str().unwrap(), "property");
    assert_eq!(color["parent"].as_str().unwrap(), "theme");

    // Check signatures
    assert_eq!(settings["sig"].as_str().unwrap(), "\"settings\": object");
    assert_eq!(color["sig"].as_str().unwrap(), "\"color\": string");

    // All struct_hashes must be 16 hex chars
    for entity in &entities {
        let sh = entity["struct_hash"].as_str().unwrap();
        assert_eq!(sh.len(), 16, "struct_hash wrong length for {}", entity["name"]);
    }
}

#[test]
fn indexes_toml_fixture() {
    let output = run_sigil_index(
        &fixture_path(),
        &["--files", &format!("{}/sample.toml", fixture_path())],
    );
    let entities: Vec<serde_json::Value> = output.lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // Root keys: name, version, settings, dependencies
    // Nested: theme (under settings), color, font_size (under theme),
    //         debug, tags (under settings), serde, blake3 (under dependencies)
    assert!(entities.len() >= 10, "expected at least 10 entities, got {}", entities.len());

    let names: Vec<&str> = entities.iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"name"), "missing 'name' key");
    assert!(names.contains(&"settings"), "missing 'settings' key");
    assert!(names.contains(&"theme"), "missing 'theme' key");
    assert!(names.contains(&"color"), "missing 'color' key");
    assert!(names.contains(&"dependencies"), "missing 'dependencies' key");

    // Check kinds
    let settings = entities.iter().find(|e| e["name"] == "settings").unwrap();
    assert_eq!(settings["kind"].as_str().unwrap(), "object");

    let tags = entities.iter().find(|e| e["name"] == "tags").unwrap();
    assert_eq!(tags["kind"].as_str().unwrap(), "array");

    let color = entities.iter().find(|e| e["name"] == "color").unwrap();
    assert_eq!(color["kind"].as_str().unwrap(), "property");
    assert_eq!(color["parent"].as_str().unwrap(), "theme");

    // Check signatures
    assert_eq!(settings["sig"].as_str().unwrap(), "\"settings\": table");
    assert_eq!(color["sig"].as_str().unwrap(), "\"color\": string");

    // All struct_hashes must be 16 hex chars
    for entity in &entities {
        let sh = entity["struct_hash"].as_str().unwrap();
        assert_eq!(sh.len(), 16, "struct_hash wrong length for {}", entity["name"]);
    }
}

#[test]
fn incremental_caching_works() {
    // Use a temp directory with a copy of fixtures
    let tmp = std::env::temp_dir().join("sigil_incr_test");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    // Copy a fixture file
    std::fs::copy(
        format!("{}/sample.py", fixture_path()),
        tmp.join("sample.py"),
    ).unwrap();

    // First run: should parse
    let output1 = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .arg("index")
        .arg("--root")
        .arg(&tmp)
        .arg("--verbose")
        .arg("--full")
        .output()
        .expect("failed to run sigil");
    assert!(output1.status.success());
    let stderr1 = String::from_utf8_lossy(&output1.stderr);
    assert!(stderr1.contains("files parsed"), "first run should parse files");

    // cache.json should exist
    assert!(tmp.join(".sigil/cache.json").exists(), "cache.json must exist after first run");

    // Second run without --full: should use cache
    let output2 = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .arg("index")
        .arg("--root")
        .arg(&tmp)
        .arg("--verbose")
        .output()
        .expect("failed to run sigil");
    assert!(output2.status.success());
    let stderr2 = String::from_utf8_lossy(&output2.stderr);
    assert!(stderr2.contains("0 files parsed"), "second run should parse 0 files");

    // Output should be identical
    let entities1 = std::fs::read_to_string(tmp.join(".sigil/entities.jsonl")).unwrap();
    let run3 = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .arg("index")
        .arg("--root")
        .arg(&tmp)
        .arg("--stdout")
        .output()
        .expect("failed");
    let entities_stdout = String::from_utf8(run3.stdout).unwrap();
    assert_eq!(entities1, entities_stdout, "file output and stdout must match");

    std::fs::remove_dir_all(&tmp).ok();
}
