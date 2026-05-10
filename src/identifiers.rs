//! Deterministic identifier extraction from arbitrary natural-language text.
//!
//! Targets symbol-shaped tokens that callers (e.g. wiki retrieval pipelines)
//! want to match against indexed entity names: CamelCase, snake_case, and
//! dotted paths like `Class::method` or `module.func`. Pure regex + token
//! filter — no language detection, no parsing.

/// Extract identifiers from `text`. Currently: CamelCase tokens of length ≥ 3.
use regex::Regex;
use std::sync::OnceLock;

fn token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Match identifier-shaped tokens optionally chained by `.` or `::`.
        Regex::new(r"[A-Za-z_][A-Za-z0-9_]*(?:(?:\.|::)[A-Za-z_][A-Za-z0-9_]*)+|[A-Za-z_][A-Za-z0-9_]*").unwrap()
    })
}

fn split_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `::` matched first so `::` consumes as one delimiter rather than two
    // `:` chars splitting into a phantom empty segment.
    RE.get_or_init(|| Regex::new(r"::|\.").unwrap())
}

pub fn extract(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::<String>::new();
    for m in token_re().find_iter(text) {
        let tok = m.as_str();
        // Candidates: the full token, plus each `.`/`::`-separated segment.
        let mut candidates: Vec<&str> = vec![tok];
        for seg in split_re().split(tok).filter(|s| !s.is_empty()) {
            candidates.push(seg);
        }
        for c in candidates {
            if !is_symbol_shaped(c) {
                continue;
            }
            if seen.insert(c.to_string()) {
                out.push(c.to_string());
            }
        }
    }
    out
}

fn is_symbol_shaped(s: &str) -> bool {
    if s.len() < 3 {
        return false;
    }
    // Symbol-shaped = at least one uppercase letter (CamelCase, dotted
    // PascalCase) OR an underscore (snake_case) OR a digit (less common
    // but still distinguishing from English words). Pure-lowercase
    // English nouns like "method", "class", "func" are intentionally
    // dropped — they match too broadly for retrieval promotion.
    let has_upper = s.chars().any(|c| c.is_uppercase());
    let has_under = s.contains('_');
    let has_digit = s.chars().any(|c| c.is_ascii_digit());
    let has_dot = s.contains('.') || s.contains("::");
    has_upper || has_under || has_digit || has_dot
}
