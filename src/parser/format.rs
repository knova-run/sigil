// Vendored from codeix v0.5.0 (Apache-2.0 / MIT), src/index/format.rs.
// See src/parser/NOTICE for attribution.
//
// These are the internal types emitted by the tree-sitter parser modules.
// Sigil translates them into its own on-disk `Entity` / `Reference` schema
// in src/index.rs.

use serde::{Deserialize, Serialize};

/// One line in a symbols export — a symbol extracted from the AST.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolEntry {
    pub file: String,
    pub name: String,
    pub kind: String,
    pub line: [u32; 2],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    /// Parser-provided signature text. When `Some`, `index.rs` uses this
    /// verbatim instead of running its own line-range signature extractor —
    /// useful for entities like constants where the "signature" is the
    /// literal RHS value, not the surrounding declaration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project: String,
}

/// A text block (docstring, comment, etc.) extracted from the AST.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextEntry {
    pub file: String,
    pub kind: String,
    pub line: [u32; 2],
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project: String,
}

/// A reference to a symbol (call, import, type annotation, instantiation, definition).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferenceEntry {
    pub file: String,
    pub name: String,
    pub kind: String,
    pub line: [u32; 2],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project: String,
}
