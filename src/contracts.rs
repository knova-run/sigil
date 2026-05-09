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
    /// Composite join key matching repowise's contract_id form:
    ///   - HTTP:  `http::<METHOD>::<NORMALIZED_PATH>`
    ///   - gRPC:  `grpc::<Service>/<Method>`
    ///   - Topic: `topic::<topic-name>`
    /// Path-style params (`:id`, `{userId}`, `[id]`) are normalized to
    /// `{param}` so the same contract from different framework conventions
    /// produces an identical id — required for cross-repo matching.
    pub contract_id: String,
    pub kind: String,         // "http" | "grpc" | "topic"
    pub role: String,         // "provider" | "consumer" | "publisher" | "subscriber"
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

/// Normalize an HTTP path so paths from different framework conventions
/// produce an identical canonical form. Mirrors repowise's normalize_http_path:
///
///   - strip query string
///   - strip trailing slash (preserve root `/`)
///   - lowercase
///   - collapse `:param`, `{name}`, `[name]` → `{param}`
pub fn normalize_http_path(path: &str) -> String {
    let no_query = match path.split_once('?') {
        Some((head, _)) => head,
        None => path,
    };
    let lower = no_query.to_ascii_lowercase();
    let trimmed: String = if lower.len() > 1 && lower.ends_with('/') {
        lower.trim_end_matches('/').to_string()
    } else {
        lower
    };
    // Collapse all three param styles to {param} via a single regex pass.
    static PARAM_RE: OnceLock<Regex> = OnceLock::new();
    let re = PARAM_RE.get_or_init(|| {
        Regex::new(r":[A-Za-z_][A-Za-z0-9_]*|\{[^}]+\}|\[[^\]]+\]").unwrap()
    });
    re.replace_all(&trimmed, "{param}").into_owned()
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
            "proto" => out.extend(scan_proto(&rel, &text)),
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
            let method = caps[1].to_uppercase();
            let normalized = normalize_http_path(&caps[2]);
            out.push(ContractRow {
                contract_id: format!("http::{method}::{normalized}"),
                kind: "http".to_string(),
                role: "consumer".to_string(),
                method: Some(method),
                path: Some(normalized),
                topic: None,
                file: file.to_string(),
                line: (i + 1) as u32,
                language: language.to_string(),
                framework: "axios".to_string(),
            });
            continue;
        }
        if let Some(caps) = express_re().captures(line) {
            let method = caps[1].to_uppercase();
            let normalized = normalize_http_path(&caps[2]);
            out.push(ContractRow {
                contract_id: format!("http::{method}::{normalized}"),
                kind: "http".to_string(),
                role: "provider".to_string(),
                method: Some(method),
                path: Some(normalized),
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

fn proto_service_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"^\s*service\s+([A-Za-z_][A-Za-z0-9_]*)\s*\{?"#).unwrap())
}

fn proto_rpc_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"^\s*rpc\s+([A-Za-z_][A-Za-z0-9_]*)\s*\("#).unwrap())
}

fn scan_proto(file: &str, text: &str) -> Vec<ContractRow> {
    let mut out = Vec::new();
    let mut current_service: Option<String> = None;
    for (i, line) in text.lines().enumerate() {
        if let Some(caps) = proto_service_re().captures(line) {
            current_service = Some(caps[1].to_string());
            continue;
        }
        // Naive close-brace tracking — fine for the common one-service-per-file
        // case. For multi-service files the caller can split the proto.
        if line.trim() == "}" {
            current_service = None;
        }
        if let Some(svc) = current_service.as_deref() {
            if let Some(caps) = proto_rpc_re().captures(line) {
                let path = format!("{svc}/{}", &caps[1]);
                out.push(ContractRow {
                    contract_id: format!("grpc::{path}"),
                    kind: "grpc".to_string(),
                    role: "provider".to_string(),
                    method: None,
                    path: Some(path),
                    topic: None,
                    file: file.to_string(),
                    line: (i + 1) as u32,
                    language: "proto".to_string(),
                    framework: "grpc".to_string(),
                });
            }
        }
    }
    out
}

fn kafka_send_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Matches `<producer>.send('topic', ...)`. Distinguishing from HTTP
        // verbs is by the bare `.send(` shape with a string literal first
        // arg — typical of Kafka/NATS publisher idioms.
        Regex::new(
            r#"\b[A-Za-z_][A-Za-z0-9_]*\.send\(\s*['"]([A-Za-z_][\w./:\-]*)['"]"#,
        )
        .unwrap()
    })
}

fn scan_python(file: &str, text: &str) -> Vec<ContractRow> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if let Some(caps) = fastapi_re().captures(line) {
            let method = caps[1].to_uppercase();
            let normalized = normalize_http_path(&caps[2]);
            out.push(ContractRow {
                contract_id: format!("http::{method}::{normalized}"),
                kind: "http".to_string(),
                role: "provider".to_string(),
                method: Some(method),
                path: Some(normalized),
                topic: None,
                file: file.to_string(),
                line: (i + 1) as u32,
                language: "python".to_string(),
                framework: "fastapi".to_string(),
            });
            continue;
        }
        if let Some(caps) = kafka_send_re().captures(line) {
            let topic = caps[1].to_string();
            out.push(ContractRow {
                contract_id: format!("topic::{topic}"),
                kind: "topic".to_string(),
                role: "publisher".to_string(),
                method: None,
                path: None,
                topic: Some(topic),
                file: file.to_string(),
                line: (i + 1) as u32,
                language: "python".to_string(),
                framework: "kafka".to_string(),
            });
        }
    }
    out
}
