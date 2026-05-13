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
    // Collapse all four param styles to {param} via a single regex pass:
    //   * `:slug`            — Rails / Express / DRF (`:id`)
    //   * `{slug}`           — FastAPI / Spring / OpenAPI
    //   * `[slug]`           — Next.js dynamic segments
    //   * `${slug}` / `$slug` — JS template-literal interpolation
    //                          (e.g. `requests.get(`/articles/${slug}`)`).
    // The template-literal form is what cross-repo frontend↔backend
    // matching needed to land: a Conduit/RealWorld frontend writes
    // `/articles/${slug}` and the Django/DRF backend's `<slug>` converter
    // resolves to `{slug}` — both must collapse to `{param}` to join.
    static PARAM_RE: OnceLock<Regex> = OnceLock::new();
    let re = PARAM_RE.get_or_init(|| {
        Regex::new(r"\$\{[^}]+\}|:[A-Za-z_][A-Za-z0-9_]*|\{[^}]+\}|\[[^\]]+\]").unwrap()
    });
    re.replace_all(&trimmed, "{param}").into_owned()
}

/// Translate Django route patterns into a canonical form that joins
/// with consumer paths. Handles three input shapes:
///
///   * Django 2.0+ `path('foo/<int:pk>/', view)` — converters like
///     `<int:pk>`, `<str:slug>`, `<uuid:id>`, `<path:rest>`.
///   * Django 1.x `url(r'^foo/(?P<pk>\d+)/$', view)` — Python regex
///     with `^`/`$` anchors, character classes, and `(?P<name>…)`
///     named-capture groups.
///   * Mixed (the legacy `url(r'^foo/<int:pk>/$', view)` form).
///
/// The output is suitable for `normalize_http_path` to collapse to
/// `{param}` consistently.
fn django_path_to_braces(p: &str) -> String {
    // 1. Strip regex anchors and trailing whitespace markers.
    let mut s = p.to_string();
    if let Some(rest) = s.strip_prefix('^') {
        s = rest.to_string();
    }
    if let Some(rest) = s.strip_suffix('$') {
        s = rest.to_string();
    }
    // 2. Named-capture groups `(?P<pk>\d+)` / `(?P<slug>[-\w]+)` → `{pk}`.
    static NAMED_RE: OnceLock<Regex> = OnceLock::new();
    let named = NAMED_RE.get_or_init(|| {
        Regex::new(r"\(\?P<([A-Za-z_][A-Za-z0-9_]*)>[^)]*\)").unwrap()
    });
    s = named.replace_all(&s, "{$1}").into_owned();
    // 3. Unnamed capture groups `(\d+)` / `([^/]+)` → `{param}`.
    static UNNAMED_RE: OnceLock<Regex> = OnceLock::new();
    let unnamed = UNNAMED_RE.get_or_init(|| Regex::new(r"\([^)]*\)").unwrap());
    s = unnamed.replace_all(&s, "{param}").into_owned();
    // 4. Django converter syntax `<int:pk>` / `<slug>` → `{pk}` / `{slug}`.
    static CONV_RE: OnceLock<Regex> = OnceLock::new();
    let conv = CONV_RE.get_or_init(|| {
        Regex::new(r"<(?:[A-Za-z_]+:)?([A-Za-z_][A-Za-z0-9_]*)>").unwrap()
    });
    s = conv.replace_all(&s, "{$1}").into_owned();
    // 5. Strip trailing `?` (optional-trailing-slash regex idiom: `/?$`
    //    becomes `/?` after step 2; collapse to no trailing slash).
    if s.ends_with("/?") {
        s.truncate(s.len() - 2);
        s.push('/');
    }
    s
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
            "java" | "kt" | "kts" => out.extend(scan_java(&rel, &text)),
            "rb" => out.extend(scan_ruby(&rel, &text)),
            "rs" => out.extend(scan_rust(&rel, &text)),
            "php" => out.extend(scan_php(&rel, &text)),
            "cs" => out.extend(scan_csharp(&rel, &text)),
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
        // Match `<receiver>.<verb>('/path', …)`. Receiver is captured so
        // the scanner can decide PROVIDER (Express-style `router.get`)
        // vs CONSUMER (api-wrapper `api.get`).
        //
        // Path-must-start-with-`/` guard eliminates the data-structure
        // false positives that the pre-PostHog regex produced
        // (`params.get('ordering')`, `cache.get(...)`, `Map.get(...)`).
        Regex::new(
            r#"\b([A-Za-z_][A-Za-z0-9_]*)\.(get|post|put|delete|patch|options|head|all)\(\s*['"`](/[^'"`]*)['"`]"#,
        )
        .unwrap()
    })
}

/// True when `receiver` looks like a server-side HTTP router/app.
/// Server-side receivers map `.<verb>('/path')` to a route DEFINITION;
/// every other receiver is treated as a CONSUMER call against an HTTP
/// API wrapper (`api.get`, `requests.get`, `apiClient.post`, etc.).
fn is_server_side_receiver(receiver: &str) -> bool {
    matches!(
        receiver,
        "app" | "router" | "server" | "expressApp" | "r"
    ) || receiver.ends_with("Router")
        || receiver.ends_with("App")
        || receiver.ends_with("Server")
        || (receiver.starts_with("router") && receiver[6..].chars().all(|c| c.is_ascii_digit()))
}

fn django_path_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Django route declarations:
        //   * `path('foo/<int:pk>/', view)` (Django 2.0+)
        //   * `re_path(r'^foo/(?P<pk>\d+)/$', view)` (regex form)
        //   * `url(r'^foo/$', view)` (legacy pre-2.0 — still widespread
        //     in older Django apps including the RealWorld backend).
        // We capture the route literal; the method binding is `*`
        // because Django routes don't carry a method (the view function
        // / class-based view's allowed methods set decide).
        Regex::new(r#"\b(?:re_path|path|url)\(\s*r?['"]([^'"]+)['"]"#).unwrap()
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

fn superagent_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `superagent.get(url)` / `superagent.post(url)` / `superagent.del(url)`.
        // Also matches the chained-style `superagent.get(`${ROOT}${url}`)`.
        // `del` is superagent's alias for DELETE.
        Regex::new(
            r#"\bsuperagent\.(get|post|put|patch|del|delete|head|options)\(\s*[`'"]"#,
        )
        .unwrap()
    })
}

fn superagent_url_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Extract the URL from `superagent.<verb>(arg)`. We accept the
        // template-literal form (`${API_ROOT}${url}`) and fall back to a
        // plain string literal. Template literals can't be resolved
        // statically, so we capture whatever follows `${API_ROOT}` or
        // the literal-only form.
        Regex::new(
            r#"\bsuperagent\.(?:get|post|put|patch|del|delete|head|options)\(\s*[`'"](?:\$\{[^}]+\})?([^`'"]+)[`'"]"#,
        )
        .unwrap()
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
        // superagent consumer — Conduit / RealWorld React frontend uses
        // this (instead of axios/fetch). Same verb-method shape, plus a
        // `del` alias for DELETE.
        if let Some(verb_caps) = superagent_re().captures(line)
            && let Some(url_caps) = superagent_url_re().captures(line)
        {
            let raw_verb = verb_caps[1].to_string();
            let method = if raw_verb == "del" {
                "DELETE".to_string()
            } else {
                raw_verb.to_uppercase()
            };
            let normalized = normalize_http_path(&url_caps[1]);
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
                framework: "superagent".to_string(),
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
            let receiver = caps[1].to_string();
            let method = caps[2].to_uppercase();
            let normalized = normalize_http_path(&caps[3]);
            // Server-side receivers (`app.get`, `router.get`, `*Router.get`)
            // → provider; everything else (`api.get`, `requests.get`,
            // `apiClient.post`) → consumer of an HTTP API wrapper.
            let (role, framework) = if is_server_side_receiver(&receiver) {
                ("provider", "express")
            } else {
                ("consumer", "api-wrapper")
            };
            out.push(ContractRow {
                contract_id: format!("http::{method}::{normalized}"),
                kind: "http".to_string(),
                role: role.to_string(),
                method: Some(method),
                path: Some(normalized),
                topic: None,
                file: file.to_string(),
                line: (i + 1) as u32,
                language: language.to_string(),
                framework: framework.to_string(),
            });
        }
    }
    // @grpc/grpc-js — Node gRPC server + client patterns.
    //   server: `server.addService(usersProto.UserService.service, handlers)`
    //   client: `new usersProto.UserService(addr, credentials)` (no `Client`
    //           suffix — the generated stub IS the named class).
    static GRPC_JS_SERVER: OnceLock<Regex> = OnceLock::new();
    let grpc_js_server = GRPC_JS_SERVER.get_or_init(|| {
        Regex::new(r#"\.addService\s*\(\s*[A-Za-z_][A-Za-z0-9_\.]*\.([A-Z][A-Za-z0-9_]*)\.service"#).unwrap()
    });
    static GRPC_JS_CLIENT: OnceLock<Regex> = OnceLock::new();
    let grpc_js_client = GRPC_JS_CLIENT.get_or_init(|| {
        Regex::new(r#"\bnew\s+[A-Za-z_][A-Za-z0-9_\.]*\.([A-Z][A-Za-z0-9_]*)\s*\([^)]*credentials"#).unwrap()
    });
    let file_uses_grpc = text.contains("@grpc/grpc-js") || text.contains("@grpc/proto-loader") || text.contains("require('grpc')") || text.contains("from 'grpc'");
    if file_uses_grpc {
        for (i, line) in text.lines().enumerate() {
            if let Some(caps) = grpc_js_server.captures(line) {
                let svc = caps[1].to_string();
                out.push(ContractRow {
                    contract_id: format!("grpc::{svc}"),
                    kind: "grpc".to_string(), role: "provider".to_string(),
                    method: None, path: Some(svc), topic: None,
                    file: file.to_string(), line: (i + 1) as u32,
                    language: language.to_string(), framework: "grpc".to_string(),
                });
            }
            if let Some(caps) = grpc_js_client.captures(line) {
                let svc = caps[1].to_string();
                out.push(ContractRow {
                    contract_id: format!("grpc::{svc}"),
                    kind: "grpc".to_string(), role: "consumer".to_string(),
                    method: None, path: Some(svc), topic: None,
                    file: file.to_string(), line: (i + 1) as u32,
                    language: language.to_string(), framework: "grpc".to_string(),
                });
            }
        }
    }
    // Redis / NATS pub-sub for JS/TS files.
    emit_pubsub_rows(file, text, language, &mut out);
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

/// Gate Go gRPC client/server detection on the file importing a gRPC
/// package. Without this, `.New<X>Client(` matches non-gRPC factories
/// like `redis.NewFailoverClient(...)`, `redis.NewClusterClient(...)`,
/// `kafka.NewClient(...)`, etc. — emitting tens of false `grpc::Foo`
/// rows per Go library that has a polymorphic constructor.
fn go_file_uses_grpc(text: &str) -> bool {
    text.contains("google.golang.org/grpc")
        || text.contains("\"grpc\"")
        || text.contains("grpc.Dial")
        || text.contains("grpc.NewServer")
        || text.contains("grpc.RegisterService")
}

fn redis_streams_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Redis Streams ops (BullMQ, Sidekiq Pro, custom event buses):
        //   * `client.xadd("stream-key", ...)` — publisher
        //   * `client.xread(...)`, `client.xreadgroup(...)` — subscriber
        //   * `client.XAdd(ctx, ...)`, `client.XRead(...)` — Go variants
        // Capture the first quoted argument that looks like a stream key.
        Regex::new(
            r#"\b[A-Za-z_][A-Za-z0-9_]*\.(?:xadd|xreadgroup|xread|XAdd|XReadGroup|XRead)\s*\([^)"]*?['"]([A-Za-z0-9_][\w./:\-]*)['"]"#,
        )
        .unwrap()
    })
}

fn scan_go(file: &str, text: &str) -> Vec<ContractRow> {
    let mut out = Vec::new();
    // Go pub-sub (go-redis Publish/Subscribe + nats.go Publish/Subscribe).
    for (i, line) in text.lines().enumerate() {
        if let Some(caps) = redis_go_publish_re().captures(line) {
            let topic = caps[1].to_string();
            let framework = if topic.contains('.') { "nats" } else { "redis" };
            out.push(ContractRow {
                contract_id: format!("topic::{topic}"),
                kind: "topic".to_string(),
                role: "publisher".to_string(),
                method: None,
                path: None,
                topic: Some(topic),
                file: file.to_string(),
                line: (i + 1) as u32,
                language: "go".to_string(),
                framework: framework.to_string(),
            });
            continue;
        }
        if redis_go_subscribe_re().is_match(line) {
            // Capture every quoted topic after the ctx argument.
            let args_start = line.find(".Subscribe(").map(|p| p + ".Subscribe(".len()).unwrap_or(0);
            let after = &line[args_start..];
            for cap in quoted_topic_args_re().captures_iter(after) {
                let topic = cap[1].to_string();
                let framework = if topic.contains('.') { "nats" } else { "redis" };
                out.push(ContractRow {
                    contract_id: format!("topic::{topic}"),
                    kind: "topic".to_string(),
                    role: "subscriber".to_string(),
                    method: None,
                    path: None,
                    topic: Some(topic),
                    file: file.to_string(),
                    line: (i + 1) as u32,
                    language: "go".to_string(),
                    framework: framework.to_string(),
                });
            }
            continue;
        }
    }
    for (i, line) in text.lines().enumerate() {
        // gRPC server registration: `pb.RegisterFooServer(...)` →
        // provider of every method on `FooService`. We emit a single
        // service-level contract (no method) so it joins with a `.proto`
        // service if one exists in another repo. Gated on a file-level
        // gRPC import probe — without it, `redis.NewFailoverClient(...)`
        // and `kafka.NewProducerClient(...)` patterns get tagged as
        // gRPC bindings (real bug seen on the go-redis library, 58
        // false `grpc::Failover|Cluster|Universal|...` rows).
        if !go_file_uses_grpc(text) {
            // Skip the gRPC branch but still attempt the HTTP route
            // branch below — Go files that don't use gRPC may still
            // define HTTP routes.
        } else if let Some(caps) = go_grpc_server_re().captures(line) {
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
        // method on `FooService`. Same gate as the server branch.
        if go_file_uses_grpc(text)
            && let Some(caps) = go_grpc_client_re().captures(line)
        {
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

fn java_grpc_impl_base_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `extends FooServiceGrpc.FooServiceImplBase` — gRPC provider.
        Regex::new(r#"extends\s+([A-Za-z_][A-Za-z0-9_]*)Grpc\."#).unwrap()
    })
}

fn java_grpc_stub_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `FooServiceGrpc.newBlockingStub(channel)` / `newFutureStub` /
        // `newStub`. Consumer side.
        Regex::new(r#"\b([A-Za-z_][A-Za-z0-9_]*)Grpc\.new(?:Blocking|Future)?Stub\s*\("#).unwrap()
    })
}

fn scan_java(file: &str, text: &str) -> Vec<ContractRow> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        // Java gRPC server: `class X extends FooGrpc.FooImplBase`
        if let Some(caps) = java_grpc_impl_base_re().captures(line) {
            let svc = caps[1].to_string();
            out.push(ContractRow {
                contract_id: format!("grpc::{svc}"),
                kind: "grpc".to_string(),
                role: "provider".to_string(),
                method: None,
                path: Some(svc),
                topic: None,
                file: file.to_string(),
                line: (i + 1) as u32,
                language: "java".to_string(),
                framework: "grpc".to_string(),
            });
            continue;
        }
        // Java gRPC client: `FooGrpc.newBlockingStub(channel)`
        if let Some(caps) = java_grpc_stub_re().captures(line) {
            let svc = caps[1].to_string();
            out.push(ContractRow {
                contract_id: format!("grpc::{svc}"),
                kind: "grpc".to_string(),
                role: "consumer".to_string(),
                method: None,
                path: Some(svc),
                topic: None,
                file: file.to_string(),
                line: (i + 1) as u32,
                language: "java".to_string(),
                framework: "grpc".to_string(),
            });
            continue;
        }
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
    // Java Jedis pub/sub: `jedis.publish("ch", msg)` /
    // `jedis.subscribe(listener, "ch1", "ch2")`. Same shape as the
    // Python/JS lowercase variants — reuse those regexes.
    for (i, line) in text.lines().enumerate() {
        if let Some(caps) = redis_publish_re().captures(line) {
            let topic = caps[1].to_string();
            let framework = if topic.contains('.') { "nats" } else { "redis" };
            out.push(ContractRow {
                contract_id: format!("topic::{topic}"),
                kind: "topic".to_string(), role: "publisher".to_string(),
                method: None, path: None, topic: Some(topic),
                file: file.to_string(), line: (i + 1) as u32,
                language: "java".to_string(), framework: framework.to_string(),
            });
            continue;
        }
        if redis_subscribe_re().is_match(line) {
            for topic in extract_subscribe_topics(line) {
                let framework = if topic.contains('.') { "nats" } else { "redis" };
                out.push(ContractRow {
                    contract_id: format!("topic::{topic}"),
                    kind: "topic".to_string(), role: "subscriber".to_string(),
                    method: None, path: None, topic: Some(topic),
                    file: file.to_string(), line: (i + 1) as u32,
                    language: "java".to_string(), framework: framework.to_string(),
                });
            }
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

// ─── Ruby / Rails ────────────────────────────────────────────────────────────

fn rails_route_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `get '/users'` / `post '/users', to: 'users#create'` etc. Matches
        // `match` too (Rails' explicit multi-verb form). Lowercase verb
        // followed by a string literal (single or double quoted).
        Regex::new(
            r#"(?m)^\s*(get|post|put|patch|delete|match|head|options)\s+['"]([^'"]+)['"]"#,
        )
        .unwrap()
    })
}

fn rails_resources_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `resources :users` / `resource :session` — RESTful expansion. We
        // emit one base contract per declaration; downstream consumers
        // join on the path prefix.
        Regex::new(r#"^\s*(?:resources|resource)\s+:([A-Za-z_][A-Za-z0-9_]*)"#).unwrap()
    })
}

fn ruby_grpc_service_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `class Foo < Users::UserService::Service` — gRPC server impl.
        // Captures the trailing service name before `::Service`.
        Regex::new(r#"class\s+\w+\s*<\s*(?:[A-Za-z_][A-Za-z0-9_]*::)*([A-Z][A-Za-z0-9_]*)::Service\b"#).unwrap()
    })
}

fn ruby_grpc_stub_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `Users::UserService::Stub.new(...)` — gRPC client stub.
        Regex::new(r#"\b(?:[A-Za-z_][A-Za-z0-9_]*::)*([A-Z][A-Za-z0-9_]*)::Stub\.new"#).unwrap()
    })
}

fn scan_ruby(file: &str, text: &str) -> Vec<ContractRow> {
    let mut out = Vec::new();
    // Ruby gRPC + Redis pub-sub (scanned on every .rb file, independent
    // of routes.rb gate below).
    let file_uses_grpc = text.contains("require 'grpc'") || text.contains("require \"grpc\"")
        || text.contains("_services_pb") || text.contains("GRPC::");
    if file_uses_grpc {
        for (i, line) in text.lines().enumerate() {
            if let Some(caps) = ruby_grpc_service_re().captures(line) {
                let svc = caps[1].to_string();
                if svc.len() >= 2 {
                    out.push(ContractRow {
                        contract_id: format!("grpc::{svc}"),
                        kind: "grpc".to_string(), role: "provider".to_string(),
                        method: None, path: Some(svc), topic: None,
                        file: file.to_string(), line: (i + 1) as u32,
                        language: "ruby".to_string(), framework: "grpc".to_string(),
                    });
                }
            }
            if let Some(caps) = ruby_grpc_stub_re().captures(line) {
                let svc = caps[1].to_string();
                if svc.len() >= 2 {
                    out.push(ContractRow {
                        contract_id: format!("grpc::{svc}"),
                        kind: "grpc".to_string(), role: "consumer".to_string(),
                        method: None, path: Some(svc), topic: None,
                        file: file.to_string(), line: (i + 1) as u32,
                        language: "ruby".to_string(), framework: "grpc".to_string(),
                    });
                }
            }
        }
    }
    emit_pubsub_rows(file, text, "ruby", &mut out);
    // Quick filter: this file has to plausibly be a Rails routes file
    // or a controller — skip pure model / lib / spec files to avoid
    // matching `get :latest, on: :collection` in non-router contexts.
    // Routes typically live under `config/routes`, but Engine-style
    // routes can be anywhere. Use a content sniff: `Rails.application
    // .routes.draw` or `routes.draw` opens a routes block; or any line
    // beginning with a verb + literal path.
    let looks_like_routes = file.contains("config/routes")
        || file.ends_with("routes.rb")
        || text.contains("routes.draw")
        || text.contains("Rails.application.routes");
    if !looks_like_routes {
        return out;
    }
    for line in text.lines() {
        // Skip comments.
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
    }
    // Regex passes — use the line index from enumerate.
    for (i, line) in text.lines().enumerate() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        if let Some(caps) = rails_route_re().captures(line) {
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
                language: "ruby".to_string(),
                framework: "rails".to_string(),
            });
            continue;
        }
        if let Some(caps) = rails_resources_re().captures(line) {
            let name = caps[1].to_string();
            let normalized = normalize_http_path(&format!("/{name}"));
            out.push(ContractRow {
                contract_id: format!("http::*::{normalized}"),
                kind: "http".to_string(),
                role: "provider".to_string(),
                method: Some("*".to_string()),
                path: Some(normalized),
                topic: None,
                file: file.to_string(),
                line: (i + 1) as u32,
                language: "ruby".to_string(),
                framework: "rails".to_string(),
            });
        }
    }
    out
}

// ─── Rust web frameworks ─────────────────────────────────────────────────────

fn rust_axum_route_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // axum: `.route("/users", get(handler))` — verb is the inner
        // function name (`get`/`post`/`put`/`delete`/`patch`/`head`/
        // `options`). Captures path then verb.
        Regex::new(
            r#"\.route\(\s*"([^"]+)"\s*,\s*(get|post|put|delete|patch|head|options)\s*\("#,
        )
        .unwrap()
    })
}

fn rust_rocket_macro_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Rocket: `#[get("/path")]` / `#[post("/path", data = "<body>")]`.
        Regex::new(
            r#"#\[(get|post|put|delete|patch|head|options)\(\s*"([^"]+)""#,
        )
        .unwrap()
    })
}

fn rust_actix_route_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // actix-web: `web::get().to(handler)` chained off a resource, e.g.
        // `.service(web::resource("/users").route(web::get().to(list_users)))`.
        // Captures path from the resource, verb from web::<verb>().
        Regex::new(
            r#"web::resource\(\s*"([^"]+)"\s*\)[^;]*?web::(get|post|put|delete|patch|head|options)\s*\("#,
        )
        .unwrap()
    })
}

fn rust_reqwest_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // reqwest: `client.get("/url")` / `client.post(...)` etc.
        // (Receiver is `client`-shaped; require start with `/` or http://.)
        Regex::new(
            r#"\bclient\.(get|post|put|delete|patch|head)\s*\(\s*"((?:https?://[^"]+)|/[^"]*)""#,
        )
        .unwrap()
    })
}

fn rust_tonic_server_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // tonic provider: `impl users::user_service_server::UserService for X`
        // or `add_service(UserServiceServer::new(...))`. Service name comes
        // from the `_service_server::<Name>` module path or the
        // `<Name>Server::new(` constructor.
        Regex::new(r#"\b([A-Z][A-Za-z0-9_]*)(?:Server::new|_server::[a-z_]+)"#).unwrap()
    })
}

fn rust_tonic_client_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // tonic consumer: `UserServiceClient::connect(...)` or the
        // `_client::<Name>` module path.
        Regex::new(r#"\b([A-Z][A-Za-z0-9_]*)Client::connect"#).unwrap()
    })
}

fn rust_redis_publish_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // redis-rs: `con.publish("channel", msg)`. Method is lowercase
        // in Rust (matches the redis::Commands trait).
        Regex::new(r#"\b[A-Za-z_][A-Za-z0-9_]*\.publish[\s:<>(\),A-Za-z0-9_]*\(\s*"([A-Za-z0-9_][\w./:\-]*)""#).unwrap()
    })
}

fn rust_redis_subscribe_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // redis-rs: `pubsub.subscribe("ch")` / `pubsub.subscribe(&["ch1", "ch2"])`.
        // We pre-match the call shape then extract every quoted topic.
        Regex::new(r#"\b[A-Za-z_][A-Za-z0-9_]*\.subscribe\s*\("#).unwrap()
    })
}

fn rust_nats_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // async-nats: `client.publish("subject", payload).await` and
        // `client.subscribe("subject").await`. The `.await` suffix is
        // optional in our match — we capture the call.
        Regex::new(r#"\b[A-Za-z_][A-Za-z0-9_]*\.(publish|subscribe)\s*\(\s*"([A-Za-z0-9_][\w./:>*\-]*)""#).unwrap()
    })
}

fn scan_rust(file: &str, text: &str) -> Vec<ContractRow> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        // axum .route("/p", get(...))
        if let Some(caps) = rust_axum_route_re().captures(line) {
            let path = caps[1].to_string();
            let method = caps[2].to_uppercase();
            let normalized = normalize_http_path(&path);
            out.push(ContractRow {
                contract_id: format!("http::{method}::{normalized}"),
                kind: "http".to_string(),
                role: "provider".to_string(),
                method: Some(method),
                path: Some(normalized),
                topic: None,
                file: file.to_string(),
                line: (i + 1) as u32,
                language: "rust".to_string(),
                framework: "axum".to_string(),
            });
            continue;
        }
        // Rocket #[get("/p")]
        if let Some(caps) = rust_rocket_macro_re().captures(line) {
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
                language: "rust".to_string(),
                framework: "rocket".to_string(),
            });
            continue;
        }
        // actix-web resource + web::<verb>()
        if let Some(caps) = rust_actix_route_re().captures(line) {
            let path = caps[1].to_string();
            let method = caps[2].to_uppercase();
            let normalized = normalize_http_path(&path);
            out.push(ContractRow {
                contract_id: format!("http::{method}::{normalized}"),
                kind: "http".to_string(),
                role: "provider".to_string(),
                method: Some(method),
                path: Some(normalized),
                topic: None,
                file: file.to_string(),
                line: (i + 1) as u32,
                language: "rust".to_string(),
                framework: "actix".to_string(),
            });
            continue;
        }
        // tonic gRPC server
        if let Some(caps) = rust_tonic_server_re().captures(line) {
            let svc = caps[1].to_string();
            // Skip generic short matches like `Server` from `Server::builder`
            if svc != "Server" && svc.len() >= 3 {
                out.push(ContractRow {
                    contract_id: format!("grpc::{svc}"),
                    kind: "grpc".to_string(), role: "provider".to_string(),
                    method: None, path: Some(svc), topic: None,
                    file: file.to_string(), line: (i + 1) as u32,
                    language: "rust".to_string(), framework: "tonic".to_string(),
                });
                continue;
            }
        }
        // tonic gRPC client
        if let Some(caps) = rust_tonic_client_re().captures(line) {
            let svc = caps[1].to_string();
            if svc.len() >= 3 {
                out.push(ContractRow {
                    contract_id: format!("grpc::{svc}"),
                    kind: "grpc".to_string(), role: "consumer".to_string(),
                    method: None, path: Some(svc), topic: None,
                    file: file.to_string(), line: (i + 1) as u32,
                    language: "rust".to_string(), framework: "tonic".to_string(),
                });
                continue;
            }
        }
        // Rust redis-rs publish
        if let Some(caps) = rust_redis_publish_re().captures(line) {
            let topic = caps[1].to_string();
            let framework = if topic.contains('.') { "nats" } else { "redis" };
            out.push(ContractRow {
                contract_id: format!("topic::{topic}"),
                kind: "topic".to_string(), role: "publisher".to_string(),
                method: None, path: None, topic: Some(topic),
                file: file.to_string(), line: (i + 1) as u32,
                language: "rust".to_string(), framework: framework.to_string(),
            });
            continue;
        }
        if rust_redis_subscribe_re().is_match(line) {
            for topic in extract_subscribe_topics(line) {
                let framework = if topic.contains('.') { "nats" } else { "redis" };
                out.push(ContractRow {
                    contract_id: format!("topic::{topic}"),
                    kind: "topic".to_string(), role: "subscriber".to_string(),
                    method: None, path: None, topic: Some(topic),
                    file: file.to_string(), line: (i + 1) as u32,
                    language: "rust".to_string(), framework: framework.to_string(),
                });
            }
            continue;
        }
        // async-nats — same shape, dotted subjects → nats framework
        if let Some(caps) = rust_nats_re().captures(line) {
            let verb = &caps[1];
            let topic = caps[2].to_string();
            let role = if verb == "publish" { "publisher" } else { "subscriber" };
            let framework = if topic.contains('.') { "nats" } else { "redis" };
            out.push(ContractRow {
                contract_id: format!("topic::{topic}"),
                kind: "topic".to_string(), role: role.to_string(),
                method: None, path: None, topic: Some(topic),
                file: file.to_string(), line: (i + 1) as u32,
                language: "rust".to_string(), framework: framework.to_string(),
            });
            continue;
        }
        // reqwest consumer
        if let Some(caps) = rust_reqwest_re().captures(line) {
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
                language: "rust".to_string(),
                framework: "reqwest".to_string(),
            });
        }
    }
    out
}

// ─── PHP / Laravel ───────────────────────────────────────────────────────────

fn laravel_route_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `Route::get('/users', ...)` etc. Match the verb in the static
        // call. `any` and `match` are also valid Laravel verbs — emit
        // `*` for those.
        Regex::new(
            r#"\bRoute::(get|post|put|patch|delete|options|head|any|match)\(\s*['"]([^'"]+)['"]"#,
        )
        .unwrap()
    })
}

fn laravel_resource_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `Route::resource('users', UserController::class)` — RESTful.
        Regex::new(r#"\bRoute::(?:resource|apiResource)\(\s*['"]([^'"]+)['"]"#).unwrap()
    })
}

fn scan_php(file: &str, text: &str) -> Vec<ContractRow> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if let Some(caps) = laravel_route_re().captures(line) {
            let raw_verb = caps[1].to_string();
            let method = if raw_verb == "any" || raw_verb == "match" {
                "*".to_string()
            } else {
                raw_verb.to_uppercase()
            };
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
                language: "php".to_string(),
                framework: "laravel".to_string(),
            });
            continue;
        }
        if let Some(caps) = laravel_resource_re().captures(line) {
            let raw = caps[1].to_string();
            let with_slash = if raw.starts_with('/') { raw } else { format!("/{raw}") };
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
                language: "php".to_string(),
                framework: "laravel".to_string(),
            });
        }
    }
    // PHP redis pub/sub. Predis uses `$client->publish('ch', msg)` and
    // `$pubsub->subscribe('ch')` (and the `->` operator instead of `.`).
    // Reuse the regexes by handling `->` separately.
    static PHP_PUB_RE: OnceLock<Regex> = OnceLock::new();
    let php_pub = PHP_PUB_RE.get_or_init(|| {
        Regex::new(r#"\$[A-Za-z_][A-Za-z0-9_]*->publish\s*\(\s*['"]([A-Za-z0-9_][\w./:\-]*)['"]"#).unwrap()
    });
    static PHP_SUB_RE: OnceLock<Regex> = OnceLock::new();
    let php_sub = PHP_SUB_RE.get_or_init(|| {
        Regex::new(r#"\$[A-Za-z_][A-Za-z0-9_]*->subscribe\s*\("#).unwrap()
    });
    for (i, line) in text.lines().enumerate() {
        if let Some(caps) = php_pub.captures(line) {
            let topic = caps[1].to_string();
            let framework = if topic.contains('.') { "nats" } else { "redis" };
            out.push(ContractRow {
                contract_id: format!("topic::{topic}"),
                kind: "topic".to_string(), role: "publisher".to_string(),
                method: None, path: None, topic: Some(topic),
                file: file.to_string(), line: (i + 1) as u32,
                language: "php".to_string(), framework: framework.to_string(),
            });
        }
        if php_sub.is_match(line) {
            // Replace `->subscribe` with `.subscribe` so the shared
            // arg-extractor picks up the topic args.
            let normalised = line.replace("->subscribe", ".subscribe");
            for topic in extract_subscribe_topics(&normalised) {
                let framework = if topic.contains('.') { "nats" } else { "redis" };
                out.push(ContractRow {
                    contract_id: format!("topic::{topic}"),
                    kind: "topic".to_string(), role: "subscriber".to_string(),
                    method: None, path: None, topic: Some(topic),
                    file: file.to_string(), line: (i + 1) as u32,
                    language: "php".to_string(), framework: framework.to_string(),
                });
            }
        }
    }
    out
}

// ─── C# / ASP.NET ────────────────────────────────────────────────────────────

fn aspnet_attribute_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `[HttpGet("/path")]` / `[HttpPost("path")]` etc. The path is
        // optional; bare `[HttpPost]` matches with no path captured (we
        // emit `*`).
        Regex::new(r#"\[Http(Get|Post|Put|Delete|Patch|Options|Head)(?:\s*\(\s*"([^"]+)")?"#).unwrap()
    })
}

fn aspnet_minimal_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Minimal API: `app.MapGet("/health", ...)`.
        Regex::new(r#"\.Map(Get|Post|Put|Delete|Patch|Options|Head)\(\s*"([^"]+)""#).unwrap()
    })
}

fn aspnet_httpclient_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `client.GetAsync("/users")` / `PostAsync` / etc. URL must
        // start with `/` or `http(s)://` (the latter gets scheme+host
        // stripped by `normalize_http_path`).
        Regex::new(
            r#"\.\s*(Get|Post|Put|Delete|Patch)Async\s*\(\s*"((?:https?://[^"]+)|/[^"]*)""#,
        )
        .unwrap()
    })
}

fn csharp_grpc_map_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // ASP.NET Core gRPC server: `app.MapGrpcService<UserServiceImpl>()`.
        // Captures the service-class name; gRPC convention strips the
        // `Impl`/`Service` suffix to recover the proto service name.
        Regex::new(r#"\.\s*MapGrpcService\s*<\s*([A-Za-z_][A-Za-z0-9_]*)\s*>"#).unwrap()
    })
}

fn csharp_grpc_client_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `new UserServiceClient(channel)` — generated gRPC stub.
        // Generated class always ends in `Client`. Skip common HTTP
        // collisions (`HttpClient`, `WebClient`, `RestClient`).
        Regex::new(r#"\bnew\s+([A-Za-z_][A-Za-z0-9_]*?)Client\s*\("#).unwrap()
    })
}

/// Strip the trailing `Impl` / `Service` suffix from a C# class name
/// to recover the proto service name. `UserServiceImpl` → `UserService`;
/// already-canonical `UserService` stays as is.
fn csharp_strip_impl_suffix(name: &str) -> String {
    if let Some(prefix) = name.strip_suffix("Impl") {
        return prefix.to_string();
    }
    name.to_string()
}

fn scan_csharp(file: &str, text: &str) -> Vec<ContractRow> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        // C# gRPC server: `app.MapGrpcService<FooImpl>()`
        if let Some(caps) = csharp_grpc_map_re().captures(line) {
            let svc = csharp_strip_impl_suffix(&caps[1]);
            out.push(ContractRow {
                contract_id: format!("grpc::{svc}"),
                kind: "grpc".to_string(),
                role: "provider".to_string(),
                method: None,
                path: Some(svc),
                topic: None,
                file: file.to_string(),
                line: (i + 1) as u32,
                language: "csharp".to_string(),
                framework: "grpc".to_string(),
            });
            continue;
        }
        // C# gRPC client: `new FooClient(channel)`. Skip well-known
        // non-gRPC client class names — HttpClient, WebClient, etc.
        if let Some(caps) = csharp_grpc_client_re().captures(line) {
            let svc = caps[1].to_string();
            if !matches!(svc.as_str(), "Http" | "Web" | "Rest" | "Soap" | "Service") {
                out.push(ContractRow {
                    contract_id: format!("grpc::{svc}"),
                    kind: "grpc".to_string(),
                    role: "consumer".to_string(),
                    method: None,
                    path: Some(svc),
                    topic: None,
                    file: file.to_string(),
                    line: (i + 1) as u32,
                    language: "csharp".to_string(),
                    framework: "grpc".to_string(),
                });
                continue;
            }
        }
        if let Some(caps) = aspnet_attribute_re().captures(line) {
            let method = caps[1].to_uppercase();
            let raw_path = caps.get(2).map(|m| m.as_str().to_string());
            let (normalized, contract_id) = match raw_path {
                Some(p) => {
                    let with_slash = if p.starts_with('/') { p } else { format!("/{p}") };
                    let n = normalize_http_path(&with_slash);
                    (n.clone(), format!("http::{method}::{n}"))
                }
                None => ("*".to_string(), format!("http::{method}::*")),
            };
            out.push(ContractRow {
                contract_id,
                kind: "http".to_string(),
                role: "provider".to_string(),
                method: Some(method),
                path: Some(normalized),
                topic: None,
                file: file.to_string(),
                line: (i + 1) as u32,
                language: "csharp".to_string(),
                framework: "aspnet".to_string(),
            });
            continue;
        }
        if let Some(caps) = aspnet_minimal_re().captures(line) {
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
                language: "csharp".to_string(),
                framework: "aspnet-minimal".to_string(),
            });
            continue;
        }
        if let Some(caps) = aspnet_httpclient_re().captures(line) {
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
                language: "csharp".to_string(),
                framework: "httpclient".to_string(),
            });
        }
    }
    // C# StackExchange.Redis pub/sub — same `.Publish(...)` / `.Subscribe(...)`
    // shape as Go (PascalCase verb). Reuse the Go regex (it's case-
    // sensitive so it won't match the lowercase JS/Python variants).
    for (i, line) in text.lines().enumerate() {
        if let Some(caps) = redis_go_publish_re().captures(line) {
            let topic = caps[1].to_string();
            let framework = if topic.contains('.') { "nats" } else { "redis" };
            out.push(ContractRow {
                contract_id: format!("topic::{topic}"),
                kind: "topic".to_string(), role: "publisher".to_string(),
                method: None, path: None, topic: Some(topic),
                file: file.to_string(), line: (i + 1) as u32,
                language: "csharp".to_string(), framework: framework.to_string(),
            });
            continue;
        }
        if redis_go_subscribe_re().is_match(line) {
            // Find the args of `.Subscribe(...)` and extract topic strings.
            let args_start = line.find(".Subscribe(").map(|p| p + ".Subscribe(".len()).unwrap_or(0);
            let after = &line[args_start..];
            for cap in quoted_topic_args_re().captures_iter(after) {
                let topic = cap[1].to_string();
                let framework = if topic.contains('.') { "nats" } else { "redis" };
                out.push(ContractRow {
                    contract_id: format!("topic::{topic}"),
                    kind: "topic".to_string(), role: "subscriber".to_string(),
                    method: None, path: None, topic: Some(topic),
                    file: file.to_string(), line: (i + 1) as u32,
                    language: "csharp".to_string(), framework: framework.to_string(),
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

// ─── Python gRPC ─────────────────────────────────────────────────────────────

fn py_grpc_servicer_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `add_AuthServiceServicer_to_server(servicer, server)` — provider.
        Regex::new(r#"\badd_(\w+?)Servicer_to_server\s*\("#).unwrap()
    })
}

fn py_grpc_stub_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `AuthServiceStub(channel)` — consumer. Gate on PascalCase since
        // `Stub` is also a generic suffix.
        Regex::new(r#"\b([A-Z][A-Za-z0-9_]*)Stub\s*\("#).unwrap()
    })
}

// ─── Redis / NATS pub-sub ────────────────────────────────────────────────────

fn redis_publish_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `<client>.publish('channel', ...)` (Python redis-py, Node
        // ioredis/node-redis). Channel must start with letter / digit /
        // `_` / `.` so we don't match `<X>.publish(this, ...)`.
        Regex::new(r#"\b[A-Za-z_][A-Za-z0-9_]*\.publish\s*\(\s*['"]([A-Za-z0-9_][\w./:\-]*)['"]"#).unwrap()
    })
}

fn redis_subscribe_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `<client>.subscribe('a', 'b', ...)`. We pre-match the call shape
        // then a follow-up pass extracts every quoted topic argument.
        Regex::new(r#"\b[A-Za-z_][A-Za-z0-9_]*\.subscribe\s*\("#).unwrap()
    })
}

fn redis_go_publish_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Two Go publisher shapes:
        //   * go-redis: `rdb.Publish(ctx, "channel", payload)` — ctx
        //     arg first, then topic.
        //   * NATS: `nc.Publish("subject", data)` — topic is first arg.
        // The first quoted argument is always the topic, so we look
        // for `.Publish(... "topic" ...)` and pluck the first string.
        Regex::new(r#"\b[A-Za-z_][A-Za-z0-9_]*\.Publish\s*\([^)"]*?"([A-Za-z0-9_][\w./:\-]*)""#).unwrap()
    })
}

fn redis_go_subscribe_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Two Go subscriber shapes:
        //   * go-redis: `rdb.Subscribe(ctx, "a", "b", ...)` — ctx arg
        //     then one or more topic args.
        //   * NATS: `nc.Subscribe("subject", handler)` — topic first.
        // Match the call shape; the topic-args pass picks up every
        // quoted argument in the parenthesised list.
        Regex::new(r#"\b[A-Za-z_][A-Za-z0-9_]*\.Subscribe\s*\("#).unwrap()
    })
}

fn quoted_topic_args_re() -> &'static Regex {
    // Inside an argument list, capture every single- or double-quoted
    // identifier-shaped string. Used to fan out multi-topic subscribe()
    // calls (Redis: `subscribe('a', 'b', 'c')`).
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"['"]([A-Za-z0-9_][\w./:\->]*)['"]"#).unwrap())
}

/// Determine whether a Redis-style `<client>.subscribe(...)` line is
/// truly a pub/sub subscribe vs something HTTP/JS-shaped (e.g. RxJS
/// `observable.subscribe(...)`). Gate: at least one argument must be
/// a string literal that looks like a topic name.
fn extract_subscribe_topics(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    // Find the `(...)` argument list of the matching subscribe call.
    let Some(start) = line.find(".subscribe(") else { return out };
    let after = &line[start + ".subscribe(".len()..];
    // Bound the scan to the first closing `)` at depth 0.
    let mut depth = 1usize;
    let mut end = after.len();
    for (i, c) in after.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = i;
                    break;
                }
            }
            _ => {}
        }
    }
    let args = &after[..end];
    for caps in quoted_topic_args_re().captures_iter(args) {
        out.push(caps[1].to_string());
    }
    out
}

/// Emit publish/subscribe contract rows for a Python or JS/TS file.
/// Both share the same `<client>.publish('topic', ...)` / `.subscribe(...)`
/// shape; `language` and the receiver heuristic differ.
///
/// Also detects Redis Streams (XADD / XREAD / XREADGROUP) — the
/// queueing primitive that bullmq, Sidekiq Pro, and most modern Redis-
/// backed event buses use instead of plain pub/sub.
fn emit_pubsub_rows(file: &str, text: &str, language: &str, out: &mut Vec<ContractRow>) {
    for (i, line) in text.lines().enumerate() {
        if let Some(caps) = redis_publish_re().captures(line) {
            let topic = caps[1].to_string();
            // Distinguish Redis from NATS by surrounding context. NATS
            // subjects conventionally use dotted hierarchy (`events.x.y`)
            // — if the topic contains a `.`, prefer NATS framework tag.
            let framework = if topic.contains('.') { "nats" } else { "redis" };
            out.push(ContractRow {
                contract_id: format!("topic::{topic}"),
                kind: "topic".to_string(),
                role: "publisher".to_string(),
                method: None,
                path: None,
                topic: Some(topic),
                file: file.to_string(),
                line: (i + 1) as u32,
                language: language.to_string(),
                framework: framework.to_string(),
            });
        }
        if redis_subscribe_re().is_match(line) {
            for topic in extract_subscribe_topics(line) {
                let framework = if topic.contains('.') { "nats" } else { "redis" };
                out.push(ContractRow {
                    contract_id: format!("topic::{topic}"),
                    kind: "topic".to_string(),
                    role: "subscriber".to_string(),
                    method: None,
                    path: None,
                    topic: Some(topic),
                    file: file.to_string(),
                    line: (i + 1) as u32,
                    language: language.to_string(),
                    framework: framework.to_string(),
                });
            }
        }
        // Redis Streams. XADD = publisher, XREAD/XREADGROUP = subscriber.
        if let Some(caps) = redis_streams_re().captures(line) {
            let topic = caps[1].to_string();
            // Redis Streams command flags (`BLOCK`, `STREAMS`, `MAXLEN`,
            // `GROUP`, `COUNT`, `NOACK`, etc.) appear before the actual
            // stream key in XREAD/XREADGROUP. When the topic capture
            // lands on one of these, the real stream key is a runtime
            // variable we can't resolve — drop the row rather than
            // emit a bogus `topic::BLOCK`. Seen on BullMQ where every
            // stream key is `KEYS[2]` or `eventStreamKey`.
            if matches!(
                topic.as_str(),
                "BLOCK" | "STREAMS" | "MAXLEN" | "MINID" | "GROUP" | "COUNT"
                    | "NOACK" | "NOMKSTREAM" | "LIMIT" | "ID" | "MKSTREAM" | "*"
            ) {
                continue;
            }
            // Determine role from the lowercase method name in the line.
            let lower = line.to_ascii_lowercase();
            let role = if lower.contains(".xadd(") {
                "publisher"
            } else {
                "subscriber"
            };
            out.push(ContractRow {
                contract_id: format!("topic::{topic}"),
                kind: "topic".to_string(),
                role: role.to_string(),
                method: None,
                path: None,
                topic: Some(topic),
                file: file.to_string(),
                line: (i + 1) as u32,
                language: language.to_string(),
                framework: "redis-streams".to_string(),
            });
        }
    }
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
        // Python gRPC provider: `add_FooServicer_to_server(...)`
        if let Some(caps) = py_grpc_servicer_re().captures(line) {
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
                language: "python".to_string(),
                framework: "grpc".to_string(),
            });
            continue;
        }
        // Python gRPC consumer: `FooServiceStub(channel)`. Gate on
        // PascalCase to avoid false matches like `Stub(...)` alone or
        // `MyStub` test classes — at least one letter before `Stub`.
        if let Some(caps) = py_grpc_stub_re().captures(line) {
            let svc = caps[1].to_string();
            // Skip very generic single-letter prefixes.
            if svc.len() >= 2 {
                out.push(ContractRow {
                    contract_id: format!("grpc::{svc}"),
                    kind: "grpc".to_string(),
                    role: "consumer".to_string(),
                    method: None,
                    path: Some(svc),
                    topic: None,
                    file: file.to_string(),
                    line: (i + 1) as u32,
                    language: "python".to_string(),
                    framework: "grpc".to_string(),
                });
                continue;
            }
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
    // Redis / NATS pub-sub for Python files.
    emit_pubsub_rows(file, text, "python", &mut out);
    out
}
