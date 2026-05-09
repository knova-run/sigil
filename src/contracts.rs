//! Contract extraction: HTTP routes, gRPC services, queue topics.
//!
//! Scans source files for two kinds of artifacts:
//!   - Providers: HTTP route handlers, gRPC service implementations,
//!     queue subscribers / topic consumers.
//!   - Consumers: HTTP client calls, gRPC client stubs, queue
//!     publishers.
//!
//! When run in workspace mode, the runner can match providers in one
//! repo against consumers in another to surface cross-repo contract
//! relationships without an LLM call.
//!
//! MVP coverage:
//!   - HTTP provider patterns: FastAPI (`@app.<verb>("...")`).
//!   - More patterns (Express, Spring, Laravel, Go, axios, fetch,
//!     requests, gRPC, Kafka/NATS/SQS) land incrementally.

use regex::Regex;
use serde::Serialize;
use std::path::Path;
use std::sync::OnceLock;

#[derive(Debug, Serialize, PartialEq)]
pub struct ContractRow {
    pub kind: String,         // "http" | "grpc" | "topic"
    pub role: String,         // "provider" | "consumer"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub file: String,
    pub line: u32,
    pub language: String,
    pub framework: String,
}

/// Walk `root` and return all contract rows discovered.
pub fn extract_from_root(root: &Path) -> Vec<ContractRow> {
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<ContractRow>) {
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
            walk(root, &path, out);
            continue;
        }
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().to_string();
        match ext {
            "py" => out.extend(scan_python(&rel, &text)),
            "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => {
                out.extend(scan_js_ts(&rel, &text, ext))
            }
            _ => {}
        }
    }
}

fn is_skipped_dir(path: &Path) -> bool {
    let name = match path.file_name() {
        Some(n) => n.to_string_lossy(),
        None => return false,
    };
    matches!(
        name.as_ref(),
        ".git" | "node_modules" | "vendor" | "target" | "dist" | "build" | ".venv" | "__pycache__"
    )
}

fn fastapi_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Match @<app>.<verb>("path") with optional surrounding whitespace and
        // additional kwargs after the path. Captures the verb and path.
        Regex::new(
            r#"@\s*[A-Za-z_][A-Za-z0-9_]*\.(get|post|put|delete|patch|options|head)\(\s*['"]([^'"]+)['"]"#,
        )
        .unwrap()
    })
}

fn express_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Match `app.get('/path', ...)` and similar Express verbs. Captures
        // the verb and the literal route string.
        Regex::new(
            r#"\b[A-Za-z_][A-Za-z0-9_]*\.(get|post|put|delete|patch|options|head|all)\(\s*['"`]([^'"`]+)['"`]"#,
        )
        .unwrap()
    })
}

fn axios_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Match axios.get('/path', ...) — also catches fetch.get-style code.
        // Distinguishes from server-side patterns by the `axios.` prefix
        // explicitly.
        Regex::new(
            r#"\baxios\.(get|post|put|delete|patch|options|head)\(\s*['"`]([^'"`]+)['"`]"#,
        )
        .unwrap()
    })
}

fn scan_js_ts(file: &str, text: &str, ext: &str) -> Vec<ContractRow> {
    let language = if ext.starts_with('t') { "typescript" } else { "javascript" };
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        // axios consumer first — `axios.<verb>` would also match the express
        // regex (it's a method-call shape), so check axios specifically.
        if let Some(caps) = axios_re().captures(line) {
            out.push(ContractRow {
                kind: "http".to_string(),
                role: "consumer".to_string(),
                method: Some(caps[1].to_uppercase()),
                path: Some(caps[2].to_string()),
                topic: None,
                file: file.to_string(),
                line: (i + 1) as u32,
                language: language.to_string(),
                framework: "axios".to_string(),
            });
            continue;
        }
        if let Some(caps) = express_re().captures(line) {
            out.push(ContractRow {
                kind: "http".to_string(),
                role: "provider".to_string(),
                method: Some(caps[1].to_uppercase()),
                path: Some(caps[2].to_string()),
                topic: None,
                file: file.to_string(),
                line: (i + 1) as u32,
                language: language.to_string(),
                framework: "express".to_string(),
            });
        }
    }
    out
}

fn scan_python(file: &str, text: &str) -> Vec<ContractRow> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if let Some(caps) = fastapi_re().captures(line) {
            out.push(ContractRow {
                kind: "http".to_string(),
                role: "provider".to_string(),
                method: Some(caps[1].to_uppercase()),
                path: Some(caps[2].to_string()),
                topic: None,
                file: file.to_string(),
                line: (i + 1) as u32,
                language: "python".to_string(),
                framework: "fastapi".to_string(),
            });
        }
    }
    out
}
