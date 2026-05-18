//! Identifier-aware tokenizer for code retrieval.
//!
//! Splits CamelCase, snake_case, kebab-case, and acronym runs into
//! lower-cased word tokens. Numeric-only tokens are dropped because they
//! rarely carry retrieval signal and inflate the posting lists.

/// Split a piece of source-shaped text into normalised word tokens.
///
/// Boundaries: any non-alphanumeric character, plus case transitions
/// `lower → Upper` (camelCase), `UPPER → Upperlower` (acronym→Word), and
/// transitions between letters and digits. Output is lower-case; tokens
/// consisting entirely of digits are dropped.
pub fn tokenize(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();

    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            flush(&mut cur, &mut out);
            continue;
        }
        if cur.is_empty() {
            cur.push(c);
            continue;
        }
        let prev = cur.chars().last().unwrap();
        let next = chars.get(i + 1).copied();
        let boundary = (prev.is_lowercase() && c.is_uppercase())
            || (prev.is_uppercase()
                && c.is_uppercase()
                && next.map_or(false, |n| n.is_lowercase()))
            || (prev.is_alphabetic() && c.is_ascii_digit())
            || (prev.is_ascii_digit() && c.is_alphabetic());
        if boundary {
            flush(&mut cur, &mut out);
        }
        cur.push(c);
    }
    flush(&mut cur, &mut out);
    out
}

fn flush(cur: &mut String, out: &mut Vec<String>) {
    if cur.is_empty() {
        return;
    }
    let token = std::mem::take(cur).to_lowercase();
    if token.chars().all(|c| c.is_ascii_digit()) {
        return;
    }
    out.push(token);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camel_case_splits_on_case_boundary() {
        assert_eq!(tokenize("buildIndex"), vec!["build", "index"]);
    }

    #[test]
    fn pascal_case_splits_on_case_boundary() {
        assert_eq!(tokenize("BuildIndex"), vec!["build", "index"]);
    }

    #[test]
    fn snake_case_splits_on_underscore() {
        assert_eq!(tokenize("build_index_for_repo"), vec!["build", "index", "for", "repo"]);
    }

    #[test]
    fn kebab_case_splits_on_hyphen() {
        assert_eq!(tokenize("parse-json-file"), vec!["parse", "json", "file"]);
    }

    #[test]
    fn acronym_run_then_camel_keeps_acronym_grouped() {
        // "parseHTTPResponse" → ["parse", "http", "response"]
        // The acronym (HTTP) stays a single token; the boundary detected when
        // the next char is lowercase is "P→Response".
        assert_eq!(
            tokenize("parseHTTPResponse"),
            vec!["parse", "http", "response"]
        );
    }

    #[test]
    fn mixed_separators_handled() {
        assert_eq!(
            tokenize("parse_json-fileBuilder"),
            vec!["parse", "json", "file", "builder"]
        );
    }

    #[test]
    fn pure_numeric_tokens_dropped() {
        assert_eq!(tokenize("123 456"), Vec::<String>::new());
    }

    #[test]
    fn alphanumeric_with_digits_keeps_alpha_part() {
        // "v1" → ["v"] (single-letter dropped below)? Actually keep "v" out
        // of stopword territory: we don't filter single chars here, just
        // numeric-only. So "v1" → "v" survives, "1" drops.
        // "parse2json" → ["parse", "json"].
        assert_eq!(tokenize("parse2json"), vec!["parse", "json"]);
    }

    #[test]
    fn whitespace_and_punctuation_are_separators() {
        assert_eq!(
            tokenize("parse the JSON file."),
            vec!["parse", "the", "json", "file"]
        );
    }

    #[test]
    fn empty_input_yields_empty_output() {
        assert_eq!(tokenize(""), Vec::<String>::new());
    }
}
