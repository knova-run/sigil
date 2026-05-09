//! Lightweight security signal extractor — regex pattern scan.
//!
//! Mirrors repowise's `_PATTERNS` registry: detects calls to `eval`, `exec`,
//! `pickle.loads`, `os.system`, `subprocess(…shell=True)`; hardcoded passwords
//! and api keys; raw-SQL concat / f-string interpolation; `verify=False`
//! TLS bypass; weak hashes (md5/sha1).
//!
//! Pure regex, no LLM. Outputs JSONL: { file, line, kind, severity }.

use regex::Regex;
use serde::Serialize;
use std::path::Path;
use std::sync::OnceLock;

#[derive(Debug, Serialize, PartialEq)]
pub struct SecurityFinding {
    pub file: String,
    pub line: u32,
    pub kind: String,
    pub severity: String,
}

struct Pattern {
    re: Regex,
    kind: &'static str,
    severity: &'static str,
}

fn patterns() -> &'static Vec<Pattern> {
    static PATS: OnceLock<Vec<Pattern>> = OnceLock::new();
    PATS.get_or_init(|| {
        vec![
            // High-severity: arbitrary-code execution and credential leakage.
            Pattern { re: Regex::new(r"\beval\s*\(").unwrap(), kind: "eval_call", severity: "high" },
            Pattern { re: Regex::new(r"\bexec\s*\(").unwrap(), kind: "exec_call", severity: "high" },
            Pattern { re: Regex::new(r"pickle\.loads").unwrap(), kind: "pickle_loads", severity: "high" },
            Pattern { re: Regex::new(r"subprocess\..*shell\s*=\s*True").unwrap(), kind: "subprocess_shell_true", severity: "high" },
            Pattern { re: Regex::new(r"\bos\.system").unwrap(), kind: "os_system", severity: "high" },
            Pattern { re: Regex::new(r#"password\s*=\s*['"]"#).unwrap(), kind: "hardcoded_password", severity: "high" },
            Pattern { re: Regex::new(r#"(?:api_?key|secret)\s*=\s*['"]"#).unwrap(), kind: "hardcoded_secret", severity: "high" },
            // Medium severity: SQL-injection-shaped patterns and disabled TLS.
            Pattern { re: Regex::new(r#"f['"].*SELECT.*\{.*\}"#).unwrap(), kind: "fstring_sql", severity: "med" },
            Pattern { re: Regex::new(r#"\.execute\(\s*['"]\s*SELECT.*\+"#).unwrap(), kind: "concat_sql", severity: "med" },
            Pattern { re: Regex::new(r"verify\s*=\s*False").unwrap(), kind: "tls_verify_false", severity: "med" },
            // Low severity: weak crypto. Still surfaced — md5/sha1 in 2026 is a code smell.
            Pattern { re: Regex::new(r"\bmd5\b|\bsha1\b").unwrap(), kind: "weak_hash", severity: "low" },
        ]
    })
}

/// Walk `root` and collect all security findings across scanned files.
pub fn scan_root(root: &Path) -> Vec<SecurityFinding> {
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<SecurityFinding>) {
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
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().to_string();
        out.extend(scan_text(&rel, &text));
    }
}

fn is_skipped_dir(path: &Path) -> bool {
    let name = match path.file_name() { Some(n) => n.to_string_lossy(), None => return false };
    matches!(
        name.as_ref(),
        ".git" | "node_modules" | "__pycache__" | ".venv" | "venv" | "target"
            | "dist" | "build" | "vendor" | ".sigil"
    )
}

fn is_scanned_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("py" | "js" | "ts" | "tsx" | "jsx" | "rb" | "go" | "java" | "cs" | "rs" | "php")
    )
}

/// Scan an in-memory source text. Public so callers can run the patterns
/// against synthetic fixtures or non-filesystem inputs.
pub fn scan_text(file: &str, source: &str) -> Vec<SecurityFinding> {
    let pats = patterns();
    let mut out = Vec::new();
    for (i, line) in source.lines().enumerate() {
        for p in pats {
            if p.re.is_match(line) {
                out.push(SecurityFinding {
                    file: file.to_string(),
                    line: (i + 1) as u32,
                    kind: p.kind.to_string(),
                    severity: p.severity.to_string(),
                });
            }
        }
    }
    out
}
