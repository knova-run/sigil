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
    // Strip scheme + host so that a consumer's `axios.get('http://api/users')`
    // collapses to `/users` and joins with a provider's `@app.get('/users')`.
    // Mirrors repowise's `_extract_path_from_url`.
    let stripped: &str = if let Some(rest) = path.strip_prefix("http://") {
        rest.split_once('/').map(|(_, p)| p).unwrap_or("")
    } else if let Some(rest) = path.strip_prefix("https://") {
        rest.split_once('/').map(|(_, p)| p).unwrap_or("")
    } else {
        path
    };
    // The strip above drops the leading slash; re-add it.
    let with_slash: String = if stripped.starts_with('/') {
        stripped.to_string()
    } else {
        format!("/{stripped}")
    };
    let no_query = match with_slash.split_once('?') {
        Some((head, _)) => head.to_string(),
        None => with_slash,
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

/// Translate Django path-converters (`<int:pk>`, `<str:slug>`,
/// `<uuid:id>`, `<path:rest>`, etc.) into the curly-brace form that
/// `normalize_http_path`'s `{param}` collapsing already understands.
fn django_path_to_braces(p: &str) -> String {
    // Replace `<converter:name>` and bare `<name>` with `{name}`.
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"<(?:[A-Za-z_]+:)?([A-Za-z_][A-Za-z0-9_]*)>").unwrap());
    re.replace_all(p, "{$1}").into_owned()
}

/// Walk `root` and return all contract rows discovered.
pub fn extract_from_root(root: &Path) -> Vec<ContractRow> {
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

/// A contract row tagged with its workspace member name. Emitted only
/// when `extract` is called against a workspace root. The `repo` field
/// matches what `resolve_workspace_contract_links` writes to
/// `.sigil-workspace/contracts.jsonl`.
#[derive(Debug, Serialize, PartialEq)]
pub struct WorkspaceContractRow {
    pub repo: String,
    #[serde(flatten)]
    pub row: ContractRow,
}

/// Workspace-aware variant. When `root` contains
/// `.sigil-workspace/members.json`, fan out across every enabled
/// member, tag each row with the member name, and return the union.
/// Otherwise emits exactly the same rows as `extract_from_root` (with
/// `repo` set to the root's basename so single-repo callers can still
/// pipe through the same downstream consumers).
pub fn extract_workspace_or_repo(root: &Path) -> Vec<WorkspaceContractRow> {
    let workspace_marker = root.join(".sigil-workspace").join("members.json");
    if workspace_marker.exists() {
        let members = crate::workspace::list(root).unwrap_or_default();
        let mut out = Vec::new();
        for m in members.into_iter().filter(|m| !m.disabled) {
            let mp = std::path::Path::new(&m.path);
            for row in extract_from_root(mp) {
                out.push(WorkspaceContractRow {
                    repo: m.name.clone(),
                    row,
                });
            }
        }
        return out;
    }
    let repo = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string());
    extract_from_root(root)
        .into_iter()
        .map(|row| WorkspaceContractRow {
            repo: repo.clone(),
            row,
        })
        .collect()
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
            "go" => out.extend(scan_go(&rel, &text)),
            "java" => out.extend(scan_java(&rel, &text)),
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
        ".git" | "node_modules" | "vendor" | "target" | "dist" | "build"
            | ".venv" | "venv" | "__pycache__" | ".sigil" | ".repowise-workspace"
            // QA pass surfaced contracts indexing `.yarn/releases/yarn-*.cjs`
            // on slate (TypeScript) — a 2.7 MB minified vendored binary
            // that hits Express-style route patterns by accident. Add the
            // common JS/TS vendored / cache directory names so contracts
            // doesn't mine artifacts.
            | ".yarn" | ".pnp" | ".next" | ".nuxt" | ".turbo" | ".cache"
            | "coverage" | ".coverage" | ".nyc_output" | "out" | ".output"
            | ".gradle" | ".idea" | ".vscode"
    )
}

#[cfg(test)]
mod skipped_dir_tests {
    use super::is_skipped_dir;
    use std::path::PathBuf;

    #[test]
    fn skips_yarn_releases_and_other_vendored_dirs() {
        for name in [
            ".yarn", ".pnp", ".next", ".turbo", ".cache",
            "coverage", "node_modules", ".sigil",
        ] {
            assert!(
                is_skipped_dir(&PathBuf::from(name)),
                "`{}` should be skipped",
                name
            );
        }
    }

    #[test]
    fn does_not_skip_normal_dirs() {
        for name in ["src", "lib", "packages", "tests"] {
            assert!(
                !is_skipped_dir(&PathBuf::from(name)),
                "`{}` should NOT be skipped",
                name
            );
        }
    }
}

fn fastapi_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Match `@app.get("path")` / `@router.post("path")` / `@api.put(...)`
        // (with an optional `_<suffix>` for `@app_router`, `@api_v2`, etc.).
        // Pre-PostHog this regex was `@<any-ident>.<verb>(...)` which
        // catastrophically matched `@mock.patch(...)` (91% of FastAPI
        // emissions on PostHog were `@mock.patch` decorators).
        Regex::new(
            r#"@(app|router|api|app_router|router_v\d+|api_v\d+|api_router|sub_app|sub_router)\.(get|post|put|delete|patch|options|head)\(\s*['"]([^'"]+)['"]"#,
        )
        .unwrap()
    })
}

fn express_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Match `app.get('/path', ...)` / `router.get(...)` / `r.get(...)`
        // and variants. Two narrowing rules vs the pre-PostHog version
        // (which matched `<any-ident>.<verb>('...')` and produced
        // ~93% false positives like `params.get('foo')`, `cache.get(...)`,
        // `redis.get(...)` etc.):
        //
        //   1. Receiver must be a recognised Express-style router/app
        //      identifier (incl. common suffixes like `Router`).
        //   2. Path must start with `/` — HTTP routes always do, but
        //      data-structure `.get('key')` calls don't.
        Regex::new(
            r#"\b(?:app|router|api|app_router|server|expressApp|router\d*|[A-Za-z_][A-Za-z0-9_]*Router)\.(get|post|put|delete|patch|options|head|all)\(\s*['"`](/[^'"`]*)['"`]"#,
        )
        .unwrap()
    })
}

fn django_path_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Django `path('foo/<int:pk>/', view)` and
        // `re_path(r'^foo/(?P<pk>\d+)/$', view)`. We capture the route
        // literal; the method binding is `*` because Django routes
        // don't carry a method (the view function decides).
        Regex::new(r#"\b(?:re_path|path)\(\s*r?['"]([^'"]+)['"]"#).unwrap()
    })
}

fn drf_router_register_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // DRF: `router.register(r'projects', ProjectViewSet, ...)` —
        // expands to 5 routes (list / retrieve / create / update /
        // partial_update / destroy). We emit one provider row per
        // canonical viewset action; cross-repo matching joins on the
        // base path, not the action suffix.
        Regex::new(r#"\brouter\.register\(\s*r?['"]([^'"]+)['"]"#).unwrap()
    })
}

fn drf_action_methods_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Capture the `methods=['GET', 'POST', ...]` list inside an
        // `@action(...)` call. Lookbehind for `@action` is too costly in
        // Rust regex (no fixed-width assertions for `\b@action\b`); we
        // gate on the literal `@action` prefix in the scan step.
        Regex::new(r#"methods\s*=\s*\[([^\]]+)\]"#).unwrap()
    })
}

fn drf_action_url_path_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"url_path\s*=\s*['"]([^'"]+)['"]"#).unwrap())
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

fn fetch_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Match fetch('/path') or fetch('/path', { method: 'POST', ... }).
        // We capture the URL string and optionally peek for a `method:` key
        // in the options object via a second pass below.
        Regex::new(r#"\bfetch\s*\(\s*['"`]([^'"`]+)['"`]"#).unwrap()
    })
}

fn fetch_method_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Same shape, with the options object peeked for method: 'POST'.
        Regex::new(r#"\bfetch\s*\(\s*['"`]([^'"`]+)['"`]\s*,\s*\{[^}]*method\s*:\s*['"]([A-Za-z]+)['"]"#).unwrap()
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
        // fetch consumer — defaults to GET unless an options object names
        // a different method. Probe fetch_method_re first to catch the
        // explicit method form, fall back to fetch_re for the bare form.
        let fetch_match = fetch_method_re()
            .captures(line)
            .map(|c| (c[2].to_uppercase(), c[1].to_string()))
            .or_else(|| {
                fetch_re()
                    .captures(line)
                    .map(|c| ("GET".to_string(), c[1].to_string()))
            });
        if let Some((method, raw)) = fetch_match {
            let normalized = normalize_http_path(&raw);
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
                framework: "fetch".to_string(),
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

fn go_route_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Covers four idioms in one pass:
        //   * net/http: `http.Handle("/p", ...)` / `http.HandleFunc("/p", ...)`
        //   * gin / echo: `r.GET("/p", ...)` (uppercase verb on a receiver)
        //   * chi: `r.Get("/p", ...)` (PascalCase verb)
        //   * gorilla-mux: `r.HandleFunc("/p", ...)`
        // `Handle` and `HandleFunc` carry no method — repowise emits `*`.
        Regex::new(
            r#"\.(GET|POST|PUT|DELETE|PATCH|OPTIONS|HEAD|Get|Post|Put|Delete|Patch|Options|Head|Handle|HandleFunc)\(\s*['"`]([^'"`]+)['"`]"#,
        )
        .unwrap()
    })
}

fn go_grpc_server_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `pb.RegisterAuthServiceServer(grpcServer, &impl{})` — service
        // name is the prefix before `Server`.
        Regex::new(r#"\.Register(\w+)Server\s*\("#).unwrap()
    })
}

fn go_grpc_client_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `pb.NewAuthServiceClient(conn)` — service name is the prefix
        // before `Client`.
        Regex::new(r#"\.New(\w+)Client\s*\("#).unwrap()
    })
}

fn scan_go(file: &str, text: &str) -> Vec<ContractRow> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        // gRPC server registration: `pb.RegisterFooServer(...)` →
        // provider of every method on `FooService`. We emit a single
        // service-level contract (no method) so it joins with a `.proto`
        // service if one exists in another repo.
        if let Some(caps) = go_grpc_server_re().captures(line) {
            let svc = caps[1].to_string();
            out.push(ContractRow {
                contract_id: format!("grpc::{svc}"),
                kind: "grpc".to_string(),
                role: "provider".to_string(),
                method: None,
                path: Some(svc.clone()),
                topic: None,
                file: file.to_string(),
                line: (i + 1) as u32,
                language: "go".to_string(),
                framework: "grpc".to_string(),
            });
            continue;
        }
        // gRPC client stub: `pb.NewFooClient(conn)` → consumer of every
        // method on `FooService`.
        if let Some(caps) = go_grpc_client_re().captures(line) {
            let svc = caps[1].to_string();
            out.push(ContractRow {
                contract_id: format!("grpc::{svc}"),
                kind: "grpc".to_string(),
                role: "consumer".to_string(),
                method: None,
                path: Some(svc.clone()),
                topic: None,
                file: file.to_string(),
                line: (i + 1) as u32,
                language: "go".to_string(),
                framework: "grpc".to_string(),
            });
            continue;
        }
        if let Some(caps) = go_route_re().captures(line) {
            let raw_verb = caps[1].to_string();
            // Repowise emits `*` for Handle / HandleFunc since they
            // don't bind a method.
            let method = if raw_verb == "Handle" || raw_verb == "HandleFunc" {
                "*".to_string()
            } else {
                raw_verb.to_uppercase()
            };
            let normalized = normalize_http_path(&caps[2]);
            // The framework label is best-effort; we can't distinguish
            // gin vs chi vs echo from a single line — call it `go` and
            // let downstream filter if they care.
            out.push(ContractRow {
                contract_id: format!("http::{method}::{normalized}"),
                kind: "http".to_string(),
                role: "provider".to_string(),
                method: Some(method),
                path: Some(normalized),
                topic: None,
                file: file.to_string(),
                line: (i + 1) as u32,
                language: "go".to_string(),
                framework: "go".to_string(),
            });
        }
    }
    out
}

fn spring_method_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // @GetMapping("/path") / @PostMapping(value = "/path") / etc.
        Regex::new(
            r#"@(Get|Post|Put|Delete|Patch)Mapping\s*\(\s*(?:value\s*=\s*)?['"]([^'"]+)['"]"#,
        )
        .unwrap()
    })
}

fn scan_java(file: &str, text: &str) -> Vec<ContractRow> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if let Some(caps) = spring_method_re().captures(line) {
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
                language: "java".to_string(),
                framework: "spring".to_string(),
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
    // Brace depth inside the current service body. `rpc` lines live at depth
    // 1; nested `message`, `oneof`, or `option { ... }` push deeper and must
    // close fully before we clear `current_service`.
    let mut depth: i32 = 0;
    for (i, line) in text.lines().enumerate() {
        if current_service.is_none() {
            if let Some(caps) = proto_service_re().captures(line) {
                current_service = Some(caps[1].to_string());
                let opens = line.matches('{').count() as i32;
                let closes = line.matches('}').count() as i32;
                depth = opens - closes;
                if depth <= 0 {
                    // Single-line `service Foo {}` — already closed.
                    if opens > 0 {
                        current_service = None;
                    }
                    depth = 0;
                }
                continue;
            }
        }
        if let Some(svc) = current_service.as_deref() {
            // Only emit rpc at top level of the service body to avoid
            // capturing rpc-shaped tokens inside nested option blocks.
            if depth == 1 {
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
            let opens = line.matches('{').count() as i32;
            let closes = line.matches('}').count() as i32;
            depth += opens - closes;
            if depth <= 0 {
                current_service = None;
                depth = 0;
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

fn requests_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"\b(?:requests|httpx)\.(get|post|put|delete|patch|options|head)\(\s*['"]([^'"]+)['"]"#,
        )
        .unwrap()
    })
}

fn scan_python(file: &str, text: &str) -> Vec<ContractRow> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if let Some(caps) = fastapi_re().captures(line) {
            // FastAPI: caps[1] = receiver (app|router|api|…), caps[2] = verb,
            // caps[3] = path.
            let method = caps[2].to_uppercase();
            let normalized = normalize_http_path(&caps[3]);
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
        // Django path() / re_path() — method is `*` since the view dispatch
        // decides which verbs to accept.
        if let Some(caps) = django_path_re().captures(line) {
            let raw_path = caps[1].to_string();
            // Strip Django's regex anchors and skip non-route lookups like
            // `path.join(...)` (which our `\b` already excludes via the
            // `path(` opener — but defend against odd matches anyway).
            if raw_path.is_empty() {
                continue;
            }
            // Django path patterns use `<int:pk>` / `<str:slug>` / `<uuid:id>`;
            // normalize them to `{param}` for cross-repo joining with the
            // consumer's `/api/.../{id}` literal.
            let pre_normalized = django_path_to_braces(&raw_path);
            let normalized = normalize_http_path(&pre_normalized);
            out.push(ContractRow {
                contract_id: format!("http::*::{normalized}"),
                kind: "http".to_string(),
                role: "provider".to_string(),
                method: Some("*".to_string()),
                path: Some(normalized),
                topic: None,
                file: file.to_string(),
                line: (i + 1) as u32,
                language: "python".to_string(),
                framework: "django".to_string(),
            });
            continue;
        }
        // DRF `router.register(r'foo', FooViewSet, ...)` — emit one base
        // contract; downstream consumers join on the path prefix.
        if let Some(caps) = drf_router_register_re().captures(line) {
            let raw = caps[1].to_string();
            // The DRF basename is registered without a leading slash; add
            // one so the contract_id joins cleanly with consumer paths.
            let with_slash = if raw.starts_with('/') {
                raw
            } else {
                format!("/{raw}")
            };
            let normalized = normalize_http_path(&with_slash);
            out.push(ContractRow {
                contract_id: format!("http::*::{normalized}"),
                kind: "http".to_string(),
                role: "provider".to_string(),
                method: Some("*".to_string()),
                path: Some(normalized),
                topic: None,
                file: file.to_string(),
                line: (i + 1) as u32,
                language: "python".to_string(),
                framework: "drf".to_string(),
            });
            continue;
        }
        // DRF `@action(methods=['GET'], url_path='custom')`. Match the
        // line starts-with `@action(` (cheap pre-filter), then pull
        // methods + url_path with two separate regex passes since Rust
        // regex doesn't tolerate the lookahead/non-greedy combination
        // needed for one all-in-one capture.
        let trimmed_line = line.trim_start();
        if trimmed_line.starts_with("@action(")
            && let Some(mcaps) = drf_action_methods_re().captures(line)
        {
            let methods_str = mcaps[1].to_string();
            let path = drf_action_url_path_re()
                .captures(line)
                .map(|c| c[1].to_string())
                .unwrap_or_else(|| "*".to_string());
            let methods_str = methods_str.as_str();
            let normalized = if path == "*" {
                "*".to_string()
            } else {
                let with_slash = if path.starts_with('/') {
                    path
                } else {
                    format!("/{path}")
                };
                normalize_http_path(&with_slash)
            };
            // Methods are like `'GET', 'POST'` — split and emit one row each.
            for tok in methods_str.split(',') {
                let m = tok.trim().trim_matches(|c| c == '\'' || c == '"');
                if m.is_empty() {
                    continue;
                }
                let method = m.to_uppercase();
                out.push(ContractRow {
                    contract_id: format!("http::{method}::{normalized}"),
                    kind: "http".to_string(),
                    role: "provider".to_string(),
                    method: Some(method),
                    path: Some(normalized.clone()),
                    topic: None,
                    file: file.to_string(),
                    line: (i + 1) as u32,
                    language: "python".to_string(),
                    framework: "drf".to_string(),
                });
            }
            continue;
        }
        if let Some(caps) = requests_re().captures(line) {
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
                language: "python".to_string(),
                framework: "requests".to_string(),
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
