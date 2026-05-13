//! MCP stdio server, built on the official Rust MCP SDK (`rmcp`).
//!
//! Each of the 6 tools is a `#[tool]`-annotated method on `SigilServer`
//! that defers to the pure functions in `super::tools`. The wire
//! protocol (JSON-RPC framing, `tools/list` synthesis from `#[tool]`
//! attributes, `tools/call` dispatch, content-block packing) is handled
//! by `#[tool_router]` + `#[tool_handler]`; we just write the handler
//! bodies.
//!
//! Per-connection state — most importantly the client's `sampling`
//! capability — is captured in `initialize()` and stored on the
//! `Arc<RwLock<…>>` field. `get_answer` reads it to decide whether to
//! emit the `sampling_supported: true` flag (and omit the
//! synthesize-inline fallback note) for sampling-capable clients.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, Implementation, InitializeRequestParam, InitializeResult,
    ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::transport::stdio;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use tokio::sync::RwLock;

use crate::query::index::Index;
use crate::rank::RankManifest;

/// Per-connection mutable state. `Arc<RwLock<...>>` so the handler
/// can be cloned cheaply into the rmcp dispatcher while still
/// supporting interior mutation from `initialize()`.
#[derive(Debug, Default)]
struct ConnState {
    supports_sampling: bool,
}

/// `sigil mcp` server.
///
/// All read-only data (the index + rank manifest + root path) lives
/// behind `Arc`s so this struct is cheap to clone — rmcp clones the
/// handler per request internally.
#[derive(Clone)]
pub struct SigilServer {
    root: Arc<PathBuf>,
    idx: Arc<Index>,
    rank: Arc<RankManifest>,
    state: Arc<RwLock<ConnState>>,
    tool_router: ToolRouter<SigilServer>,
}

// ── Tool argument types ─────────────────────────────────────────────
//
// Each `#[tool]` method gets a `Parameters<T>` arg, where `T` must be
// `Deserialize + JsonSchema`. The schemas surface in `tools/list` so
// clients can validate inputs before sending them.

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SearchArgs {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ContextArgs {
    targets: Vec<String>,
    #[serde(default)]
    include: Option<Vec<String>>,
    #[serde(default)]
    compact: Option<bool>,
    #[serde(default)]
    depth: Option<usize>,
    #[serde(default)]
    budget: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct OverviewArgs {
    #[serde(default)]
    budget: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DeadCodeArgs {
    #[serde(default)]
    min_confidence: Option<f64>,
    #[serde(default)]
    include_internals: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct WhyArgs {
    #[serde(default)]
    query: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AnswerArgs {
    question: String,
    #[serde(default)]
    max_targets: Option<usize>,
}

// ── Tool handlers ──────────────────────────────────────────────────

#[tool_router]
impl SigilServer {
    pub fn new(root: PathBuf, idx: Index, rank: RankManifest) -> Self {
        Self {
            root: Arc::new(root),
            idx: Arc::new(idx),
            rank: Arc::new(rank),
            state: Arc::new(RwLock::new(ConnState::default())),
            tool_router: Self::tool_router(),
        }
    }

    fn text_result(value: serde_json::Value) -> CallToolResult {
        CallToolResult::success(vec![Content::text(value.to_string())])
    }

    #[tool(description = "Symbol-aware search over the codebase index. Returns ranked entity matches with file, name, kind, line, and (when known) parent class + signature. Useful when you have a symbol name and need to locate its definition or check which class/module owns it.")]
    async fn sigil_search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let limit = args.limit.unwrap_or(25);
        let v = super::tools::search(&self.idx, &args.query, limit);
        Ok(Self::text_result(v))
    }

    #[tool(description = "Structural context bundle per code symbol. Targets can be bare names, qualified forms like 'src/file.rs::Class::method', or bare file paths (per-file digest). Includes signature, doc, kind, line range, blast-radius numbers, callers, callees, and heritage. Batch multiple targets in one call.")]
    async fn get_context(
        &self,
        Parameters(args): Parameters<ContextArgs>,
    ) -> Result<CallToolResult, McpError> {
        let opts = super::tools::ContextToolOptions {
            include_source: args
                .include
                .as_ref()
                .map(|arr| arr.iter().any(|s| s == "source"))
                .unwrap_or(false),
            compact: args.compact.unwrap_or(true),
            depth: args.depth.unwrap_or(10),
            budget: args.budget.unwrap_or(1500),
        };
        let v = super::tools::context(&self.idx, &self.root, &args.targets, &opts);
        Ok(Self::text_result(v))
    }

    #[tool(description = "High-level architecture map of the repository. Returns ranked files with top entities and subsystems detected via community detection. Useful as a first call on an unfamiliar codebase.")]
    async fn get_overview(
        &self,
        Parameters(args): Parameters<OverviewArgs>,
    ) -> Result<CallToolResult, McpError> {
        let budget = args.budget.unwrap_or(2500);
        let v = super::tools::overview(&self.idx, &self.rank, budget);
        Ok(Self::text_result(v))
    }

    #[tool(description = "Framework-aware dead-code findings partitioned by confidence: `safe_to_delete` (>= 0.85 — file-level and exported-orphan tier) and `review_first` (< 0.85 — internal helpers, higher false-positive rate). Each finding has file, kind, line, confidence, and recent-activity signal.")]
    async fn get_dead_code(
        &self,
        Parameters(args): Parameters<DeadCodeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let min_confidence = args.min_confidence.unwrap_or(0.4);
        let include_internals = args.include_internals.unwrap_or(false);
        let v = super::tools::dead_code(&self.root, &self.idx, min_confidence, include_internals);
        Ok(Self::text_result(v))
    }

    #[tool(description = "Architectural decision records mined from code annotations (WHY:, DECISION:, RATIONALE:, TRADEOFF:, ADR:, REJECTED:). Pass a free-text query for NL search, a file path for decisions in that file, or no args for all decisions.")]
    async fn get_why(
        &self,
        Parameters(args): Parameters<WhyArgs>,
    ) -> Result<CallToolResult, McpError> {
        let v = super::tools::why(&self.root, args.query.as_deref());
        Ok(Self::text_result(v))
    }

    #[tool(description = "Retrieval-augmented synthesis over the structural code index. Given a natural-language question, finds the most relevant symbols and architectural decisions, bundles their context, and provides a synthesis prompt the client can hand to its own model via MCP sampling. Sigil performs no LLM calls itself. When the client advertises sampling capability the response carries `sampling_supported: true`; otherwise a fallback `note` explains synthesize-inline behavior.")]
    async fn get_answer(
        &self,
        Parameters(args): Parameters<AnswerArgs>,
    ) -> Result<CallToolResult, McpError> {
        let max_targets = args.max_targets.unwrap_or(8);
        let bundle =
            super::tools::answer_bundle(&self.idx, &self.root, &args.question, max_targets);
        let mut response = serde_json::to_value(&bundle).unwrap_or(serde_json::Value::Null);
        let supports_sampling = self.state.read().await.supports_sampling;
        response["sampling_supported"] = serde_json::json!(supports_sampling);
        if !supports_sampling {
            response["note"] = serde_json::json!(
                "client does not support sampling; synthesize from bundle.synthesis_prompt inline"
            );
        }
        Ok(Self::text_result(response))
    }
}

#[tool_handler]
impl ServerHandler for SigilServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_server_info(Implementation::new("sigil", env!("CARGO_PKG_VERSION")))
    }

    async fn initialize(
        &self,
        params: InitializeRequestParam,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        // Capture client capabilities — most importantly whether the
        // client speaks `sampling/createMessage` (issue #41).
        if params.capabilities.sampling.is_some() {
            self.state.write().await.supports_sampling = true;
        }
        Ok(self.get_info().into())
    }
}

/// Load the index from `.sigil/` under `root` and run the rmcp
/// stdio server. Returns when the client closes stdin.
pub fn run_stdio(root: PathBuf) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    runtime.block_on(async { run_stdio_async(root).await })
}

async fn run_stdio_async(root: PathBuf) -> Result<()> {
    let idx = load_index(&root).context("load index")?;
    let rank = crate::map::load_rank_manifest(&root).unwrap_or_default();
    let server = SigilServer::new(root, idx, rank);
    let service = server.serve(stdio()).await.context("serve mcp stdio")?;
    service.waiting().await.context("mcp service wait")?;
    Ok(())
}

fn load_index(root: &std::path::Path) -> Result<Index> {
    let sigil_dir = root.join(".sigil");
    let entities_path = sigil_dir.join("entities.jsonl");
    let refs_path = sigil_dir.join("refs.jsonl");
    anyhow::ensure!(
        entities_path.exists(),
        "no .sigil/entities.jsonl at {} — run `sigil index` first",
        root.display()
    );

    let entities = read_jsonl(&entities_path)?;
    let refs = if refs_path.exists() {
        read_jsonl(&refs_path)?
    } else {
        Vec::new()
    };
    Ok(Index::build(entities, refs))
}

fn read_jsonl<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Result<Vec<T>> {
    let s = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in s.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let row: T = serde_json::from_str(line)
            .with_context(|| format!("{}:line {} parse", path.display(), i + 1))?;
        out.push(row);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::Entity;

    fn ent(file: &str, name: &str, kind: &str, line: u32) -> Entity {
        Entity {
            file: file.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            line_start: line,
            line_end: line,
            parent: None,
            qualified_name: None,
            sig: None,
            meta: None,
            body_hash: None,
            sig_hash: None,
            struct_hash: "0123456789abcdef".to_string(),
            visibility: None,
            rank: None,
            blast_radius: None,
            doc: None,
            heritage: Vec::new(),
            alias: None,
        }
    }

    fn small_server() -> SigilServer {
        let idx = Index::build(
            vec![ent("src/lib.rs", "process_data", "function", 12)],
            vec![],
        );
        SigilServer::new(PathBuf::from("."), idx, RankManifest::default())
    }

    #[test]
    fn server_info_advertises_tool_capability_and_protocol_version() {
        let server = small_server();
        let info = server.get_info();
        assert_eq!(info.protocol_version, ProtocolVersion::V_2024_11_05);
        assert!(
            info.capabilities.tools.is_some(),
            "server must advertise `tools` capability"
        );
        assert_eq!(info.server_info.name, "sigil");
    }

    #[test]
    fn tool_router_registers_all_six_tools() {
        let server = small_server();
        // The tool_router exposes a `list_all` of tool definitions
        // matching the `#[tool]` annotations.
        let names: Vec<String> = server
            .tool_router
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(names.len(), 6);
        for expected in [
            "sigil_search",
            "get_context",
            "get_overview",
            "get_dead_code",
            "get_why",
            "get_answer",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "missing tool: {expected} (got {names:?})"
            );
        }
    }

    #[tokio::test]
    async fn sigil_search_returns_text_content_with_hits_payload() {
        let server = small_server();
        let result = server
            .sigil_search(Parameters(SearchArgs {
                query: "process".to_string(),
                limit: Some(10),
            }))
            .await
            .expect("tool call should succeed");
        assert_eq!(result.is_error, Some(false));
        let text = match &result.content[0].raw {
            rmcp::model::RawContent::Text(t) => t.text.clone(),
            other => panic!("expected text content, got {:?}", other),
        };
        let payload: serde_json::Value =
            serde_json::from_str(&text).expect("text payload is JSON");
        let hits = payload["hits"].as_array().expect("hits array");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["n"], "process_data");
    }

    #[tokio::test]
    async fn get_answer_omits_note_when_sampling_supported() {
        let server = small_server();
        server.state.write().await.supports_sampling = true;
        let result = server
            .get_answer(Parameters(AnswerArgs {
                question: "how does process_data work?".to_string(),
                max_targets: Some(3),
            }))
            .await
            .expect("tool call");
        let text = match &result.content[0].raw {
            rmcp::model::RawContent::Text(t) => t.text.clone(),
            _ => panic!("expected text"),
        };
        let payload: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(payload["sampling_supported"], true);
        assert!(payload.get("note").is_none());
    }

    #[tokio::test]
    async fn get_answer_includes_fallback_note_when_sampling_absent() {
        let server = small_server();
        // default: supports_sampling = false
        let result = server
            .get_answer(Parameters(AnswerArgs {
                question: "what does process_data do?".to_string(),
                max_targets: None,
            }))
            .await
            .expect("tool call");
        let text = match &result.content[0].raw {
            rmcp::model::RawContent::Text(t) => t.text.clone(),
            _ => panic!("expected text"),
        };
        let payload: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(payload["sampling_supported"], false);
        assert!(
            payload["note"]
                .as_str()
                .unwrap_or_default()
                .contains("synthesize")
        );
    }
}
