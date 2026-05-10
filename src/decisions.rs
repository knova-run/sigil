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

use anyhow::Result;
use serde::Serialize;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Serialize, PartialEq)]
pub struct DecisionMarker {
    pub file: String,
    pub line: u32,
    pub marker: String,
    pub text: String,
    /// Provenance of the marker. `None` means inline-source (the original
    /// behavior — kept off the wire so older consumers parse unchanged).
    /// `Some("commit_message")` means we lifted it from a git commit
    /// body via `--include-git-history`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<&'static str>,
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
                source: None,
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
    let bytes = trimmed.as_bytes();
    for marker in MARKERS {
        let m_len = marker.len();
        if bytes.len() < m_len + 1 {
            continue;
        }
        // Markers are ASCII; if the prefix bytes aren't ASCII, the head can't
        // match. Bail before any &str slicing — that would panic on a UTF-8
        // boundary (e.g. an em-dash in a doc comment).
        if !bytes[..m_len].is_ascii() {
            continue;
        }
        let head = &trimmed[..m_len];
        if head.eq_ignore_ascii_case(marker) && bytes[m_len] == b':' {
            // Safe: m_len + 1 is past an ASCII byte, so on a char boundary.
            let body = trimmed[m_len + 1..].trim();
            if !body.is_empty() {
                return Some((marker, body));
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

/// Commit-message markers we lift via `--include-git-history`.
///
/// Distinct from the inline-comment marker set: developers don't write
/// `# ADR:` in commits, they write `Why:` or `Decision:` or `Refactor for:`.
/// Matching only the prefixes a human would actually use in a commit body
/// keeps false positives near zero.
const COMMIT_MARKERS: &[&str] =
    &["Why", "Decision", "Tradeoff", "Refactor for", "Rationale"];

/// Walk git history for `root`, scanning each commit body for
/// `^<Marker>:\s+<text>$` lines. Each match yields one DecisionMarker
/// with `source = "commit_message"`. Falls back to an empty vec if `root`
/// is not a git repo or git is unavailable — the caller treats commit
/// archaeology as a strict opt-in via `--include-git-history`.
pub fn extract_from_git_history(root: &Path) -> Result<Vec<DecisionMarker>> {
    // Record-separator-delimited records so multi-line bodies stay intact.
    // %H sha · newline · body · blank line · file-list (from --name-only).
    const RECORD_SEP: &str = "\x1e";
    let output = Command::new("git")
        .args([
            "log",
            "--no-merges",
            &format!("--pretty=format:{}%H%n%B", RECORD_SEP),
            "--name-only",
        ])
        .current_dir(root)
        .output()?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    let mut out = Vec::new();
    for record in text.split(RECORD_SEP) {
        if record.trim().is_empty() {
            continue;
        }
        // First line is the sha. The remainder is body + blank line +
        // changed files (from --name-only). We don't need the sha for the
        // emitted row — we surface the first changed file as `file`, or
        // "HEAD" if the commit has none.
        let mut lines = record.lines();
        let _sha = lines.next().unwrap_or("");
        let rest: Vec<&str> = lines.collect();
        // Split rest into (body, files) at the last blank line, same
        // trick as log_significant. Walks from the end so a body with
        // its own blank paragraphs survives.
        let boundary = rest.iter().rposition(|l| l.trim().is_empty());
        let (body_lines, file_lines): (&[&str], &[&str]) = match boundary {
            Some(idx) => (&rest[..idx], &rest[idx + 1..]),
            None => (&rest[..], &[]),
        };
        let first_file = file_lines
            .iter()
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .unwrap_or("HEAD")
            .to_string();
        for line in body_lines {
            if let Some((marker, text)) = scan_commit_line(line) {
                out.push(DecisionMarker {
                    file: first_file.clone(),
                    line: 0,
                    marker: marker.to_string(),
                    text: text.to_string(),
                    source: Some("commit_message"),
                });
            }
        }
    }
    Ok(out)
}

/// Match a single commit-message line against the COMMIT_MARKERS list.
/// Marker prefix is matched case-insensitively against the start of the
/// (trimmed) line; the marker is canonicalized to its source-cased form.
fn scan_commit_line(line: &str) -> Option<(&'static str, &str)> {
    let trimmed = line.trim_start();
    let bytes = trimmed.as_bytes();
    for marker in COMMIT_MARKERS {
        let m_len = marker.len();
        if bytes.len() < m_len + 1 {
            continue;
        }
        if !bytes[..m_len].is_ascii() {
            continue;
        }
        let head = &trimmed[..m_len];
        if head.eq_ignore_ascii_case(marker) && bytes[m_len] == b':' {
            let body = trimmed[m_len + 1..].trim();
            if !body.is_empty() {
                return Some((marker, body));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_line_matches_why_decision_tradeoff_refactor_rationale() {
        for prefix in &["Why", "Decision", "Tradeoff", "Refactor for", "Rationale"] {
            let line = format!("{}: keep the FK on user_id for now", prefix);
            let (marker, text) = scan_commit_line(&line).expect(prefix);
            assert_eq!(marker, *prefix);
            assert_eq!(text, "keep the FK on user_id for now");
        }
    }

    #[test]
    fn commit_line_is_case_insensitive() {
        let (marker, _) = scan_commit_line("WHY: legacy callers depend on it").unwrap();
        assert_eq!(marker, "Why");
    }

    #[test]
    fn commit_line_rejects_non_markers() {
        assert!(scan_commit_line("Just a normal commit body line").is_none());
        assert!(scan_commit_line("Wherefore: not a recognized marker").is_none());
        assert!(scan_commit_line("Why").is_none()); // no colon
        assert!(scan_commit_line("Why:").is_none()); // empty body
    }
}

