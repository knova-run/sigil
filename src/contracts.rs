//! Contract extraction: HTTP / WebSocket / gRPC / messaging / RPC /
//! GraphQL / database contracts across 9 source languages.
//!
//! Scans source files for two kinds of artifacts:
//!   - Providers: HTTP route handlers, gRPC service impls, WebSocket
//!     route registrations, topic subscribers, task workers, GraphQL
//!     schema fields, RPC procedure definitions, ORM table declarations.
//!   - Consumers: HTTP client calls, gRPC client stubs, topic publishers,
//!     task enqueuers, GraphQL queries, RPC client calls.
//!
//! When run in workspace mode, the matcher joins providers in one repo
//! against consumers in another by normalized `contract_id` to surface
//! cross-repo contract relationships without an LLM call.
//!
//! Coverage (current). Across Python, JS/TS, Go, Rust, Java/Kotlin,
//! Ruby, PHP, C#, and `.proto`/`.graphql`/`.sql`/`.yaml`/`.json`:
//!   - HTTP server: FastAPI, Django (path/url/re_path), DRF (router/
//!     @action), Flask, Express, NestJS, Spring (@*Mapping), Laravel,
//!     ASP.NET attributes + minimal API, Go net/http + gin/echo/chi,
//!     Rails routes, Rust axum/actix/rocket.
//!   - HTTP client: axios, fetch, superagent, requests, httpx,
//!     reqwest, HttpClient, generic `<wrapper>.<verb>('/path')`.
//!   - WebSocket: FastAPI `@app.websocket`, Express-ws, Spring STOMP,
//!     Go gorilla, browser `new WebSocket(...)`, ActionCable; socket.io
//!     `socket.on` / `socket.emit` events.
//!   - gRPC: `.proto` service+rpc, Go + Python + Java + C# + Rust tonic
//!     + Node `@grpc/grpc-js` provider+consumer stubs.
//!   - Topics / queues: Kafka, Redis pub/sub + Streams, NATS, AWS
//!     SQS/SNS/EventBridge, GCP Pub/Sub, RabbitMQ/AMQP.
//!   - Task queues: Celery, Sidekiq, bullmq, RQ, asynq.
//!   - RPC: tRPC, JSON-RPC.
//!   - GraphQL: SDL providers + Apollo/urql/Relay gql`` consumers.
//!   - Database: SQLAlchemy / Django / Alembic / raw `CREATE TABLE` /
//!     Mongo collections (owner + reader).
//!   - Spec ingestion: OpenAPI 2.0/3.x and AsyncAPI YAML/JSON.
//!
//! Env-var-aware: when a topic arg is `os.environ['X']` /
//! `process.env.X` / `ENV['X']` etc., the contract_id is encoded as
//! `topic::$ENV.<NAME>`. Workspace-level `.env` / docker-compose
//! resolution lifts this to literal values for confidence tiering.

use regex::Regex;
use serde::Serialize;
use std::path::Path;
use std::sync::OnceLock;

#[derive(Debug, Serialize, PartialEq)]
pub struct ContractRow {
    /// Composite join key. Per-kind shapes:
    ///   - http      `http::<METHOD>::<NORMALIZED_PATH>` (method=`*` when
    ///               the framework binds at dispatch time, e.g. Django)
    ///   - websocket `ws::<NORMALIZED_PATH>`
    ///   - event     `event::<NAME>` (socket.io)
    ///   - grpc      `grpc::<Service>[/<Method>]`
    ///   - topic     `topic::<topic-name>` or `topic::$ENV.<VARNAME>`
    ///   - task      `task::<task-name>`
    ///   - rpc       `rpc::<procedure-name>` (tRPC / JSON-RPC)
    ///   - graphql   `graphql::<Type>.<field>`
    ///   - db        `db::<table-or-collection>`
    /// Path-style params (`:id`, `{userId}`, `[id]`, `${slug}`) are
    /// normalized to `{param}` so the same contract from different
    /// framework conventions produces an identical id.
    pub contract_id: String,
    /// `http` | `websocket` | `event` | `grpc` | `topic` | `task` |
    /// `rpc` | `graphql` | `db`.
    pub kind: String,
    /// `provider` | `consumer` | `publisher` | `subscriber` | `owner`
    /// | `reader` | `writer`. The owner/reader/writer triad applies to
    /// db contracts; everything else uses provider/consumer or the
    /// queue-flavored publisher/subscriber.
    pub role: String,
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
    // The bare-`$slug` arm is ordered AFTER `${…}` so the braced form
    // wins on greedy alternation.
    static PARAM_RE: OnceLock<Regex> = OnceLock::new();
    let re = PARAM_RE.get_or_init(|| {
        Regex::new(r"\$\{[^}]+\}|\$[A-Za-z_][A-Za-z0-9_]*|:[A-Za-z_][A-Za-z0-9_]*|\{[^}]+\}|\[[^\]]+\]").unwrap()
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
    // 5. Strip trailing `/?` (optional-trailing-slash regex idiom:
    //    `/?$` becomes `/?` after step 1 + 2; collapse to no trailing
    //    slash so `normalize_http_path`'s downstream comparison joins
    //    cleanly with the same path written without an optional slash).
    if s.ends_with("/?") {
        s.truncate(s.len() - 2);
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
            "graphql" | "gql" => out.extend(scan_graphql(&rel, &text)),
            "sql" => emit_db_table_rows(&rel, &text, "sql", out),
            "yaml" | "yml" | "json" => out.extend(scan_openapi(&rel, &text)),
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
            | ".venv" | "venv" | "__pycache__"
            // Workspace data dirs. `.sigil-workspace` is sigil's own
            // workspace dir — running `sigil contracts --root <ws>`
            // against a workspace root must not recurse into the
            // generated `contracts.jsonl` / `cross_repo_refs.jsonl` /
            // `members.json` files. Mirrors `is_indexer_skipped_dir`
            // in src/index.rs.
            | ".sigil" | ".sigil-workspace" | ".repowise-workspace"
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
    // WebSocket — express-ws provider + browser/Node `new WebSocket()` consumer.
    for (i, line) in text.lines().enumerate() {
        if let Some(caps) = ws_express_re().captures(line) {
            push_ws_provider(&mut out, file, (i + 1) as u32, &caps[1], language, "express-ws");
            continue;
        }
        if let Some(caps) = ws_browser_re().captures(line) {
            push_ws_consumer(&mut out, file, (i + 1) as u32, &caps[1], language, "websocket");
        }
    }
    // tRPC routers (provider) + client procedure calls (consumer).
    let uses_trpc = text.contains("@trpc/server") || text.contains("@trpc/client")
        || text.contains("@trpc/react") || text.contains("t.procedure")
        || text.contains("trpc.");
    if uses_trpc {
        for (i, line) in text.lines().enumerate() {
            if let Some(caps) = trpc_procedure_re().captures(line) {
                let name = caps[1].to_string();
                out.push(ContractRow {
                    contract_id: format!("rpc::{name}"),
                    kind: "rpc".to_string(), role: "provider".to_string(),
                    method: None, path: Some(name.clone()), topic: None,
                    file: file.to_string(), line: (i + 1) as u32,
                    language: language.to_string(), framework: "trpc".to_string(),
                });
                continue;
            }
            if let Some(caps) = trpc_call_re().captures(line) {
                let name = caps[1].to_string();
                out.push(ContractRow {
                    contract_id: format!("rpc::{name}"),
                    kind: "rpc".to_string(), role: "consumer".to_string(),
                    method: None, path: Some(name.clone()), topic: None,
                    file: file.to_string(), line: (i + 1) as u32,
                    language: language.to_string(), framework: "trpc".to_string(),
                });
            }
        }
    }
    // GraphQL client calls (Apollo / urql / Relay / graphql-request).
    emit_graphql_client_rows(file, text, language, &mut out);
    // JSON-RPC request bodies (method:"name" json keys).
    if text.contains("jsonrpc") || text.contains("\"jsonrpc\"") {
        for (i, line) in text.lines().enumerate() {
            if let Some(caps) = jsonrpc_call_re().captures(line) {
                let name = caps[1].to_string();
                // Skip the literal `"jsonrpc"` constant
                if name != "jsonrpc" {
                    out.push(ContractRow {
                        contract_id: format!("rpc::{name}"),
                        kind: "rpc".to_string(), role: "consumer".to_string(),
                        method: None, path: Some(name.clone()), topic: None,
                        file: file.to_string(), line: (i + 1) as u32,
                        language: language.to_string(), framework: "jsonrpc".to_string(),
                    });
                }
            }
        }
    }
    // socket.io event-level (server `on` + client `emit`).
    emit_socketio_rows(file, text, language, &mut out);
    // bullmq: `new Queue('emails')` = enqueuer (consumer of the queue
    // surface); `new Worker('emails', ...)` = handler (provider). Gate
    // on the bullmq import so generic `new Queue(...)` test-class
    // constructors don't trigger.
    let uses_bullmq = text.contains("bullmq") || text.contains("bull");
    if uses_bullmq {
        for (i, line) in text.lines().enumerate() {
            if let Some(caps) = bullmq_queue_re().captures(line) {
                let name = caps[1].to_string();
                push_task_row(&mut out, file, (i + 1) as u32, &name, "consumer", language, "bullmq");
                continue;
            }
            if let Some(caps) = bullmq_worker_re().captures(line) {
                let name = caps[1].to_string();
                push_task_row(&mut out, file, (i + 1) as u32, &name, "provider", language, "bullmq");
            }
        }
    }
    // Redis / NATS pub-sub for JS/TS files.
    emit_pubsub_rows(file, text, language, &mut out);
    // SQS/SNS/EventBridge/GCP/AMQP — aws-sdk-js, amqplib, @google-cloud/pubsub.
    emit_cloud_queue_rows(file, text, language, &mut out);
    // DB table contracts (TypeORM / Prisma / Sequelize / Mongo).
    emit_db_table_rows(file, text, language, &mut out);
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
        // Env-var fallback for Go publish/subscribe — go-redis's
        // `rdb.Publish(ctx, os.Getenv("TOPIC"), payload)`. Skip the
        // ctx arg (first positional) and parse the second.
        if line.contains(".Publish(") && !line.contains("\".") {
            // Heuristic: only attempt env-var fallback when no string
            // literal precedes the `)`. Cheap pre-check: no quote chars
            // between `.Publish(` and the matching `)`. Falls through
            // when a literal IS present (handled by redis_go_publish_re).
            static GO_ENV_PUB_RE: OnceLock<Regex> = OnceLock::new();
            let go_pub = GO_ENV_PUB_RE.get_or_init(|| {
                Regex::new(r#"\.Publish\s*\(\s*[A-Za-z_][A-Za-z0-9_]*\s*,\s*((?:os\.Getenv|os\.LookupEnv)\s*\(\s*"([A-Z][A-Z0-9_]*)"\s*\))"#).unwrap()
            });
            if let Some(caps) = go_pub.captures(line) {
                let name = caps[2].to_string();
                out.push(ContractRow {
                    contract_id: format!("topic::$ENV.{name}"),
                    kind: "topic".to_string(), role: "publisher".to_string(),
                    method: None, path: None, topic: Some(format!("$ENV.{name}")),
                    file: file.to_string(), line: (i + 1) as u32,
                    language: "go".to_string(), framework: "redis".to_string(),
                });
                continue;
            }
        }
        // Go env-var fallback for Subscribe (handles single env-var arg).
        // The literal-topic case is already handled by the
        // `redis_go_subscribe_re()` block higher up, but the env-var
        // regex below requires `os.Getenv(...)` specifically so it
        // can't double-fire on a line that has only a literal.
        if line.contains(".Subscribe(") {
            static GO_ENV_SUB_RE: OnceLock<Regex> = OnceLock::new();
            let go_sub = GO_ENV_SUB_RE.get_or_init(|| {
                Regex::new(r#"\.Subscribe\s*\(\s*[A-Za-z_][A-Za-z0-9_]*\s*,\s*(?:os\.Getenv|os\.LookupEnv)\s*\(\s*"([A-Z][A-Z0-9_]*)"\s*\)"#).unwrap()
            });
            if let Some(caps) = go_sub.captures(line) {
                let name = caps[1].to_string();
                out.push(ContractRow {
                    contract_id: format!("topic::$ENV.{name}"),
                    kind: "topic".to_string(), role: "subscriber".to_string(),
                    method: None, path: None, topic: Some(format!("$ENV.{name}")),
                    file: file.to_string(), line: (i + 1) as u32,
                    language: "go".to_string(), framework: "redis".to_string(),
                });
                continue;
            }
        }
        if let Some(caps) = redis_go_publish_re().captures(line) {
            let topic = caps[1].to_string();
            let framework = if topic.starts_with("$ENV.") || !topic.contains('.') { "redis" } else { "nats" };
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
                let framework = if topic.starts_with("$ENV.") || !topic.contains('.') { "redis" } else { "nats" };
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
        // asynq (Go task queue): `NewTask("name", ...)` consumer +
        // `mux.HandleFunc("name", ...)` provider. Gated on the asynq
        // import to avoid HandleFunc collisions with net/http.
        let uses_asynq = text.contains("hibiken/asynq") || text.contains("\"asynq\"");
        if uses_asynq {
            if let Some(caps) = asynq_new_task_re().captures(line) {
                let name = caps[1].to_string();
                push_task_row(&mut out, file, (i + 1) as u32, &name, "consumer", "go", "asynq");
                continue;
            }
            if let Some(caps) = asynq_handle_re().captures(line) {
                let name = caps[1].to_string();
                push_task_row(&mut out, file, (i + 1) as u32, &name, "provider", "go", "asynq");
                continue;
            }
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
            // If the handler body that follows uses gorilla/websocket
            // Upgrader.Upgrade, retag this route as a WebSocket
            // provider in addition to the HTTP route. We look ahead up
            // to 30 lines for the upgrader call.
            let body_lines: String = text.lines()
                .skip(i + 1).take(30).collect::<Vec<_>>().join("\n");
            if body_lines.contains(".Upgrade(") && text.contains("gorilla/websocket") {
                out.push(ContractRow {
                    contract_id: format!("ws::{normalized}"),
                    kind: "websocket".to_string(), role: "provider".to_string(),
                    method: None, path: Some(normalized.clone()), topic: None,
                    file: file.to_string(), line: (i + 1) as u32,
                    language: "go".to_string(), framework: "gorilla".to_string(),
                });
            }
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
    // AWS / GCP / RabbitMQ for Go (aws-sdk-go, cloud.google.com/go/pubsub,
    // streadway/amqp). The same kwarg shapes used by the Python boto3
    // SDK appear as struct field names in Go (`QueueUrl: aws.String(...)`).
    emit_cloud_queue_rows(file, text, "go", &mut out);
    emit_db_table_rows(file, text, "go", &mut out);
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
    // Spring WebSocket / STOMP: `@MessageMapping("/chat.send")`.
    for (i, line) in text.lines().enumerate() {
        if let Some(caps) = ws_spring_re().captures(line) {
            push_ws_provider(&mut out, file, (i + 1) as u32, &caps[1], "java", "spring-stomp");
        }
    }
    // Java Jedis pub/sub: `jedis.publish("ch", msg)` /
    // `jedis.subscribe(listener, "ch1", "ch2")`. Same shape as the
    // Python/JS lowercase variants — reuse those regexes.
    for (i, line) in text.lines().enumerate() {
        if let Some(caps) = redis_publish_re().captures(line) {
            let topic = caps[1].to_string();
            let framework = if topic.starts_with("$ENV.") || !topic.contains('.') { "redis" } else { "nats" };
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
                let framework = if topic.starts_with("$ENV.") || !topic.contains('.') { "redis" } else { "nats" };
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
    emit_cloud_queue_rows(file, text, "java", &mut out);
    emit_db_table_rows(file, text, "java", &mut out);
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
    // Rails ActionCable mount: `mount ActionCable.server => '/cable'`
    for (i, line) in text.lines().enumerate() {
        if let Some(caps) = ws_action_cable_re().captures(line) {
            push_ws_provider(&mut out, file, (i + 1) as u32, &caps[1], "ruby", "actioncable");
        }
    }
    // Sidekiq workers (multi-line: class + include Sidekiq::Worker)
    emit_sidekiq_provider_rows(file, text, &mut out);
    // Sidekiq enqueuers: `EmailWorker.perform_async(...)`
    for (i, line) in text.lines().enumerate() {
        if let Some(caps) = sidekiq_call_re().captures(line) {
            let name = caps[1].to_string();
            push_task_row(&mut out, file, (i + 1) as u32, &name, "consumer", "ruby", "sidekiq");
        }
    }
    emit_pubsub_rows(file, text, "ruby", &mut out);
    emit_cloud_queue_rows(file, text, "ruby", &mut out);
    emit_db_table_rows(file, text, "ruby", &mut out);
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
            let framework = if topic.starts_with("$ENV.") || !topic.contains('.') { "redis" } else { "nats" };
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
                let framework = if topic.starts_with("$ENV.") || !topic.contains('.') { "redis" } else { "nats" };
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
            let framework = if topic.starts_with("$ENV.") || !topic.contains('.') { "redis" } else { "nats" };
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
            let framework = if topic.starts_with("$ENV.") || !topic.contains('.') { "redis" } else { "nats" };
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
                let framework = if topic.starts_with("$ENV.") || !topic.contains('.') { "redis" } else { "nats" };
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
    emit_cloud_queue_rows(file, text, "php", &mut out);
    emit_db_table_rows(file, text, "php", &mut out);
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
            let framework = if topic.starts_with("$ENV.") || !topic.contains('.') { "redis" } else { "nats" };
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
                let framework = if topic.starts_with("$ENV.") || !topic.contains('.') { "redis" } else { "nats" };
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
    emit_cloud_queue_rows(file, text, "csharp", &mut out);
    emit_db_table_rows(file, text, "csharp", &mut out);
    out
}

// ─── OpenAPI / AsyncAPI ingestion ────────────────────────────────────────────
//
// When a workspace member ships an `openapi.yaml` / `asyncapi.yaml`,
// sigil parses it as the authoritative contract. OpenAPI paths become
// `http::<METHOD>::<path>` provider rows; AsyncAPI channels become
// `topic::<channel>` publisher/subscriber rows. Each member that
// publishes a spec is treated as the provider of every route it lists.

fn scan_openapi(file: &str, text: &str) -> Vec<ContractRow> {
    let mut out = Vec::new();
    // Detect the spec kind by leading top-level key. Both OpenAPI 2.0
    // (swagger) and 3.x expose `paths:`; AsyncAPI exposes `channels:`.
    let is_openapi = text.contains("openapi:") || text.contains("swagger:") || text.contains("\"openapi\":") || text.contains("\"swagger\":");
    let is_asyncapi = text.contains("asyncapi:") || text.contains("\"asyncapi\":");
    if !is_openapi && !is_asyncapi {
        return out;
    }

    // YAML parsing — use the existing serde_yml dep already pulled in
    // for sigil's `yaml_index.rs`.
    let Ok(doc): Result<serde_yml::Value, _> = serde_yml::from_str(text) else {
        return out;
    };

    let methods = ["get", "post", "put", "delete", "patch", "options", "head"];

    if is_openapi
        && let Some(paths) = doc.get("paths").and_then(|p| p.as_mapping())
    {
        for (key, val) in paths {
            let Some(path) = key.as_str() else { continue };
            let Some(verbs) = val.as_mapping() else { continue };
            for (method_key, _) in verbs {
                let Some(m) = method_key.as_str() else { continue };
                let m_lower = m.to_ascii_lowercase();
                if !methods.contains(&m_lower.as_str()) { continue; }
                let normalized = normalize_http_path(path);
                let method_upper = m_lower.to_uppercase();
                out.push(ContractRow {
                    contract_id: format!("http::{method_upper}::{normalized}"),
                    kind: "http".to_string(),
                    role: "provider".to_string(),
                    method: Some(method_upper),
                    path: Some(normalized),
                    topic: None,
                    file: file.to_string(),
                    line: 0,
                    language: "yaml".to_string(),
                    framework: "openapi".to_string(),
                });
            }
        }
    }

    if is_asyncapi
        && let Some(channels) = doc.get("channels").and_then(|c| c.as_mapping())
    {
        for (key, val) in channels {
            let Some(channel) = key.as_str() else { continue };
            let Some(ops) = val.as_mapping() else { continue };
            for (op_key, _) in ops {
                let Some(op) = op_key.as_str() else { continue };
                let role = match op {
                    "publish" => "publisher",
                    "subscribe" => "subscriber",
                    _ => continue,
                };
                out.push(ContractRow {
                    contract_id: format!("topic::{channel}"),
                    kind: "topic".to_string(),
                    role: role.to_string(),
                    method: None,
                    path: None,
                    topic: Some(channel.to_string()),
                    file: file.to_string(),
                    line: 0,
                    language: "yaml".to_string(),
                    framework: "asyncapi".to_string(),
                });
            }
        }
    }

    out
}

// ─── Database table contracts ────────────────────────────────────────────────
//
// `kind=db`, contract_id `db::<table-or-collection>`. Roles:
//   * `owner` — declares the table/collection (ORM model, migration)
//   * `writer` — explicit INSERT/UPDATE/DELETE references (future work)
//   * `reader` — SELECT / .find / .objects.filter (future work)
//
// MVP focuses on `owner` rows from ORM declarations and migrations.
// Cross-service "two services both own/write the same table" is the
// strongest signal for hidden coupling.

fn sqlalchemy_tablename_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `__tablename__ = "users"` — SQLAlchemy declarative.
        Regex::new(r#"__tablename__\s*=\s*['"]([A-Za-z_][\w]*)['"]"#).unwrap()
    })
}

fn django_dbtable_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `db_table = 'projects'` — Django `class Meta`.
        Regex::new(r#"db_table\s*=\s*['"]([A-Za-z_][\w]*)['"]"#).unwrap()
    })
}

fn sql_create_table_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `CREATE TABLE [IF NOT EXISTS] <name>`. Case-insensitive.
        Regex::new(r#"(?i)CREATE\s+TABLE(?:\s+IF\s+NOT\s+EXISTS)?\s+`?"?([A-Za-z_][\w]*)`?"?"#).unwrap()
    })
}

fn alembic_create_table_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Alembic: `op.create_table('events', ...)`.
        Regex::new(r#"op\.create_table\s*\(\s*['"]([A-Za-z_][\w]*)['"]"#).unwrap()
    })
}

fn mongo_collection_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `db.collection('users')` / `db.collection("users")` /
        // `db.get_collection('orders')` / `db["users"]` /
        // `getCollection('users')`.
        Regex::new(r#"\b(?:collection|get_collection|getCollection)\s*\(\s*['"]([A-Za-z_][\w]*)['"]|\bdb\[\s*['"]([A-Za-z_][\w]*)['"]\s*\]"#).unwrap()
    })
}

fn push_db_row(
    out: &mut Vec<ContractRow>, file: &str, line_no: u32,
    table: &str, role: &str, language: &str, framework: &str,
) {
    out.push(ContractRow {
        contract_id: format!("db::{table}"),
        kind: "db".to_string(),
        role: role.to_string(),
        method: None,
        path: Some(table.to_string()),
        topic: None,
        file: file.to_string(),
        line: line_no,
        language: language.to_string(),
        framework: framework.to_string(),
    });
}

/// Shared scanner for DB table contracts across all languages. Each
/// language's scanner calls this — most patterns are language-
/// agnostic regex over text.
fn emit_db_table_rows(file: &str, text: &str, language: &str, out: &mut Vec<ContractRow>) {
    for (i, line) in text.lines().enumerate() {
        if let Some(caps) = sqlalchemy_tablename_re().captures(line) {
            push_db_row(out, file, (i + 1) as u32, &caps[1], "owner", language, "sqlalchemy");
            continue;
        }
        if let Some(caps) = django_dbtable_re().captures(line) {
            push_db_row(out, file, (i + 1) as u32, &caps[1], "owner", language, "django");
            continue;
        }
        if let Some(caps) = alembic_create_table_re().captures(line) {
            push_db_row(out, file, (i + 1) as u32, &caps[1], "owner", language, "alembic");
            continue;
        }
        if let Some(caps) = sql_create_table_re().captures(line) {
            push_db_row(out, file, (i + 1) as u32, &caps[1], "owner", language, "sql");
            continue;
        }
        if let Some(caps) = mongo_collection_re().captures(line) {
            // First capture group OR second capture group (the two alternatives).
            let name = caps.get(1).or(caps.get(2)).map(|m| m.as_str().to_string());
            if let Some(n) = name {
                push_db_row(out, file, (i + 1) as u32, &n, "reader", language, "mongo");
            }
        }
    }
}

// ─── GraphQL / tRPC / JSON-RPC ───────────────────────────────────────────────
//
// Three RPC-like protocols. contract_ids:
//   * GraphQL: `graphql::Query.<name>` / `graphql::Mutation.<name>` /
//              `graphql::Subscription.<name>`
//   * tRPC:    `rpc::<procedure-name>` (framework=`trpc`)
//   * JSON-RPC: `rpc::<method-name>` (framework=`jsonrpc`)

fn graphql_type_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Match `type Query {` / `type Mutation {` / `type Subscription {`
        // openings in a GraphQL SDL file. Captures the type name.
        Regex::new(r#"^\s*(?:extend\s+)?type\s+(Query|Mutation|Subscription)\b"#).unwrap()
    })
}

fn graphql_field_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Field declarations inside Query/Mutation/Subscription:
        //   `user(id: ID!): User`
        //   `users(limit: Int): [User!]!`
        // We capture the field name (the identifier before `(` or `:`).
        Regex::new(r#"^\s*([a-z_][A-Za-z0-9_]*)\s*[(:]"#).unwrap()
    })
}

fn scan_graphql(file: &str, text: &str) -> Vec<ContractRow> {
    let mut out = Vec::new();
    let mut current_type: Option<String> = None;
    let mut depth: i32 = 0;
    for (i, line) in text.lines().enumerate() {
        // Track brace depth so we know when we leave the type body.
        let opens = line.matches('{').count() as i32;
        let closes = line.matches('}').count() as i32;
        if let Some(caps) = graphql_type_re().captures(line) {
            current_type = Some(caps[1].to_string());
            depth = (depth + opens - closes).max(0);
            continue;
        }
        if current_type.is_some() && depth > 0
            && let Some(fcaps) = graphql_field_re().captures(line)
        {
            let field = fcaps[1].to_string();
            let ty = current_type.as_ref().unwrap();
            out.push(ContractRow {
                contract_id: format!("graphql::{ty}.{field}"),
                kind: "graphql".to_string(),
                role: "provider".to_string(),
                method: None,
                path: Some(format!("{ty}.{field}")),
                topic: None,
                file: file.to_string(),
                line: (i + 1) as u32,
                language: "graphql".to_string(),
                framework: "graphql".to_string(),
            });
        }
        depth = (depth + opens - closes).max(0);
        if depth == 0 {
            current_type = None;
        }
    }
    out
}

fn gql_client_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // gql`query GetUser(...) { user(id: …) {...} }` / `mutation Foo(...) { … }` /
        // `subscription Bar { … }`. Capture both the op kind (query/mutation/
        // subscription) and the root field name inside the braces.
        // Two captures needed; we run two passes.
        Regex::new(r#"\b(query|mutation|subscription)\s+[A-Za-z_][A-Za-z0-9_]*\s*(?:\([^)]*\))?\s*\{\s*([A-Za-z_][A-Za-z0-9_]*)"#).unwrap()
    })
}

fn emit_graphql_client_rows(file: &str, text: &str, language: &str, out: &mut Vec<ContractRow>) {
    // Apollo Client / urql / Relay / etc. all parse `gql` template
    // literals or pass the same shape. The op kind + root field name
    // determines the contract_id.
    let uses_gql = text.contains("@apollo/client") || text.contains("urql")
        || text.contains("react-relay") || text.contains("graphql-request")
        || text.contains("gql`") || text.contains("graphql`");
    if !uses_gql {
        return;
    }
    // The gql template can span multiple lines; flatten the full text
    // for the regex (each match retains the starting line via
    // counting newlines up to the match index).
    for caps in gql_client_re().captures_iter(text) {
        let op_kind = &caps[1];
        let field = &caps[2];
        let type_name = match op_kind {
            "query" => "Query",
            "mutation" => "Mutation",
            "subscription" => "Subscription",
            _ => continue,
        };
        // Locate the line number of the match.
        let pos = caps.get(0).map(|m| m.start()).unwrap_or(0);
        let line_no = text[..pos].matches('\n').count() as u32 + 1;
        out.push(ContractRow {
            contract_id: format!("graphql::{type_name}.{field}"),
            kind: "graphql".to_string(),
            role: "consumer".to_string(),
            method: None,
            path: Some(format!("{type_name}.{field}")),
            topic: None,
            file: file.to_string(),
            line: line_no,
            language: language.to_string(),
            framework: "graphql-client".to_string(),
        });
    }
}

fn trpc_procedure_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // tRPC router definitions: `userById: t.procedure.query(...)` /
        // `createUser: t.procedure.mutation(...)`.
        Regex::new(r#"\b([A-Za-z_][A-Za-z0-9_]*)\s*:\s*t\.procedure\.(?:query|mutation|subscription)"#).unwrap()
    })
}

fn trpc_call_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Client: `trpc.userById.useQuery(...)` / `.useMutation(...)`.
        // Capture procedure name after `trpc.`.
        Regex::new(r#"\btrpc\.([A-Za-z_][A-Za-z0-9_.]*)\.(?:useQuery|useMutation|useSubscription|query|mutate|subscribe)\s*\("#).unwrap()
    })
}

fn jsonrpc_method_decl_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // jsonrpcserver (Python): `@method` (bare) or
        // `@method(name="users.create")`.
        Regex::new(r#"@method(?:\s*\(\s*name\s*=\s*['"]([^'"]+)['"]\s*\))?\s*$"#).unwrap()
    })
}

fn jsonrpc_call_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // JSON-RPC request body has `"method": "name"`. We capture the
        // method name as the contract.
        Regex::new(r#"['"]method['"]\s*:\s*['"]([A-Za-z_][\w./:\-]*)['"]"#).unwrap()
    })
}

// ─── Cloud queue contracts (SQS / SNS / GCP Pub/Sub / RabbitMQ / etc.) ──────
//
// These emit `kind=topic` rows. The topic identity is extracted from
// SDK kwargs (e.g. `QueueUrl=…/orders` → `orders`, `TopicArn=…:user-events`
// → `user-events`) or positional args (`channel.basic_publish(...,
// routing_key='orders', ...)` → `orders`).

fn sqs_send_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `<sqs>.send_message(QueueUrl='…/queue-name', …)` and the
        // batch variant. Captures the QueueUrl literal; we trim to
        // the last URL segment afterwards.
        Regex::new(r#"\.send_message(?:_batch)?\s*\([^)]*QueueUrl\s*=\s*['"]([^'"]+)['"]"#).unwrap()
    })
}

fn sqs_recv_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"\.receive_message\s*\([^)]*QueueUrl\s*=\s*['"]([^'"]+)['"]"#).unwrap()
    })
}

fn sns_publish_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `<sns>.publish(TopicArn='arn:aws:sns:…:user-events', …)`.
        // The trailing `:<name>` is the topic name we want.
        Regex::new(r#"\.publish\s*\([^)]*TopicArn\s*=\s*['"]([^'"]+)['"]"#).unwrap()
    })
}

fn eventbridge_put_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `<events>.put_events(Entries=[{'Source': 'my-app', 'DetailType':
        // 'OrderPlaced', ...}])`. We pull the DetailType which acts as
        // the join key.
        Regex::new(r#"['"]DetailType['"]\s*:\s*['"]([^'"]+)['"]"#).unwrap()
    })
}

fn gcp_topic_path_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `publisher.topic_path('proj', 'user-events')`. Topic = 2nd arg.
        Regex::new(r#"\.topic_path\s*\(\s*['"][^'"]*['"]\s*,\s*['"]([^'"]+)['"]"#).unwrap()
    })
}

fn gcp_subscription_path_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"\.subscription_path\s*\(\s*['"][^'"]*['"]\s*,\s*['"]([^'"]+)['"]"#).unwrap()
    })
}

fn amqp_publish_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // pika (Python): `channel.basic_publish(exchange='', routing_key='orders', …)`.
        // amqplib (Node): `channel.publish(exchange, 'order.placed', body)` /
        //                 `channel.sendToQueue('orders', body)`.
        // The routing_key / topic name is what we want.
        Regex::new(
            r#"\.basic_publish\s*\([^)]*routing_key\s*=\s*['"]([^'"]+)['"]|\.publish\s*\(\s*['"][^'"]*['"]\s*,\s*['"]([^'"]+)['"]|\.sendToQueue\s*\(\s*['"]([^'"]+)['"]"#,
        )
        .unwrap()
    })
}

fn amqp_consume_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // pika: `channel.basic_consume(queue='orders', …)`.
        // amqplib: `channel.consume('orders', handler)`.
        Regex::new(r#"\.basic_consume\s*\([^)]*queue\s*=\s*['"]([^'"]+)['"]|\.consume\s*\(\s*['"]([^'"]+)['"]"#).unwrap()
    })
}

/// Extract the trailing path segment from a URL or ARN. Used to
/// recover queue/topic names from SQS QueueUrls, SNS TopicArns, and
/// Azure Service Bus URLs.
fn last_path_segment(s: &str) -> String {
    // For ARNs (arn:aws:sns:region:account:name), split on `:`.
    if s.starts_with("arn:") {
        if let Some(last) = s.rsplit(':').next() {
            return last.to_string();
        }
    }
    // For URLs and queue names, split on `/`.
    s.rsplit('/').next().unwrap_or(s).to_string()
}

fn push_cloud_topic(
    out: &mut Vec<ContractRow>, file: &str, line_no: u32,
    topic: &str, role: &str, language: &str, framework: &str,
) {
    out.push(ContractRow {
        contract_id: format!("topic::{topic}"),
        kind: "topic".to_string(),
        role: role.to_string(),
        method: None,
        path: None,
        topic: Some(topic.to_string()),
        file: file.to_string(),
        line: line_no,
        language: language.to_string(),
        framework: framework.to_string(),
    });
}

/// Shared cloud-queue scanner that emits SQS / SNS / EventBridge / GCP
/// Pub/Sub / AMQP rows. Called from each language's scanner.
fn emit_cloud_queue_rows(file: &str, text: &str, language: &str, out: &mut Vec<ContractRow>) {
    for (i, line) in text.lines().enumerate() {
        if let Some(caps) = sqs_send_re().captures(line) {
            let topic = last_path_segment(&caps[1]);
            push_cloud_topic(out, file, (i + 1) as u32, &topic, "publisher", language, "sqs");
            continue;
        }
        if let Some(caps) = sqs_recv_re().captures(line) {
            let topic = last_path_segment(&caps[1]);
            push_cloud_topic(out, file, (i + 1) as u32, &topic, "subscriber", language, "sqs");
            continue;
        }
        if let Some(caps) = sns_publish_re().captures(line) {
            let topic = last_path_segment(&caps[1]);
            push_cloud_topic(out, file, (i + 1) as u32, &topic, "publisher", language, "sns");
            continue;
        }
        if let Some(caps) = eventbridge_put_re().captures(line) {
            let topic = caps[1].to_string();
            push_cloud_topic(out, file, (i + 1) as u32, &topic, "publisher", language, "eventbridge");
            continue;
        }
        if let Some(caps) = gcp_topic_path_re().captures(line) {
            let topic = caps[1].to_string();
            push_cloud_topic(out, file, (i + 1) as u32, &topic, "publisher", language, "gcp-pubsub");
            continue;
        }
        if let Some(caps) = gcp_subscription_path_re().captures(line) {
            let topic = caps[1].to_string();
            push_cloud_topic(out, file, (i + 1) as u32, &topic, "subscriber", language, "gcp-pubsub");
            continue;
        }
        if let Some(caps) = amqp_publish_re().captures(line) {
            // Three capture groups; pick the first non-None.
            let topic = caps.get(1).or(caps.get(2)).or(caps.get(3))
                .map(|m| m.as_str().to_string());
            if let Some(t) = topic {
                push_cloud_topic(out, file, (i + 1) as u32, &t, "publisher", language, "amqp");
                continue;
            }
        }
        if let Some(caps) = amqp_consume_re().captures(line) {
            let topic = caps.get(1).or(caps.get(2))
                .map(|m| m.as_str().to_string());
            if let Some(t) = topic {
                push_cloud_topic(out, file, (i + 1) as u32, &t, "subscriber", language, "amqp");
            }
        }
    }
}

// ─── Task queue contracts ────────────────────────────────────────────────────
//
// Named-task contracts represent cross-process work hand-offs. The
// provider is the worker that handles the task; the consumer is the
// code that enqueues it. contract_id is `task::<name>`.
//
// Frameworks covered: Celery (Python), Sidekiq (Ruby), bullmq (Node),
// RQ (Python), asynq (Go). Hangfire (C#) and gocelery share the same
// regex shape as their language siblings and get caught incidentally.

fn celery_task_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `@app.task` (bare) or `@app.task(name='emails.send_welcome')`.
        // Captures the explicit name if present; bare decorators are
        // handled by a follow-up function-name lookup.
        Regex::new(r#"@(?:app|celery|celery_app|tasks)\.task(?:\s*\(\s*(?:[^)]*?name\s*=\s*['"]([^'"]+)['"])?[^)]*\))?\s*$"#).unwrap()
    })
}

fn celery_call_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `<name>.delay(...)` or `<name>.apply_async(...)`. The name
        // captured is the local function reference; cross-repo matching
        // joins on the registered Celery task name (which is the
        // function name by default).
        Regex::new(r#"\b([A-Za-z_][A-Za-z0-9_]*)\.(?:delay|apply_async)\s*\("#).unwrap()
    })
}

fn rq_enqueue_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `queue.enqueue('myapp.tasks.send_email', ...)`. Always uses a
        // string task path (rq's preferred form for cross-module calls).
        Regex::new(r#"\b\w+\.enqueue\s*\(\s*['"]([A-Za-z_][\w.]*)['"]"#).unwrap()
    })
}

fn sidekiq_worker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `include Sidekiq::Worker` / `include Sidekiq::Job` — the
        // CLASS in scope is the task provider. We need the class name
        // from the surrounding `class X` line, so the scan is two-pass.
        Regex::new(r#"include\s+Sidekiq::(?:Worker|Job)"#).unwrap()
    })
}

fn sidekiq_call_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `EmailWorker.perform_async(...)` / `.perform_in(1.hour, ...)`.
        Regex::new(r#"\b([A-Z][A-Za-z0-9_]*)\.(?:perform_async|perform_in|perform_at|perform_later)\s*\("#).unwrap()
    })
}

fn bullmq_queue_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `new Queue('emails')` — consumer side (the enqueuer). Worker
        // is detected separately. Capture queue name.
        Regex::new(r#"\bnew\s+Queue\s*\(\s*['"`]([^'"`]+)['"`]"#).unwrap()
    })
}

fn bullmq_worker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"\bnew\s+Worker\s*\(\s*['"`]([^'"`]+)['"`]"#).unwrap()
    })
}

fn asynq_new_task_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `asynq.NewTask("email:send", payload)` — consumer.
        Regex::new(r#"\basynq\.NewTask\s*\(\s*"([^"]+)""#).unwrap()
    })
}

fn asynq_handle_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `mux.HandleFunc("email:send", handler)` — provider, only when
        // file uses asynq.
        Regex::new(r#"\bmux\.HandleFunc\s*\(\s*"([^"]+)""#).unwrap()
    })
}

fn push_task_row(
    out: &mut Vec<ContractRow>, file: &str, line_no: u32,
    name: &str, role: &str, language: &str, framework: &str,
) {
    out.push(ContractRow {
        contract_id: format!("task::{name}"),
        kind: "task".to_string(),
        role: role.to_string(),
        method: None,
        path: Some(name.to_string()),
        topic: Some(name.to_string()),
        file: file.to_string(),
        line: line_no,
        language: language.to_string(),
        framework: framework.to_string(),
    });
}

/// Emit Celery task provider rows. Two-pass: when a `@app.task` line
/// fires, look ahead for `def <name>(...)` on the next non-decorator
/// line and use the function name as the default task name.
fn emit_celery_provider_rows(file: &str, text: &str, out: &mut Vec<ContractRow>) {
    let lines: Vec<&str> = text.lines().collect();
    for i in 0..lines.len() {
        let Some(caps) = celery_task_re().captures(lines[i]) else { continue };
        // Explicit name= wins; else look ahead.
        let name = if let Some(m) = caps.get(1) {
            m.as_str().to_string()
        } else {
            let mut j = i + 1;
            let mut found = None;
            while j < lines.len() && j < i + 6 {
                let l = lines[j].trim_start();
                if l.is_empty() || l.starts_with('#') || l.starts_with('@') {
                    j += 1; continue;
                }
                static DEF_RE: OnceLock<Regex> = OnceLock::new();
                let def = DEF_RE.get_or_init(|| Regex::new(r"^def\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(").unwrap());
                if let Some(dc) = def.captures(l) {
                    found = Some(dc[1].to_string());
                }
                break;
            }
            match found { Some(n) => n, None => continue }
        };
        push_task_row(out, file, (i + 1) as u32, &name, "provider", "python", "celery");
    }
}

/// Emit Sidekiq Worker provider rows. Like Celery, we need the class
/// name from the surrounding scope. We walk lines: when a `class X`
/// line is followed by an `include Sidekiq::Worker/Job` within ~10
/// lines, X is a task provider.
fn emit_sidekiq_provider_rows(file: &str, text: &str, out: &mut Vec<ContractRow>) {
    static CLASS_RE: OnceLock<Regex> = OnceLock::new();
    let class_re = CLASS_RE.get_or_init(|| Regex::new(r"^\s*class\s+([A-Z][A-Za-z0-9_]*)").unwrap());
    let lines: Vec<&str> = text.lines().collect();
    let mut current_class: Option<(String, usize)> = None;
    for (i, line) in lines.iter().enumerate() {
        if let Some(c) = class_re.captures(line) {
            current_class = Some((c[1].to_string(), i + 1));
        }
        if sidekiq_worker_re().is_match(line)
            && let Some((cls, class_line)) = &current_class
        {
            push_task_row(out, file, *class_line as u32, cls, "provider", "ruby", "sidekiq");
            current_class = None; // emit once per class
        }
    }
}

fn kafka_send_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"\b[A-Za-z_][A-Za-z0-9_]*\.send\(\s*['"]([A-Za-z_][\w./:\-]*)['"]"#,
        )
        .unwrap()
    })
}

// ─── WebSocket contracts ─────────────────────────────────────────────────────
//
// Two contract shapes:
//
//   1. URL-level — the WebSocket route's path. Captured as a separate
//      kind=`websocket` row with contract_id `ws::<path>` so it joins
//      cleanly with browser `new WebSocket("ws://host<path>")` consumers.
//
//   2. Event-level — socket.io `socket.on('event:name', …)` (provider)
//      and `socket.emit('event:name', …)` (consumer). contract_id
//      `event::<name>`, framework `socket.io`.

fn ws_fastapi_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"@(?:app|router)\.websocket\(\s*['"]([^'"]+)['"]"#).unwrap()
    })
}

fn ws_express_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // express-ws: `app.ws('/path', handler)`. Receiver `app|router` only.
        Regex::new(r#"\b(?:app|router|expressApp)\.ws\(\s*['"`](/[^'"`]*)['"`]"#).unwrap()
    })
}

fn ws_browser_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `new WebSocket('ws://host/path')` / `new WebSocket('wss://...')` /
        // `new WebSocket('/path')` (when same-origin).
        Regex::new(r#"\bnew\s+WebSocket\s*\(\s*['"`]((?:wss?://[^'"`]+)|/[^'"`]*)['"`]"#).unwrap()
    })
}

fn ws_spring_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Spring STOMP: `@MessageMapping("/chat.send")`.
        Regex::new(r#"@MessageMapping\s*\(\s*['"]([^'"]+)['"]"#).unwrap()
    })
}

fn ws_action_cable_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `mount ActionCable.server => '/cable'`.
        Regex::new(r#"mount\s+ActionCable\.server\s*=>\s*['"]([^'"]+)['"]"#).unwrap()
    })
}

fn ws_socketio_on_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `socket.on('event', cb)` — but ONLY when the file imports
        // socket.io (we content-sniff before calling this).
        // Skip `'connection'` and `'disconnect'` which are framework
        // lifecycle events, not user-defined contracts.
        Regex::new(r#"\bsocket\.on\s*\(\s*['"`]([A-Za-z_][\w:.\-]*)['"`]"#).unwrap()
    })
}

fn ws_socketio_emit_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"\bsocket\.emit\s*\(\s*['"`]([A-Za-z_][\w:.\-]*)['"`]"#).unwrap()
    })
}

/// Strip scheme + host from a `ws://host/path` or `wss://host/path` URL.
/// Mirrors `normalize_http_path`'s `http(s)://` stripping.
fn ws_strip_host(url: &str) -> String {
    let stripped = url
        .strip_prefix("ws://")
        .or_else(|| url.strip_prefix("wss://"));
    match stripped {
        Some(rest) => match rest.split_once('/') {
            Some((_, p)) => format!("/{p}"),
            None => "/".to_string(),
        },
        None => url.to_string(),
    }
}

/// Common emission helper for a URL-level WebSocket route.
fn push_ws_provider(
    out: &mut Vec<ContractRow>, file: &str, line_no: u32,
    path: &str, language: &str, framework: &str,
) {
    let normalized = normalize_http_path(path);
    out.push(ContractRow {
        contract_id: format!("ws::{normalized}"),
        kind: "websocket".to_string(),
        role: "provider".to_string(),
        method: None,
        path: Some(normalized),
        topic: None,
        file: file.to_string(),
        line: line_no,
        language: language.to_string(),
        framework: framework.to_string(),
    });
}

fn push_ws_consumer(
    out: &mut Vec<ContractRow>, file: &str, line_no: u32,
    url: &str, language: &str, framework: &str,
) {
    let stripped = ws_strip_host(url);
    let normalized = normalize_http_path(&stripped);
    out.push(ContractRow {
        contract_id: format!("ws::{normalized}"),
        kind: "websocket".to_string(),
        role: "consumer".to_string(),
        method: None,
        path: Some(normalized),
        topic: None,
        file: file.to_string(),
        line: line_no,
        language: language.to_string(),
        framework: framework.to_string(),
    });
}

/// Detect socket.io event-level contracts. Both server and client use
/// the same `socket.on(...)` / `socket.emit(...)` shape — the role is
/// inferred from which one fires.
fn emit_socketio_rows(file: &str, text: &str, language: &str, out: &mut Vec<ContractRow>) {
    let file_uses_socketio = text.contains("socket.io") || text.contains("socket.io-client")
        || text.contains("Server(server)") || text.contains("require('socket.io')")
        || text.contains("from 'socket.io'");
    if !file_uses_socketio {
        return;
    }
    let skip = |name: &str| matches!(
        name,
        "connection" | "disconnect" | "disconnecting" | "error" | "connect"
            | "connect_error" | "reconnect" | "reconnect_attempt" | "reconnect_error"
            | "reconnect_failed" | "newListener" | "removeListener"
    );
    for (i, line) in text.lines().enumerate() {
        if let Some(caps) = ws_socketio_on_re().captures(line) {
            let event = caps[1].to_string();
            if !skip(&event) {
                out.push(ContractRow {
                    contract_id: format!("event::{event}"),
                    kind: "event".to_string(),
                    role: "provider".to_string(),
                    method: None,
                    path: Some(event.clone()),
                    topic: Some(event),
                    file: file.to_string(),
                    line: (i + 1) as u32,
                    language: language.to_string(),
                    framework: "socket.io".to_string(),
                });
                continue;
            }
        }
        if let Some(caps) = ws_socketio_emit_re().captures(line) {
            let event = caps[1].to_string();
            if !skip(&event) {
                out.push(ContractRow {
                    contract_id: format!("event::{event}"),
                    kind: "event".to_string(),
                    role: "consumer".to_string(),
                    method: None,
                    path: Some(event.clone()),
                    topic: Some(event),
                    file: file.to_string(),
                    line: (i + 1) as u32,
                    language: language.to_string(),
                    framework: "socket.io".to_string(),
                });
            }
        }
    }
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

// ─── Env-var-aware topic resolution ──────────────────────────────────────────
//
// When a publish/subscribe arg is `os.environ['X']` / `process.env.X` /
// `ENV['X']` / `os.Getenv("X")` etc., sigil can't statically know the
// VALUE of X — but the env-var NAME is itself usable as a contract
// identity (the team convention is to use the same env-var name in
// every service that talks to the same topic).
//
// The matcher emits these as `topic::$ENV.<varname>`. Workspace-level
// `.env` / `docker-compose.yml` / `values.yaml` parsing later resolves
// these to literal values for stronger matching (see workspace.rs).

/// Try to extract an env-var name from a single argument expression.
/// Recognises every common syntax across languages:
///
///   * Python: `os.environ['X']`, `os.environ.get('X')`, `os.getenv('X')`,
///             `os.getenv('X', default)`
///   * JS/TS:  `process.env.X`, `process.env['X']`, `process.env["X"]`,
///             `import.meta.env.X`
///   * Ruby:   `ENV['X']`, `ENV.fetch('X')`, `ENV['X'] || 'default'`
///   * PHP:    `getenv('X')`, `$_ENV['X']`, `$_SERVER['X']`
///   * Go:     `os.Getenv("X")`, `os.LookupEnv("X")`
///   * Rust:   `std::env::var("X")`, `env::var("X")`, `env!("X")`
///   * Java:   `System.getenv("X")`
///   * C#:     `Environment.GetEnvironmentVariable("X")`
fn extract_env_var_ref(arg: &str) -> Option<String> {
    let a = arg.trim();
    // Match the recognised env-var lookup forms, returning the var name.
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r#"(?x)
              (?: os\.environ(?:\.get)?     # Python os.environ['X'] / .get('X')
                  | os\.getenv              # Python os.getenv('X')
                  | os\.Getenv              # Go os.Getenv("X")
                  | os\.LookupEnv           # Go os.LookupEnv("X")
                  | process\.env\.         # JS process.env.X
                  | process\.env           # JS process.env['X']
                  | import\.meta\.env\.    # Vite/ESM import.meta.env.X
                  | import\.meta\.env       # Vite/ESM import.meta.env['X']
                  | ENV\.fetch              # Ruby ENV.fetch('X')
                  | ENV                     # Ruby ENV['X']
                  | getenv                  # PHP getenv('X')
                  | \$_ENV                  # PHP $_ENV['X']
                  | \$_SERVER               # PHP $_SERVER['X']
                  | std::env::var           # Rust std::env::var
                  | env::var                # Rust env::var
                  | env!                    # Rust env!
                  | System\.getenv          # Java System.getenv("X")
                  | Environment\.GetEnvironmentVariable  # C#
              )
              \s* [\[\(]? \s*
              ['"]?([A-Z][A-Z0-9_]*)['"]?
            "#,
        )
        .unwrap()
    });
    re.captures(a).and_then(|c| c.get(1)).map(|m| m.as_str().to_string())
}

/// Topic descriptor used by `emit_pubsub_rows` and friends. A topic
/// argument either resolves to a string literal, an env-var reference,
/// or nothing static (dynamic — fstring / function call / etc.).
#[derive(Debug, Clone)]
enum TopicArg {
    Literal(String),
    Env(String),
}

impl TopicArg {
    /// Build the contract_id for this topic, prefixing env vars with
    /// `$ENV.` so they stay distinct from literal topic names.
    fn to_contract_id(&self) -> String {
        match self {
            TopicArg::Literal(s) => format!("topic::{s}"),
            TopicArg::Env(name) => format!("topic::$ENV.{name}"),
        }
    }

    fn topic_field(&self) -> String {
        match self {
            TopicArg::Literal(s) => s.clone(),
            TopicArg::Env(name) => format!("$ENV.{name}"),
        }
    }
}

/// Try to interpret a free-form argument expression as either a literal
/// string or an env-var reference. Returns None for anything else
/// (fstring, function call, identifier without a known constant).
fn parse_topic_arg(arg: &str) -> Option<TopicArg> {
    let a = arg.trim();
    // Literal string — try both single and double quote variants.
    static LIT_RE: OnceLock<Regex> = OnceLock::new();
    let lit_re = LIT_RE.get_or_init(|| {
        Regex::new(r#"^['"`]([A-Za-z0-9_][\w./:\-]*)['"`]\s*$"#).unwrap()
    });
    if let Some(c) = lit_re.captures(a) {
        return Some(TopicArg::Literal(c[1].to_string()));
    }
    // Env-var reference.
    extract_env_var_ref(a).map(TopicArg::Env)
}

/// Capture the full first-argument expression from a method call on
/// `line` whose method matches one of the verbs. Used by env-aware
/// pub-sub detection: when the existing regex didn't match because
/// the first arg isn't a string literal, fall back to this looser
/// extraction and re-interpret via `parse_topic_arg`.
fn extract_first_arg_after_method(line: &str, method_names: &[&str]) -> Option<String> {
    for m in method_names {
        let needle = format!(".{m}(");
        if let Some(idx) = line.find(&needle) {
            let after = &line[idx + needle.len()..];
            // Paren-balance to find the boundary of arg 1.
            let bytes = after.as_bytes();
            let mut depth = 1i32;
            let mut end = 0usize;
            for i in 0..bytes.len() {
                match bytes[i] {
                    b'(' | b'[' | b'{' => depth += 1,
                    b')' | b']' | b'}' => {
                        depth -= 1;
                        if depth == 0 { end = i; break; }
                    }
                    b',' if depth == 1 => { end = i; break; }
                    _ => {}
                }
            }
            return Some(after[..end].to_string());
        }
    }
    None
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

/// Walk a comma-separated argument list at depth 1 of a method call's
/// parens. Returns the raw string of each top-level argument. Handles
/// nested parens/brackets/braces correctly so a single env-var lookup
/// `os.environ['X']` is one argument, not a sequence of three.
fn split_top_level_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    let bytes = args.as_bytes();
    for i in 0..bytes.len() {
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                out.push(args[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    let tail = args[start..].trim().to_string();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

/// Determine whether a Redis-style `<client>.subscribe(...)` line is
/// truly a pub/sub subscribe vs something HTTP/JS-shaped (e.g. RxJS
/// `observable.subscribe(...)`). Walks the top-level arguments and
/// parses each as either a literal topic or an env-var reference.
/// Returns canonical contract topic identifiers (e.g. `orders` or
/// `$ENV.ORDERS_TOPIC`).
fn extract_subscribe_topics(line: &str) -> Vec<String> {
    let mut out = Vec::new();
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
    for arg in split_top_level_args(args) {
        if let Some(topic) = parse_topic_arg(&arg) {
            // Literal → bare name; env → $ENV.<name>.
            out.push(topic.topic_field());
        }
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
        let mut publisher_matched = false;
        if let Some(caps) = redis_publish_re().captures(line) {
            let topic = caps[1].to_string();
            // Distinguish Redis from NATS by surrounding context. NATS
            // subjects conventionally use dotted hierarchy (`events.x.y`)
            // — if the topic contains a `.`, prefer NATS framework tag.
            let framework = if topic.starts_with("$ENV.") || !topic.contains('.') { "redis" } else { "nats" };
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
            publisher_matched = true;
        }
        // Env-var fallback for publisher: when the line has `.publish(`
        // but the literal-string regex didn't fire, try parsing the
        // first arg as an env-var ref.
        if !publisher_matched && line.contains(".publish(") {
            if let Some(arg) = extract_first_arg_after_method(line, &["publish"])
                && let Some(topic_arg) = parse_topic_arg(&arg)
                && matches!(topic_arg, TopicArg::Env(_))
            {
                out.push(ContractRow {
                    contract_id: topic_arg.to_contract_id(),
                    kind: "topic".to_string(),
                    role: "publisher".to_string(),
                    method: None,
                    path: None,
                    topic: Some(topic_arg.topic_field()),
                    file: file.to_string(),
                    line: (i + 1) as u32,
                    language: language.to_string(),
                    framework: "redis".to_string(),
                });
            }
        }
        let mut subscriber_matched = false;
        if redis_subscribe_re().is_match(line) {
            for topic in extract_subscribe_topics(line) {
                let framework = if topic.starts_with("$ENV.") || !topic.contains('.') { "redis" } else { "nats" };
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
                subscriber_matched = true;
            }
        }
        // Env-var fallback for subscriber.
        if !subscriber_matched && line.contains(".subscribe(") {
            if let Some(arg) = extract_first_arg_after_method(line, &["subscribe"])
                && let Some(topic_arg) = parse_topic_arg(&arg)
                && matches!(topic_arg, TopicArg::Env(_))
            {
                out.push(ContractRow {
                    contract_id: topic_arg.to_contract_id(),
                    kind: "topic".to_string(),
                    role: "subscriber".to_string(),
                    method: None,
                    path: None,
                    topic: Some(topic_arg.topic_field()),
                    file: file.to_string(),
                    line: (i + 1) as u32,
                    language: language.to_string(),
                    framework: "redis".to_string(),
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
        // FastAPI @app.websocket("/path")
        if let Some(caps) = ws_fastapi_re().captures(line) {
            push_ws_provider(&mut out, file, (i + 1) as u32, &caps[1], "python", "fastapi");
            continue;
        }
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
    // JSON-RPC providers (`@method` / `@method(name="…")`) — same
    // multi-line lookup as Celery (decorator above `def name`).
    let lines: Vec<&str> = text.lines().collect();
    for i in 0..lines.len() {
        if let Some(caps) = jsonrpc_method_decl_re().captures(lines[i]) {
            let name = if let Some(m) = caps.get(1) {
                m.as_str().to_string()
            } else {
                let mut j = i + 1;
                let mut found = None;
                while j < lines.len() && j < i + 6 {
                    let l = lines[j].trim_start();
                    if l.is_empty() || l.starts_with('#') || l.starts_with('@') {
                        j += 1; continue;
                    }
                    static DEF_RE: OnceLock<Regex> = OnceLock::new();
                    let def = DEF_RE.get_or_init(|| Regex::new(r"^def\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(").unwrap());
                    if let Some(dc) = def.captures(l) { found = Some(dc[1].to_string()); }
                    break;
                }
                match found { Some(n) => n, None => continue }
            };
            out.push(ContractRow {
                contract_id: format!("rpc::{name}"),
                kind: "rpc".to_string(), role: "provider".to_string(),
                method: None, path: Some(name.clone()), topic: None,
                file: file.to_string(), line: (i + 1) as u32,
                language: "python".to_string(), framework: "jsonrpc".to_string(),
            });
        }
    }
    // JSON-RPC consumers — `"method": "name"` json keys.
    if text.contains("jsonrpc") {
        for (i, line) in text.lines().enumerate() {
            if let Some(caps) = jsonrpc_call_re().captures(line) {
                let name = caps[1].to_string();
                if name != "jsonrpc" {
                    out.push(ContractRow {
                        contract_id: format!("rpc::{name}"),
                        kind: "rpc".to_string(), role: "consumer".to_string(),
                        method: None, path: Some(name.clone()), topic: None,
                        file: file.to_string(), line: (i + 1) as u32,
                        language: "python".to_string(), framework: "jsonrpc".to_string(),
                    });
                }
            }
        }
    }
    // GraphQL clients in Python (gql / graphql-core).
    emit_graphql_client_rows(file, text, "python", &mut out);
    // Celery task providers (multi-line lookup for @app.task + def name)
    emit_celery_provider_rows(file, text, &mut out);
    // Celery / RQ enqueuers per-line
    for (i, line) in text.lines().enumerate() {
        // Celery .delay() / .apply_async()
        if let Some(caps) = celery_call_re().captures(line) {
            let name = caps[1].to_string();
            push_task_row(&mut out, file, (i + 1) as u32, &name, "consumer", "python", "celery");
            continue;
        }
        // RQ queue.enqueue('module.task', ...)
        if let Some(caps) = rq_enqueue_re().captures(line) {
            let name = caps[1].to_string();
            push_task_row(&mut out, file, (i + 1) as u32, &name, "consumer", "python", "rq");
            continue;
        }
    }
    // Redis / NATS pub-sub for Python files.
    emit_pubsub_rows(file, text, "python", &mut out);
    // SQS / SNS / EventBridge / GCP Pub/Sub / AMQP — boto3, pika, etc.
    emit_cloud_queue_rows(file, text, "python", &mut out);
    // DB table contracts (SQLAlchemy / Django / Alembic / Mongo / raw SQL).
    emit_db_table_rows(file, text, "python", &mut out);
    out
}
