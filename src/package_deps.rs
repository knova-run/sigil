//! Package dependency edge extraction from manifest files.
//!
//! Outputs one row per (manifest, dependency) edge. Supported manifests:
//!   - `go.mod`         (kind = "go")
//!   - `package.json`   (kind = "npm")
//!   - `pyproject.toml` (kind = "pip")
//!   - `Cargo.toml`     (kind = "cargo")
//!   - `pom.xml`        (kind = "maven")
//!
//! The MVP focuses on `go.mod` since it's the most direct way to detect
//! cross-repo edges in a Go monorepo (the kinesis-consumer + sqs-consumer
//! + go-dataproxy seam in Knova). Other formats land incrementally.

use serde::Serialize;
use std::path::Path;

#[derive(Debug, Serialize, PartialEq)]
pub struct PackageEdge {
    pub source_repo: String,
    pub manifest: String,
    pub dependency: String,
    pub version_spec: String,
    pub kind: String,
}

/// Walk `root`, find all known manifest files, and return their edges.
pub fn extract_from_root(root: &Path) -> Vec<PackageEdge> {
    let mut out = Vec::new();
    let source_repo = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string());
    walk(root, root, &source_repo, &mut out);
    out
}

fn walk(root: &Path, dir: &Path, source_repo: &str, out: &mut Vec<PackageEdge>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if is_skipped_dir(&path) {
                continue;
            }
            walk(root, &path, source_repo, out);
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().to_string();
        match name {
            "go.mod" => {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    out.extend(parse_go_mod(source_repo, &rel, &text));
                }
            }
            "package.json" => {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    out.extend(parse_package_json(source_repo, &rel, &text));
                }
            }
            _ => {}
        }
    }
}

/// Parse a `package.json` file. Captures `dependencies` and
/// `devDependencies` as edges with kind="npm".
pub fn parse_package_json(source_repo: &str, manifest: &str, text: &str) -> Vec<PackageEdge> {
    let mut out = Vec::new();
    let Ok(doc): Result<serde_json::Value, _> = serde_json::from_str(text) else {
        return out;
    };
    for section in &["dependencies", "devDependencies"] {
        if let Some(map) = doc.get(section).and_then(|v| v.as_object()) {
            for (dep, spec) in map {
                let version = spec.as_str().unwrap_or("").to_string();
                out.push(PackageEdge {
                    source_repo: source_repo.to_string(),
                    manifest: manifest.to_string(),
                    dependency: dep.clone(),
                    version_spec: version,
                    kind: "npm".to_string(),
                });
            }
        }
    }
    out
}

fn is_skipped_dir(path: &Path) -> bool {
    let name = match path.file_name() {
        Some(n) => n.to_string_lossy(),
        None => return false,
    };
    matches!(
        name.as_ref(),
        ".git" | "node_modules" | "vendor" | "target" | "dist" | "build" | ".venv" | ".sigil"
    )
}

/// Parse a `go.mod` file into PackageEdges. Handles both block-form
/// `require (...)` and single-line `require X v1.2.3` forms.
pub fn parse_go_mod(source_repo: &str, manifest: &str, text: &str) -> Vec<PackageEdge> {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }
        if trimmed == "require (" {
            in_block = true;
            continue;
        }
        if in_block && trimmed == ")" {
            in_block = false;
            continue;
        }
        if in_block {
            push_require_line(source_repo, manifest, trimmed, &mut out);
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("require ") {
            push_require_line(source_repo, manifest, rest.trim(), &mut out);
        }
    }
    out
}

fn push_require_line(source_repo: &str, manifest: &str, line: &str, out: &mut Vec<PackageEdge>) {
    // Strip trailing `// indirect` and similar inline comments.
    let no_comment = match line.split_once("//") {
        Some((head, _)) => head.trim(),
        None => line,
    };
    let mut parts = no_comment.split_whitespace();
    let (Some(dep), Some(version)) = (parts.next(), parts.next()) else {
        return;
    };
    out.push(PackageEdge {
        source_repo: source_repo.to_string(),
        manifest: manifest.to_string(),
        dependency: dep.to_string(),
        version_spec: version.to_string(),
        kind: "go".to_string(),
    });
}
