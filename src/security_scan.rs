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
    /// Extensions this pattern is meaningful for. `None` = any scanned
    /// language. `Some(&[...])` = only those extensions. Keeps Python-shaped
    /// idioms like `eval(` and `exec(` from misfiring on Rust method names
    /// where `cmd.exec(...)` is routine and harmless.
    langs: Option<&'static [&'static str]>,
}

fn patterns() -> &'static Vec<Pattern> {
    static PATS: OnceLock<Vec<Pattern>> = OnceLock::new();
    PATS.get_or_init(|| {
        const DYN_LANGS: &[&str] = &["py", "js", "ts", "tsx", "jsx", "rb", "php"];
        const PY: &[&str] = &["py"];
        const PY_RB: &[&str] = &["py", "rb"];
        vec![
            // High-severity: arbitrary-code execution and credential leakage.
            // eval/exec are dangerous in dynamic langs; in Rust/Go/Java/C# the
            // bare token is a method name with no security meaning.
            Pattern { re: Regex::new(r"\beval\s*\(").unwrap(), kind: "eval_call", severity: "high", langs: Some(DYN_LANGS) },
            Pattern { re: Regex::new(r"\bexec\s*\(").unwrap(), kind: "exec_call", severity: "high", langs: Some(DYN_LANGS) },
            Pattern { re: Regex::new(r"pickle\.loads").unwrap(), kind: "pickle_loads", severity: "high", langs: Some(PY) },
            Pattern { re: Regex::new(r"subprocess\..*shell\s*=\s*True").unwrap(), kind: "subprocess_shell_true", severity: "high", langs: Some(PY) },
            Pattern { re: Regex::new(r"\bos\.system").unwrap(), kind: "os_system", severity: "high", langs: Some(PY_RB) },
            Pattern { re: Regex::new(r#"password\s*=\s*['"]"#).unwrap(), kind: "hardcoded_password", severity: "high", langs: None },
            Pattern { re: Regex::new(r#"(?:api_?key|secret)\s*=\s*['"]"#).unwrap(), kind: "hardcoded_secret", severity: "high", langs: None },
            // Medium severity: SQL-injection-shaped patterns and disabled TLS.
            Pattern { re: Regex::new(r#"f['"].*SELECT.*\{.*\}"#).unwrap(), kind: "fstring_sql", severity: "med", langs: Some(PY) },
            Pattern { re: Regex::new(r#"\.execute\(\s*['"]\s*SELECT.*\+"#).unwrap(), kind: "concat_sql", severity: "med", langs: None },
            Pattern { re: Regex::new(r"verify\s*=\s*False").unwrap(), kind: "tls_verify_false", severity: "med", langs: Some(PY) },
            // Low severity: weak crypto. Still surfaced — md5/sha1 in 2026 is a code smell.
            Pattern { re: Regex::new(r"\bmd5\b|\bsha1\b").unwrap(), kind: "weak_hash", severity: "low", langs: None },
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
        let Some(ext) = file_ext(&path) else { continue };
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().to_string();
        out.extend(scan_text_for_ext(&rel, &text, ext));
    }
}

fn file_ext(path: &Path) -> Option<&'static str> {
    let ext = path.extension().and_then(|e| e.to_str())?;
    // Only scan the extensions we know how to interpret.
    Some(match ext {
        "py" => "py",
        "js" => "js",
        "ts" => "ts",
        "tsx" => "tsx",
        "jsx" => "jsx",
        "rb" => "rb",
        "go" => "go",
        "java" => "java",
        "cs" => "cs",
        "rs" => "rs",
        "php" => "php",
        _ => return None,
    })
}

fn is_skipped_dir(path: &Path) -> bool {
    let name = match path.file_name() { Some(n) => n.to_string_lossy(), None => return false };
    matches!(
        name.as_ref(),
        ".git" | "node_modules" | "__pycache__" | ".venv" | "venv" | "target"
            | "dist" | "build" | "vendor" | ".sigil" | ".repowise-workspace"
    )
}

/// Scan an in-memory source text. Public so callers can run the patterns
/// against synthetic fixtures or non-filesystem inputs. Applies every
/// pattern that has no language scope; mirrors the previous behavior for
/// callers that don't have a file extension to hand.
pub fn scan_text(file: &str, source: &str) -> Vec<SecurityFinding> {
    scan_text_inner(file, source, None)
}

/// Like `scan_text` but only runs patterns whose `langs` filter accepts
/// the given extension. Used by the filesystem walker so Python-shaped
/// idioms (`eval(`, `exec(`, `pickle.loads`) don't fire on Rust / Go
/// method names that happen to share a token.
pub fn scan_text_for_ext(file: &str, source: &str, ext: &str) -> Vec<SecurityFinding> {
    scan_text_inner(file, source, Some(ext))
}

fn scan_text_inner(file: &str, source: &str, ext: Option<&str>) -> Vec<SecurityFinding> {
    let pats = patterns();
    let mut out = Vec::new();
    for (i, line) in source.lines().enumerate() {
        for p in pats {
            if let (Some(allowed), Some(ext)) = (p.langs, ext) {
                if !allowed.iter().any(|a| *a == ext) {
                    continue;
                }
            }
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
