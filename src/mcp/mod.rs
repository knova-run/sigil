//! `sigil mcp` — native Model Context Protocol server.
//!
//! Exposes sigil's structural code intelligence as a small,
//! deterministic, zero-LLM-dependency MCP tool surface. Each tool
//! handler is a thin orchestration over the same query primitives that
//! back the CLI — no new analysis code lives here.
//!
//! Layered into two parts:
//!   * `tools` — pure functions over `Index` that produce JSON values.
//!     Easy to unit-test without spinning up an MCP transport.
//!   * `server` — hand-rolled JSON-RPC 2.0 stdio server that wires
//!     the pure tools into MCP `tools/call` requests. The MCP stdio
//!     surface is small enough that we ship the protocol directly
//!     (no rmcp / tokio dependency); SSE transport is a follow-up.

pub mod server;
pub mod tools;
