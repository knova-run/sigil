/// Languages where the body opens with `{`
fn is_brace_language(language: &str) -> bool {
    matches!(language, "rust" | "go" | "java" | "typescript" | "tsx"
        | "javascript" | "c" | "cpp" | "csharp" | "kotlin"
        | "swift" | "scala" | "php")
}

/// Languages where the body opens with `:`
fn is_colon_language(language: &str) -> bool {
    matches!(language, "python")
}

/// Detect decorator/attribute lines to skip.
fn is_decorator_line(line: &str, language: &str) -> bool {
    let trimmed = line.trim();
    match language {
        "python" => trimmed.starts_with('@'),
        "rust" => trimmed.starts_with("#[") || trimmed.starts_with("#!["),
        "java" | "csharp" | "kotlin" | "scala" => trimmed.starts_with('@'),
        "swift" => trimmed.starts_with('@'),
        "php" => trimmed.starts_with("#["),
        "typescript" | "javascript" | "tsx" => trimmed.starts_with('@'),
        _ => false,
    }
}

/// Extract the full signature of an entity from source bytes.
///
/// Returns (signature_string, body_start_line) where body_start_line is 1-indexed.
pub fn extract_signature(
    source: &str,
    line_start: usize,
    line_end: usize,
    language: &str,
) -> (Option<String>, usize) {
    let all_lines: Vec<&str> = source.lines().collect();
    if line_start == 0 || line_start > all_lines.len() {
        return (None, line_end + 1);
    }

    let start_idx = line_start - 1;
    let end_idx = (line_end).min(all_lines.len());
    let entity_lines = &all_lines[start_idx..end_idx];

    // Skip decorator/attribute lines
    let mut sig_start = 0;
    for (i, line) in entity_lines.iter().enumerate() {
        if is_decorator_line(line, language) {
            sig_start = i + 1;
        } else {
            break;
        }
    }

    if sig_start >= entity_lines.len() {
        return (None, line_end + 1);
    }

    let code_lines = &entity_lines[sig_start..];

    if is_brace_language(language) {
        extract_brace_signature(code_lines, line_start + sig_start, language)
    } else if is_colon_language(language) {
        extract_colon_signature(code_lines, line_start + sig_start)
    } else {
        (None, line_end + 1)
    }
}

fn extract_brace_signature(lines: &[&str], first_line_num: usize, _language: &str) -> (Option<String>, usize) {
    let mut sig_parts: Vec<&str> = Vec::new();
    let mut body_start_line = first_line_num + lines.len();

    for (i, line) in lines.iter().enumerate() {
        if let Some(brace_pos) = line.find('{') {
            let before_brace = line[..brace_pos].trim();
            if !before_brace.is_empty() {
                sig_parts.push(before_brace);
            }
            body_start_line = first_line_num + i + 1; // line after the {
            break;
        }
        sig_parts.push(line.trim());
    }

    if sig_parts.is_empty() {
        return (None, body_start_line);
    }

    // Join all parts with a space, then normalize
    let joined = sig_parts.join(" ");
    // Collapse whitespace
    let sig: String = joined.split_whitespace().collect::<Vec<_>>().join(" ");
    // Remove spaces after '(' and before ')'
    let sig = normalize_parens(&sig);
    // Strip trailing commas before closing parens: ", )" -> ")"
    let sig = sig.replace(", )", ")").replace(",)", ")");
    // Strip trailing comma from end of signature (e.g. where clauses)
    let sig = sig.trim_end_matches(',').trim().to_string();

    (Some(sig), body_start_line)
}

/// Remove extra spaces immediately after `(` and immediately before `)`.
fn normalize_parens(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        let ch = chars[i];
        if ch == '(' {
            result.push('(');
            // skip following spaces
            i += 1;
            while i < n && chars[i] == ' ' {
                i += 1;
            }
        } else if ch == ' ' && i + 1 < n && chars[i + 1] == ')' {
            // skip space before ')'
            i += 1;
        } else {
            result.push(ch);
            i += 1;
        }
    }
    result
}

fn extract_colon_signature(lines: &[&str], first_line_num: usize) -> (Option<String>, usize) {
    let mut sig_parts: Vec<&str> = Vec::new();
    let mut body_start_line = first_line_num + lines.len();

    for (i, line) in lines.iter().enumerate() {
        if let Some(colon_pos) = find_body_colon(line) {
            let up_to_colon = line[..=colon_pos].trim();
            sig_parts.push(up_to_colon);

            // Check if there's body content after the colon on the same line
            let after_colon = line[colon_pos + 1..].trim();
            if !after_colon.is_empty() {
                body_start_line = first_line_num + i;
            } else {
                body_start_line = first_line_num + i + 1;
            }
            break;
        }
        sig_parts.push(line.trim());
    }

    if sig_parts.is_empty() {
        return (None, body_start_line);
    }

    let sig = sig_parts.join(" ");
    let sig = sig.split_whitespace().collect::<Vec<_>>().join(" ");

    if sig.starts_with("def ") || sig.starts_with("class ")
        || sig.starts_with("async def ") {
        (Some(sig), body_start_line)
    } else {
        (None, body_start_line)
    }
}

/// Find the body-opening colon in a Python/Ruby line.
fn find_body_colon(line: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut last_colon = None;
    for (i, ch) in line.char_indices() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ':' if depth == 0 => last_colon = Some(i),
            _ => {}
        }
    }
    last_colon
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_simple_function() {
        let src = "def foo(x: int) -> bool:\n    return True\n";
        let (sig, body_start) = extract_signature(src, 1, 2, "python");
        assert_eq!(sig, Some("def foo(x: int) -> bool:".to_string()));
        assert_eq!(body_start, 2);
    }

    #[test]
    fn python_class() {
        let src = "class Config:\n    host: str\n";
        let (sig, body_start) = extract_signature(src, 1, 2, "python");
        assert_eq!(sig, Some("class Config:".to_string()));
        assert_eq!(body_start, 2);
    }

    #[test]
    fn python_decorator_excluded() {
        let src = "@dataclass\nclass Config:\n    host: str\n";
        let (sig, _) = extract_signature(src, 1, 3, "python");
        assert_eq!(sig, Some("class Config:".to_string()));
    }

    #[test]
    fn python_multi_decorator() {
        let src = "@app.route('/')\n@login_required\ndef handler():\n    pass\n";
        let (sig, body_start) = extract_signature(src, 1, 4, "python");
        assert_eq!(sig, Some("def handler():".to_string()));
        assert_eq!(body_start, 4);
    }

    #[test]
    fn rust_simple_function() {
        let src = "fn foo(x: i32) -> bool {\n    true\n}\n";
        let (sig, body_start) = extract_signature(src, 1, 3, "rust");
        assert_eq!(sig, Some("fn foo(x: i32) -> bool".to_string()));
        assert_eq!(body_start, 2);
    }

    #[test]
    fn rust_multi_line_signature() {
        let src = "pub fn process(\n    items: Vec<Item>,\n    config: &Config,\n) -> Result<()> {\n    Ok(())\n}\n";
        let (sig, body_start) = extract_signature(src, 1, 6, "rust");
        assert_eq!(sig, Some("pub fn process(items: Vec<Item>, config: &Config) -> Result<()>".to_string()));
        assert_eq!(body_start, 5);
    }

    #[test]
    fn rust_derive_excluded() {
        let src = "#[derive(Clone)]\npub struct Foo {\n    x: i32,\n}\n";
        let (sig, _) = extract_signature(src, 1, 4, "rust");
        assert_eq!(sig, Some("pub struct Foo".to_string()));
    }

    #[test]
    fn rust_where_clause() {
        let src = "pub fn run<T>(val: T) -> Result<()>\nwhere\n    T: Send + 'static,\n{\n    Ok(())\n}\n";
        let (sig, body_start) = extract_signature(src, 1, 6, "rust");
        assert_eq!(sig, Some("pub fn run<T>(val: T) -> Result<()> where T: Send + 'static".to_string()));
        assert_eq!(body_start, 5);
    }

    #[test]
    fn go_function() {
        let src = "func NewConfig() *Config {\n    return &Config{}\n}\n";
        let (sig, body_start) = extract_signature(src, 1, 3, "go");
        assert_eq!(sig, Some("func NewConfig() *Config".to_string()));
        assert_eq!(body_start, 2);
    }

    #[test]
    fn go_method_with_receiver() {
        let src = "func (c *Config) Validate() error {\n    return nil\n}\n";
        let (sig, body_start) = extract_signature(src, 1, 3, "go");
        assert_eq!(sig, Some("func (c *Config) Validate() error".to_string()));
        assert_eq!(body_start, 2);
    }

    #[test]
    fn typescript_arrow_function() {
        let src = "const handler = async (req: Request, res: Response) => {\n    res.send('ok')\n}\n";
        let (sig, body_start) = extract_signature(src, 1, 3, "typescript");
        assert_eq!(sig, Some("const handler = async (req: Request, res: Response) =>".to_string()));
        assert_eq!(body_start, 2);
    }

    #[test]
    fn python_single_line_function() {
        let src = "def foo(): pass\n";
        let (sig, body_start) = extract_signature(src, 1, 1, "python");
        assert_eq!(sig, Some("def foo():".to_string()));
        assert_eq!(body_start, 1);
    }

    #[test]
    fn import_no_signature() {
        let src = "import os\n";
        let (sig, body_start) = extract_signature(src, 1, 1, "python");
        assert!(sig.is_none());
        assert_eq!(body_start, 2);
    }

    #[test]
    fn java_annotation_excluded() {
        let src = "@Override\npublic void run() {\n    doWork();\n}\n";
        let (sig, _) = extract_signature(src, 1, 4, "java");
        assert_eq!(sig, Some("public void run()".to_string()));
    }
}
