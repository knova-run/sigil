//! MCP stdio server. Implements the Model Context Protocol JSON-RPC
//! surface directly (no external SDK dependency) — the protocol is
//! small enough that hand-rolling it is cheaper than pulling in async
//! + rmcp + tokio.
//!
//! Protocol summary:
//!   * Client sends `initialize` → server returns capabilities + tool
//!     descriptors.
//!   * Client sends `notifications/initialized` → server ignores
//!     (notifications carry no `id` and don't get a response).
//!   * Client sends `tools/list` → server returns the same descriptors
//!     it already returned during `initialize` (some clients re-list).
//!   * Client sends `tools/call` with `{name, arguments}` → server
//!     dispatches to the matching pure tool fn in `super::tools` and
//!     returns the JSON result inside an MCP `content` block.
//!
//! Transport is JSON-RPC 2.0, one message per line on stdin/stdout.
//! That's how Claude Code / Cursor / Cline launch local MCP servers
//! today — the harness pipes stdin/stdout to the binary.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use crate::query::index::Index;

/// MCP protocol version we speak. Clients send a version string in
/// `initialize`; servers reply with the version they actually
/// implement. Picking the spec date as the version is the standard
/// convention.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Per-connection state. Captures capabilities the client declared in
/// its `initialize` request — most importantly whether it supports
/// MCP `sampling/createMessage` (issue #41).
#[derive(Debug, Default, Clone)]
pub struct ServerState {
    pub supports_sampling: bool,
}

/// Load the index from `.sigil/` under `root` and run the JSON-RPC
/// stdio loop. Returns on EOF (i.e. when the client closes stdin).
pub fn run_stdio(root: PathBuf) -> Result<()> {
    let idx = load_index(&root).context("load index")?;
    let rank = crate::map::load_rank_manifest(&root).unwrap_or_default();
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let mut state = ServerState::default();
    for line in stdin.lock().lines() {
        let line = line.context("read stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        let response = handle_message(&line, &root, &idx, &rank, &mut state);
        if let Some(resp) = response {
            writeln!(stdout, "{}", resp).context("write stdout")?;
            stdout.flush().context("flush stdout")?;
        }
    }
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

/// Parse one JSON-RPC message and return a serialized response. None
/// when the message is a notification (no `id` → no response per the
/// JSON-RPC spec).
pub fn handle_message(
    line: &str,
    root: &std::path::Path,
    idx: &Index,
    rank: &crate::rank::RankManifest,
    state: &mut ServerState,
) -> Option<String> {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return Some(error_response(
                None,
                -32700,
                &format!("parse error: {}", e),
            ));
        }
    };

    let id = req.get("id").cloned();
    let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");

    // Notifications carry no id and never get a response.
    let is_notification = id.is_none();

    let result = dispatch(method, req.get("params"), root, idx, rank, state);

    if is_notification {
        return None;
    }

    match result {
        Ok(value) => Some(
            serde_json::to_string(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": value,
            }))
            .expect("response serializes infallibly"),
        ),
        Err(McpError { code, message }) => Some(error_response(id, code, &message)),
    }
}

struct McpError {
    code: i32,
    message: String,
}

fn dispatch(
    method: &str,
    params: Option<&Value>,
    root: &std::path::Path,
    idx: &Index,
    rank: &crate::rank::RankManifest,
    state: &mut ServerState,
) -> Result<Value, McpError> {
    match method {
        "initialize" => {
            // Capture client capabilities — most importantly whether
            // they support `sampling/createMessage` for #41. The
            // `capabilities.sampling` key being present means yes
            // (its value is an object that may carry sub-flags).
            if let Some(p) = params
                && let Some(caps) = p.get("capabilities")
                && caps.get("sampling").is_some()
            {
                state.supports_sampling = true;
            }
            Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "sigil",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }))
        }
        "notifications/initialized" => Ok(Value::Null),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_descriptors(state) })),
        "tools/call" => call_tool(params, root, idx, rank, state),
        _ => Err(McpError {
            code: -32601,
            message: format!("method not found: {}", method),
        }),
    }
}

fn call_tool(
    params: Option<&Value>,
    root: &std::path::Path,
    idx: &Index,
    rank: &crate::rank::RankManifest,
    state: &ServerState,
) -> Result<Value, McpError> {
    let params = params.ok_or_else(|| McpError {
        code: -32602,
        message: "tools/call requires params".to_string(),
    })?;
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| McpError {
            code: -32602,
            message: "tools/call params missing `name`".to_string(),
        })?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let payload: Value = match name {
        "sigil_search" => {
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .ok_or_else(|| McpError {
                    code: -32602,
                    message: "sigil_search requires `query` string".to_string(),
                })?;
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(25);
            super::tools::search(idx, query, limit)
        }
        "get_context" => {
            let targets: Vec<String> = args
                .get("targets")
                .and_then(|v| v.as_array())
                .ok_or_else(|| McpError {
                    code: -32602,
                    message: "get_context requires `targets` array".to_string(),
                })?
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            let opts = super::tools::ContextToolOptions {
                include_source: args
                    .get("include")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .any(|v| v.as_str() == Some("source"))
                    })
                    .unwrap_or(false),
                compact: args
                    .get("compact")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true),
                depth: args
                    .get("depth")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize)
                    .unwrap_or(10),
                budget: args
                    .get("budget")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize)
                    .unwrap_or(1500),
            };
            super::tools::context(idx, &targets, &opts)
        }
        "get_overview" => {
            let budget = args
                .get("budget")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(2500);
            super::tools::overview(idx, rank, budget)
        }
        "get_dead_code" => {
            let min_confidence = args
                .get("min_confidence")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.4);
            let include_internals = args
                .get("include_internals")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            super::tools::dead_code(root, idx, min_confidence, include_internals)
        }
        "get_why" => {
            let q = args.get("query").and_then(|v| v.as_str());
            super::tools::why(root, q)
        }
        "get_answer" => {
            let question = args
                .get("question")
                .and_then(|v| v.as_str())
                .ok_or_else(|| McpError {
                    code: -32602,
                    message: "get_answer requires `question` string".to_string(),
                })?;
            let max_targets = args
                .get("max_targets")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(8);
            let bundle = super::tools::answer_bundle(idx, root, question, max_targets);
            // Fallback path — without `sampling/createMessage`
            // round-trip support in this synchronous stdio handler,
            // the server returns the bundle and lets the agent
            // synthesize inline. The bundle carries the synthesis
            // prompt the client can use directly. When sampling is
            // supported, we mark that in the response so the client
            // knows it could chain a `sampling/createMessage` call
            // with `bundle.synthesis_prompt` — but the round-trip
            // itself is the caller's responsibility for now (a
            // synchronous server-initiated request mid-tool-call
            // requires interleaved stdio handling — tracked as
            // future work; the bundle-only path already covers the
            // sampling-incapable client case completely).
            let mut response =
                serde_json::to_value(&bundle).map_err(|e| McpError {
                    code: -32603,
                    message: format!("answer_bundle serialize: {}", e),
                })?;
            response["sampling_supported"] = json!(state.supports_sampling);
            if !state.supports_sampling {
                response["note"] = json!(
                    "client does not support sampling; synthesize from bundle.synthesis_prompt inline"
                );
            }
            response
        }
        other => {
            return Err(McpError {
                code: -32601,
                message: format!("unknown tool: {}", other),
            });
        }
    };

    // MCP tools/call wraps the response in a `content` block of typed
    // parts. We emit a single `text` part containing the JSON payload —
    // clients are expected to parse it.
    Ok(json!({
        "content": [
            {"type": "text", "text": serde_json::to_string(&payload).unwrap_or_default()}
        ],
        "isError": false,
    }))
}

fn tool_descriptors(state: &ServerState) -> Value {
    json!([
        {
            "name": "sigil_search",
            "description": "Symbol-aware search over the codebase index. Returns ranked entity matches with file, name, kind, line, and (when known) parent class + signature. Useful when you have a symbol name and need to locate its definition or check which class/module owns it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "default": 25}
                },
                "required": ["query"]
            }
        },
        {
            "name": "get_context",
            "description": "Structural context bundle per code symbol. Targets can be bare names, qualified forms like 'src/file.rs::Class::method', or bare file paths (per-file digest). Includes signature, doc, kind, line range, blast-radius numbers, callers, callees, and heritage. Batch multiple targets in one call.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "targets": {"type": "array", "items": {"type": "string"}},
                    "include": {
                        "type": "array",
                        "items": {"type": "string", "enum": ["source", "callers", "callees"]}
                    },
                    "compact": {"type": "boolean", "default": true},
                    "depth": {"type": "integer", "default": 10},
                    "budget": {"type": "integer", "default": 1500}
                },
                "required": ["targets"]
            }
        },
        {
            "name": "get_overview",
            "description": "High-level architecture map of the repository. Returns ranked files with top entities and subsystems detected via community detection. Useful as a first call on an unfamiliar codebase.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "budget": {"type": "integer", "default": 2500}
                }
            }
        },
        {
            "name": "get_dead_code",
            "description": "Framework-aware dead-code findings partitioned by confidence: `safe_to_delete` (>= 0.70) and `review_first` (< 0.70). Each finding has file, kind, line, confidence, and recent-activity signal.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "min_confidence": {"type": "number", "default": 0.4},
                    "include_internals": {"type": "boolean", "default": false}
                }
            }
        },
        {
            "name": "get_why",
            "description": "Architectural decision records mined from code annotations (WHY:, DECISION:, RATIONALE:, TRADEOFF:, ADR:, REJECTED:). Pass a free-text query for NL search, a file path for decisions in that file, or no args for all decisions.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                }
            }
        },
        {
            "name": "get_answer",
            "description": if state.supports_sampling {
                "Retrieval-augmented synthesis over the structural code index. Given a natural-language question, finds the most relevant symbols and architectural decisions, bundles their context, and provides a synthesis prompt the client can hand to its own model via MCP sampling. Sigil performs no LLM calls itself. Returns the bundle plus a `sampling_supported: true` flag — to get a synthesized answer, the client should pass `bundle.synthesis_prompt` through a `sampling/createMessage` request."
            } else {
                "Retrieval-augmented synthesis over the structural code index. Returns ranked entity bundles (signature, doc, callers/callees, heritage) plus a synthesis prompt — the calling agent synthesizes inline. The client does not advertise MCP sampling capability, so sigil cannot delegate the synthesis; the bundle-only fallback path is the only mode available."
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "question": {"type": "string"},
                    "max_targets": {"type": "integer", "default": 8}
                },
                "required": ["question"]
            }
        }
    ])
}

fn error_response(id: Option<Value>, code: i32, message: &str) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        }
    }))
    .expect("error response serializes infallibly")
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
            struct_hash: "deadbeef".to_string(),
            visibility: None,
            rank: None,
            blast_radius: None,
            doc: None,
            heritage: Vec::new(),
            alias: None,
        }
    }

    fn small_idx() -> Index {
        Index::build(
            vec![ent("src/lib.rs", "process_data", "function", 12)],
            vec![],
        )
    }

    #[test]
    fn initialize_returns_protocol_version_and_capabilities() {
        let idx = small_idx();
        let rank = crate::rank::RankManifest::default();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        });
        let mut state = ServerState::default();
        let resp = handle_message(
            &serde_json::to_string(&req).unwrap(),
            std::path::Path::new("."),
            &idx,
            &rank,
            &mut state,
        )
        .expect("initialize must respond");
        let v: Value = serde_json::from_str(&resp).expect("valid JSON");
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(v["result"]["capabilities"]["tools"].is_object());
        assert_eq!(v["result"]["serverInfo"]["name"], "sigil");
    }

    #[test]
    fn notifications_get_no_response() {
        let idx = small_idx();
        let rank = crate::rank::RankManifest::default();
        let req = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });
        let mut state = ServerState::default();
        let resp = handle_message(
            &serde_json::to_string(&req).unwrap(),
            std::path::Path::new("."),
            &idx,
            &rank,
            &mut state,
        );
        assert!(resp.is_none(), "notifications must not get a response");
    }

    #[test]
    fn tools_list_returns_all_six_tools() {
        let idx = small_idx();
        let rank = crate::rank::RankManifest::default();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        });
        let mut state = ServerState::default();
        let resp = handle_message(
            &serde_json::to_string(&req).unwrap(),
            std::path::Path::new("."),
            &idx,
            &rank,
            &mut state,
        )
        .expect("tools/list must respond");
        let v: Value = serde_json::from_str(&resp).expect("valid JSON");
        let tools = v["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 6);
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"sigil_search"));
        assert!(names.contains(&"get_context"));
        assert!(names.contains(&"get_overview"));
        assert!(names.contains(&"get_dead_code"));
        assert!(names.contains(&"get_why"));
        assert!(names.contains(&"get_answer"));
    }

    #[test]
    fn tools_call_sigil_search_returns_hits_via_content_block() {
        let idx = small_idx();
        let rank = crate::rank::RankManifest::default();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "sigil_search",
                "arguments": {"query": "process", "limit": 10}
            }
        });
        let mut state = ServerState::default();
        let resp = handle_message(
            &serde_json::to_string(&req).unwrap(),
            std::path::Path::new("."),
            &idx,
            &rank,
            &mut state,
        )
        .expect("tools/call must respond");
        let v: Value = serde_json::from_str(&resp).expect("valid JSON");
        let content = v["result"]["content"].as_array().expect("content array");
        assert_eq!(content[0]["type"], "text");
        let payload_str = content[0]["text"].as_str().expect("text payload");
        let payload: Value = serde_json::from_str(payload_str).expect("valid inner JSON");
        let hits = payload["hits"].as_array().expect("hits array");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["n"], "process_data");
    }

    #[test]
    fn unknown_method_returns_jsonrpc_method_not_found() {
        let idx = small_idx();
        let rank = crate::rank::RankManifest::default();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "no_such_method"
        });
        let mut state = ServerState::default();
        let resp = handle_message(
            &serde_json::to_string(&req).unwrap(),
            std::path::Path::new("."),
            &idx,
            &rank,
            &mut state,
        )
        .expect("must respond with error");
        let v: Value = serde_json::from_str(&resp).expect("valid JSON");
        assert_eq!(v["error"]["code"], -32601);
    }

    #[test]
    fn malformed_json_returns_parse_error() {
        let idx = small_idx();
        let rank = crate::rank::RankManifest::default();
        let mut state = ServerState::default();
        let resp = handle_message(
            "{not json}",
            std::path::Path::new("."),
            &idx,
            &rank,
            &mut state,
        )
        .expect("must respond with parse error");
        let v: Value = serde_json::from_str(&resp).expect("response is valid JSON");
        assert_eq!(v["error"]["code"], -32700);
    }

    #[test]
    fn initialize_captures_client_sampling_capability() {
        let idx = small_idx();
        let rank = crate::rank::RankManifest::default();
        let mut state = ServerState::default();
        assert!(!state.supports_sampling);
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "capabilities": {"sampling": {}}
            }
        });
        let _ = handle_message(
            &serde_json::to_string(&req).unwrap(),
            std::path::Path::new("."),
            &idx,
            &rank,
            &mut state,
        );
        assert!(state.supports_sampling, "sampling cap must be captured");
    }

    #[test]
    fn initialize_does_not_set_sampling_when_absent() {
        let idx = small_idx();
        let rank = crate::rank::RankManifest::default();
        let mut state = ServerState::default();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"capabilities": {}}
        });
        let _ = handle_message(
            &serde_json::to_string(&req).unwrap(),
            std::path::Path::new("."),
            &idx,
            &rank,
            &mut state,
        );
        assert!(!state.supports_sampling);
    }

    #[test]
    fn tools_list_includes_get_answer_with_six_tools_total() {
        let idx = small_idx();
        let rank = crate::rank::RankManifest::default();
        let mut state = ServerState::default();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": {}
        });
        let resp = handle_message(
            &serde_json::to_string(&req).unwrap(),
            std::path::Path::new("."),
            &idx,
            &rank,
            &mut state,
        )
        .expect("response");
        let v: Value = serde_json::from_str(&resp).unwrap();
        let tools = v["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 6);
        assert!(
            tools.iter().any(|t| t["name"] == "get_answer"),
            "get_answer must always be registered (fallback path)"
        );
    }

    #[test]
    fn get_answer_returns_bundle_with_sampling_supported_flag() {
        let idx = small_idx();
        let rank = crate::rank::RankManifest::default();
        let mut state = ServerState {
            supports_sampling: true,
        };
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "get_answer",
                "arguments": {"question": "how does process_data work?", "max_targets": 3}
            }
        });
        let resp = handle_message(
            &serde_json::to_string(&req).unwrap(),
            std::path::Path::new("."),
            &idx,
            &rank,
            &mut state,
        )
        .expect("response");
        let v: Value = serde_json::from_str(&resp).unwrap();
        let text = v["result"]["content"][0]["text"]
            .as_str()
            .expect("content text");
        let payload: Value = serde_json::from_str(text).expect("valid inner JSON");
        assert_eq!(payload["sampling_supported"], true);
        assert!(payload["candidates"].is_array());
        assert!(payload["synthesis_prompt"].is_string());
        assert!(payload.get("note").is_none(), "no note when sampling supported");
    }

    #[test]
    fn get_answer_returns_fallback_note_when_sampling_not_supported() {
        let idx = small_idx();
        let rank = crate::rank::RankManifest::default();
        let mut state = ServerState::default(); // sampling = false
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "get_answer",
                "arguments": {"question": "how does process_data work?"}
            }
        });
        let resp = handle_message(
            &serde_json::to_string(&req).unwrap(),
            std::path::Path::new("."),
            &idx,
            &rank,
            &mut state,
        )
        .expect("response");
        let v: Value = serde_json::from_str(&resp).unwrap();
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        let payload: Value = serde_json::from_str(text).unwrap();
        assert_eq!(payload["sampling_supported"], false);
        assert!(payload["note"].as_str().unwrap().contains("synthesize"));
    }
}
