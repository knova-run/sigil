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
//! # ADR: <text>
//! # REJECTED: <text>
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
    /// 1-indexed source line for inline-comment rows. `None` for
    /// commit-message rows, which carry no file-line context. Elided from
    /// the wire when absent so consumers don't have to special-case a `0`
    /// sentinel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
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
                line: Some((i + 1) as u32),
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
/// Skips common dependency / build directories. Output is sorted by
/// `(file, line, marker, source)` so the JSONL emit order is stable
/// across filesystems and across `--include-git-history` merges.
pub fn extract_from_root(root: &Path) -> Vec<DecisionMarker> {
    let mut out = Vec::new();
    walk(root, root, &mut out);
    sort_markers(&mut out);
    out
}

/// Deterministic sort over decision markers — `(file, line, marker, source)`.
/// Inline rows (line = `Some(N)`) precede commit rows (line = `None`) on
/// the same file. Rust's default `Option` ordering puts `None` first; we
/// invert that here so the natural reading order is "lines 1..N then
/// commit-message provenance" rather than the reverse.
pub fn sort_markers(markers: &mut [DecisionMarker]) {
    markers.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| line_order(a.line).cmp(&line_order(b.line)))
            .then_with(|| a.marker.cmp(&b.marker))
            .then_with(|| a.source.cmp(&b.source))
    });
}

/// Map `Option<u32>` to a sortable `(presence, value)` tuple where
/// inline rows (`Some`) sort before commit rows (`None`). `(0, n)` precedes
/// `(1, 0)` lexicographically, which is what we want.
fn line_order(line: Option<u32>) -> (u8, u32) {
    match line {
        Some(n) => (0, n),
        None => (1, 0),
    }
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
    // Records are RS-delimited. Inside a record we use a US byte (\x1f)
    // immediately after %B to mark the end of the commit body — git's own
    // `\n\n` separator between body and `--name-only` paths is ambiguous
    // when the body itself contains paragraph breaks (especially on
    // `--allow-empty` commits with no trailing path list).
    let output = Command::new("git")
        .args([
            "log",
            "--no-merges",
            &format!(
                "--pretty=format:{}%H%n%B{}",
                RECORD_SEP, BODY_END_SEP
            ),
            "--name-only",
        ])
        .current_dir(root)
        .output()?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(parse_git_log(&String::from_utf8_lossy(&output.stdout)))
}

/// Record-separator between commits in our git format string.
const RECORD_SEP: &str = "\x1e";
/// Unit-separator placed by the format string immediately after %B so the
/// body/paths split is unambiguous regardless of body shape or trailing
/// `--name-only` content.
const BODY_END_SEP: &str = "\x1f";

/// Parse the raw stdout of the `git log` command issued by
/// `extract_from_git_history`. Pure function so the body/paths-boundary
/// logic is testable without a temp git repo.
fn parse_git_log(text: &str) -> Vec<DecisionMarker> {
    let mut out = Vec::new();
    for record in text.split(RECORD_SEP) {
        if record.trim().is_empty() {
            continue;
        }
        // Split body from paths at the explicit BODY_END_SEP. If the
        // sentinel is missing (shouldn't happen for our format string),
        // treat the whole record as the body and emit no path attribution.
        let (header, paths_blob) = match record.split_once(BODY_END_SEP) {
            Some((h, p)) => (h, p),
            None => (record, ""),
        };
        let mut header_lines = header.lines();
        let _sha = header_lines.next().unwrap_or("");
        let body_lines: Vec<&str> = header_lines.collect();
        let first_file = paths_blob
            .lines()
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .unwrap_or("HEAD")
            .to_string();
        for line in body_lines {
            if let Some((marker, text)) = scan_commit_line(line) {
                out.push(DecisionMarker {
                    file: first_file.clone(),
                    line: None,
                    marker: marker.to_string(),
                    text: text.to_string(),
                    source: Some("commit_message"),
                });
            }
        }
    }
    out
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

    #[test]
    fn parse_git_log_attributes_empty_commits_to_head() {
        // `--allow-empty` commits emit no paths from `--name-only`. With the
        // previous "rposition of last blank line" heuristic, a paragraph
        // break inside the body was misread as the body/paths boundary,
        // causing later body text to be emitted as a fake `file` value.
        // The BODY_END_SEP sentinel pins the split unambiguously.
        let text = format!(
            "{rs}deadbeef\nWhy: we chose JWT for compat\n\nSome additional context paragraph.{us}",
            rs = RECORD_SEP,
            us = BODY_END_SEP,
        );
        let rows = parse_git_log(&text);
        assert_eq!(rows.len(), 1, "exactly one Why marker expected, got {rows:?}");
        assert_eq!(
            rows[0].file, "HEAD",
            "empty commit should attribute to HEAD, not a body paragraph; got {:?}",
            rows[0].file,
        );
        assert_eq!(rows[0].marker, "Why");
        assert_eq!(rows[0].text, "we chose JWT for compat");
    }

    #[test]
    fn commit_message_rows_omit_line_field() {
        // `line` is the source-file line number for inline rows. Commit
        // bodies have no such number, so commit-derived rows must elide
        // the field rather than emit a `0` sentinel that consumers would
        // confusingly have to special-case.
        let text = format!(
            "{rs}deadbeef\nWhy: keep JWT for compat{us}\nsrc/auth.py",
            rs = RECORD_SEP,
            us = BODY_END_SEP,
        );
        let rows = parse_git_log(&text);
        assert_eq!(rows.len(), 1);
        let json = serde_json::to_value(&rows[0]).unwrap();
        assert!(
            json.get("line").is_none(),
            "commit-derived row should omit `line` (no source-file context); got {json:?}"
        );
    }

    #[test]
    fn inline_rows_keep_line_field() {
        let rows = extract_from_text("auth.py", "# DECISION: keep JWT\n");
        assert_eq!(rows.len(), 1);
        let json = serde_json::to_value(&rows[0]).unwrap();
        assert_eq!(
            json["line"].as_u64(),
            Some(1),
            "inline rows must preserve the 1-indexed source line on the wire; got {json:?}"
        );
    }

    #[test]
    fn sort_markers_orders_by_file_then_line_with_inline_before_commit() {
        let mut rows = vec![
            DecisionMarker {
                file: "z.rs".into(),
                line: Some(5),
                marker: "WHY".into(),
                text: "z why inline".into(),
                source: None,
            },
            DecisionMarker {
                file: "a.rs".into(),
                line: None,
                marker: "Why".into(),
                text: "a why commit".into(),
                source: Some("commit_message"),
            },
            DecisionMarker {
                file: "a.rs".into(),
                line: Some(10),
                marker: "DECISION".into(),
                text: "a decision inline".into(),
                source: None,
            },
            DecisionMarker {
                file: "a.rs".into(),
                line: Some(2),
                marker: "WHY".into(),
                text: "a why inline".into(),
                source: None,
            },
        ];
        sort_markers(&mut rows);
        let order: Vec<(&str, Option<u32>, &str)> = rows
            .iter()
            .map(|m| (m.file.as_str(), m.line, m.marker.as_str()))
            .collect();
        assert_eq!(
            order,
            vec![
                ("a.rs", Some(2), "WHY"),
                ("a.rs", Some(10), "DECISION"),
                ("a.rs", None, "Why"),
                ("z.rs", Some(5), "WHY"),
            ],
        );
    }

    #[test]
    fn parse_git_log_keeps_first_changed_file_for_normal_commits() {
        let text = format!(
            "{rs}cafef00d\nDecision: keep the bearer-token shape{us}\nsrc/auth.py\nsrc/api.py",
            rs = RECORD_SEP,
            us = BODY_END_SEP,
        );
        let rows = parse_git_log(&text);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].file, "src/auth.py");
        assert_eq!(rows[0].marker, "Decision");
    }
}

