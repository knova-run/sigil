//! Decision marker extraction from source-file comments.
//!
//! Scans source files for architectural-decision anchors written as
//! line comments. Recognizes the canonical markers:
//!
//! ```text
//! # DECISION: <text>
//! # WHY: <text>
//! # RATIONALE: <text>
//! # TRADEOFF: <text>
//! ```
//!
//! and the same forms with `//` (Rust/Go/JS/TS/Java/C/C++/C#) and `--` (Lua/SQL).
//! Output is JSONL — one row per match — designed to be ingested into the
//! Knova runner's decision intelligence layer (the read side of `get_why`).

use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, PartialEq)]
pub struct DecisionMarker {
    pub file: String,
    pub line: u32,
    pub marker: String,
    pub text: String,
}

// Marker set matches repowise's MARKER_RE so the same source produces an
// identical decision-row set under either extractor.
const MARKERS: &[&str] = &[
    "DECISION", "WHY", "RATIONALE", "TRADEOFF", "ADR", "REJECTED",
];

/// Extract decision markers from a single file's source text.
pub fn extract_from_text(file_label: &str, source: &str) -> Vec<DecisionMarker> {
    let mut out = Vec::new();
    for (i, line) in source.lines().enumerate() {
        if let Some((marker, text)) = scan_line(line) {
            out.push(DecisionMarker {
                file: file_label.to_string(),
                line: (i + 1) as u32,
                marker: marker.to_string(),
                text: text.to_string(),
            });
        }
    }
    out
}

fn scan_line(line: &str) -> Option<(&'static str, &str)> {
    let stripped = line.trim_start();
    // Strip the comment prefix.
    let after_prefix = if let Some(rest) = stripped.strip_prefix("#") {
        rest
    } else if let Some(rest) = stripped.strip_prefix("//") {
        rest
    } else if let Some(rest) = stripped.strip_prefix("--") {
        rest
    } else {
        return None;
    };
    let trimmed = after_prefix.trim_start();
    for marker in MARKERS {
        // Accept "MARKER:" (case-insensitive).
        if trimmed.len() >= marker.len() + 1 {
            let head = &trimmed[..marker.len()];
            if head.eq_ignore_ascii_case(marker)
                && trimmed.as_bytes()[marker.len()] == b':'
            {
                let body = trimmed[marker.len() + 1..].trim();
                if !body.is_empty() {
                    return Some((marker, body));
                }
            }
        }
    }
    None
}

/// Walk `root` and return all decision markers found in source files.
/// Skips common dependency / build directories.
pub fn extract_from_root(root: &Path) -> Vec<DecisionMarker> {
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<DecisionMarker>) {
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
        if !is_scanned_file(&path) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().to_string();
        out.extend(extract_from_text(&rel, &text));
    }
}

fn is_skipped_dir(path: &Path) -> bool {
    let name = match path.file_name() {
        Some(n) => n.to_string_lossy(),
        None => return false,
    };
    matches!(
        name.as_ref(),
        ".git"
            | "node_modules"
            | "__pycache__"
            | ".venv"
            | "venv"
            | "target"
            | "dist"
            | "build"
            | "vendor"
            | ".sigil"
            | ".repowise-workspace"
    )
}

fn is_scanned_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some(
            "py" | "rs" | "go" | "js" | "ts" | "tsx" | "jsx" | "java"
                | "c" | "cc" | "cpp" | "cxx" | "h" | "hpp" | "cs"
                | "rb" | "lua" | "sql"
        )
    )
}

/// Helper for tests / CLI: collect from arbitrary paths into PathBufs.
pub fn _phantom() -> PathBuf {
    PathBuf::new()
}
