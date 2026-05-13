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
//!   * `server` — rmcp `ServerHandler` impl that wires the pure tools
//!     into MCP `tools/call` requests. Stdio transport only for now;
//!     SSE is a follow-up.

pub mod server;
pub mod tools;
