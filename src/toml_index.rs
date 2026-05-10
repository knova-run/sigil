use crate::entity::{Entity, Reference};
use crate::hasher;

/// Parse a TOML file and extract nested keys as entities.
pub fn parse_toml_file(
    source: &str,
    file_path: &str,
) -> Result<(Vec<Entity>, Vec<Reference>), String> {
    let value: toml::Value =
        toml::from_str(source).map_err(|e| format!("TOML parse error: {}", e))?;

    let mut entities = Vec::new();
    let lines: Vec<&str> = source.lines().collect();

    if let toml::Value::Table(map) = &value {
        let mut search_start_line = 0usize;
        extract_table_entities(
            source,
            file_path,
            map,
            None,
            &lines,
            &mut search_start_line,
            &mut entities,
        );
    }

    entities.sort_by(|a, b| a.line_start.cmp(&b.line_start));
    Ok((entities, Vec::new()))
}

/// Find the 1-indexed line number where a TOML key appears, searching from search_start_line (0-indexed).
/// Returns (1-indexed line number, 0-indexed line index) of the key line.
fn find_key_line(
    lines: &[&str],
    key: &str,
    search_start_line: usize,
) -> (u32, usize) {
    for i in search_start_line..lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();

        // Skip comments and empty lines
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Try bare key: key = ...
        if trimmed.starts_with(key) {
            let after = &trimmed[key.len()..];
            let after_trimmed = after.trim_start();
            if after_trimmed.starts_with('=') {
                return ((i + 1) as u32, i);
            }
        }

        // Try double-quoted key: "key" = ...
        let dq_needle = format!("\"{}\"", key);
        if trimmed.starts_with(&dq_needle) {
            let after = &trimmed[dq_needle.len()..];
            let after_trimmed = after.trim_start();
            if after_trimmed.starts_with('=') {
                return ((i + 1) as u32, i);
            }
        }

        // Try single-quoted key: 'key' = ...
        let sq_needle = format!("'{}'", key);
        if trimmed.starts_with(&sq_needle) {
            let after = &trimmed[sq_needle.len()..];
            let after_trimmed = after.trim_start();
            if after_trimmed.starts_with('=') {
                return ((i + 1) as u32, i);
            }
        }

        // Try table header: [key] or [parent.key]
        if trimmed.starts_with('[') && !trimmed.starts_with("[[") {
            let header = trimmed.trim_start_matches('[').trim_end_matches(']').trim();
            // Match if the last segment of the dotted key matches
            let last_segment = header.rsplit('.').next().unwrap_or(header).trim();
            let bare_segment = last_segment
                .trim_matches('"')
                .trim_matches('\'');
            if bare_segment == key {
                return ((i + 1) as u32, i);
            }
        }

        // Try array of tables header: [[key]]
        if trimmed.starts_with("[[") {
            let header = trimmed.trim_start_matches('[').trim_end_matches(']').trim();
            let last_segment = header.rsplit('.').next().unwrap_or(header).trim();
            let bare_segment = last_segment
                .trim_matches('"')
                .trim_matches('\'');
            if bare_segment == key {
                return ((i + 1) as u32, i);
            }
        }
    }

    // Fallback
    (1, 0)
}

/// Get the indentation level (number of leading spaces) of a line.
fn indent_level(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Find the end line of a TOML value.
/// Returns the 1-indexed end line number.
fn find_value_end_line(
    lines: &[&str],
    key_line_idx: usize,
    value: &toml::Value,
) -> u32 {
    let key_line = lines[key_line_idx];
    let trimmed = key_line.trim_start();

    match value {
        toml::Value::Table(_) => {
            // Check if inline table: key = { ... }
            if let Some(eq_pos) = trimmed.find('=') {
                let after_eq = trimmed[eq_pos + 1..].trim();
                if after_eq.starts_with('{') {
                    // Inline table — find matching closing brace
                    return find_inline_end_line(lines, key_line_idx, after_eq, b'{', b'}');
                }
            }

            // Table header [key] or dotted key parent — scan until next header or lesser/equal indent
            if trimmed.starts_with('[') {
                return find_table_section_end(lines, key_line_idx);
            }

            // Dotted key with inline table or implicit table
            find_block_end_line(lines, key_line_idx)
        }
        toml::Value::Array(arr) => {
            // Check if inline array: key = [ ... ]
            if let Some(eq_pos) = trimmed.find('=') {
                let after_eq = trimmed[eq_pos + 1..].trim();
                if after_eq.starts_with('[') {
                    return find_inline_end_line(lines, key_line_idx, after_eq, b'[', b']');
                }
            }

            // Array of tables [[key]] — find the end of the last element
            if trimmed.starts_with("[[") {
                // Find all consecutive [[key]] sections
                let mut last_line = key_line_idx;
                let header_content = trimmed.trim_start_matches('[').trim_end_matches(']').trim();

                for i in (key_line_idx + 1)..lines.len() {
                    let l = lines[i].trim_start();
                    if l.starts_with("[[") {
                        let h = l.trim_start_matches('[').trim_end_matches(']').trim();
                        if h == header_content {
                            // Another element of the same array of tables
                            continue;
                        }
                    }
                    if l.starts_with('[') {
                        // Different table/array-of-tables header — end before this
                        break;
                    }
                    if !l.is_empty() && !l.starts_with('#') {
                        last_line = i;
                    }
                }

                // If array is non-empty, include trailing content
                if !arr.is_empty() {
                    for i in (key_line_idx + 1)..lines.len() {
                        let l = lines[i].trim_start();
                        if l.starts_with('[') && !l.starts_with("[[") {
                            break;
                        }
                        if l.starts_with("[[") {
                            let h = l.trim_start_matches('[').trim_end_matches(']').trim();
                            if h != header_content {
                                break;
                            }
                        }
                        if !l.is_empty() && !l.starts_with('#') {
                            last_line = i;
                        }
                    }
                }

                return (last_line + 1) as u32;
            }

            (key_line_idx + 1) as u32
        }
        _ => {
            // Scalar: check for multi-line strings
            if let Some(eq_pos) = trimmed.find('=') {
                let after_eq = trimmed[eq_pos + 1..].trim();
                // Multi-line basic string: """
                if after_eq.starts_with("\"\"\"") {
                    return find_multiline_string_end(lines, key_line_idx, "\"\"\"");
                }
                // Multi-line literal string: '''
                if after_eq.starts_with("'''") {
                    return find_multiline_string_end(lines, key_line_idx, "'''");
                }
            }
            // Simple scalar: end line = key line
            (key_line_idx + 1) as u32
        }
    }
}

/// Find end line for inline constructs ({ ... } or [ ... ]) that may span multiple lines.
fn find_inline_end_line(
    lines: &[&str],
    start_line_idx: usize,
    first_content: &str,
    open: u8,
    close: u8,
) -> u32 {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut string_char = b'"';

    let bytes = first_content.as_bytes();
    let mut pos = 0;
    while pos < bytes.len() {
        let b = bytes[pos];
        if in_string {
            if b == b'\\' {
                pos += 1;
            } else if b == string_char {
                in_string = false;
            }
        } else {
            if b == b'"' || b == b'\'' {
                in_string = true;
                string_char = b;
            } else if b == open {
                depth += 1;
            } else if b == close {
                depth -= 1;
                if depth == 0 {
                    return (start_line_idx + 1) as u32;
                }
            }
        }
        pos += 1;
    }

    // Continue on subsequent lines
    for i in (start_line_idx + 1)..lines.len() {
        let bytes = lines[i].as_bytes();
        let mut pos = 0;
        while pos < bytes.len() {
            let b = bytes[pos];
            if in_string {
                if b == b'\\' {
                    pos += 1;
                } else if b == string_char {
                    in_string = false;
                }
            } else {
                if b == b'"' || b == b'\'' {
                    in_string = true;
                    string_char = b;
                } else if b == open {
                    depth += 1;
                } else if b == close {
                    depth -= 1;
                    if depth == 0 {
                        return (i + 1) as u32;
                    }
                }
            }
            pos += 1;
        }
    }

    lines.len() as u32
}

/// Find end of a [table] section: scan until next table header or end of file.
fn find_table_section_end(lines: &[&str], header_line_idx: usize) -> u32 {
    let mut last_content_line = header_line_idx;

    for i in (header_line_idx + 1)..lines.len() {
        let trimmed = lines[i].trim_start();
        if trimmed.starts_with('[') {
            break;
        }
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            last_content_line = i;
        }
    }

    (last_content_line + 1) as u32
}

/// Find end of a block-style value using indentation.
fn find_block_end_line(lines: &[&str], key_line_idx: usize) -> u32 {
    let key_indent = indent_level(lines[key_line_idx]);
    let mut last_content_line = key_line_idx;

    for i in (key_line_idx + 1)..lines.len() {
        let line = lines[i];
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let current_indent = indent_level(line);
        if current_indent <= key_indent {
            break;
        }
        last_content_line = i;
    }

    (last_content_line + 1) as u32
}

/// Find end of a multi-line string (""" or ''').
fn find_multiline_string_end(lines: &[&str], start_line_idx: usize, delimiter: &str) -> u32 {
    // The delimiter appears after = on the start line. Look for the closing delimiter.
    let start_line = lines[start_line_idx];
    // Find second occurrence of delimiter on same line (after the opening one)
    if let Some(eq_pos) = start_line.find('=') {
        let after_eq = &start_line[eq_pos + 1..];
        if let Some(first_pos) = after_eq.find(delimiter) {
            let rest = &after_eq[first_pos + delimiter.len()..];
            if rest.contains(delimiter) {
                return (start_line_idx + 1) as u32;
            }
        }
    }

    for i in (start_line_idx + 1)..lines.len() {
        if lines[i].contains(delimiter) {
            return (i + 1) as u32;
        }
    }

    lines.len() as u32
}

/// Return the TOML type name for a value.
fn toml_type_name(value: &toml::Value) -> &'static str {
    match value {
        toml::Value::String(_) => "string",
        toml::Value::Integer(_) => "integer",
        toml::Value::Float(_) => "float",
        toml::Value::Boolean(_) => "boolean",
        toml::Value::Datetime(_) => "datetime",
        toml::Value::Array(_) => "array",
        toml::Value::Table(_) => "table",
    }
}

/// Return the entity kind for a value.
fn entity_kind(value: &toml::Value) -> &'static str {
    match value {
        toml::Value::Table(_) => "object",
        toml::Value::Array(_) => "array",
        _ => "property",
    }
}

/// Recursively extract entities from a TOML table.
fn extract_table_entities(
    source: &str,
    file_path: &str,
    map: &toml::map::Map<String, toml::Value>,
    parent: Option<&str>,
    lines: &[&str],
    search_start_line: &mut usize,
    entities: &mut Vec<Entity>,
) {
    for (key, value) in map {
        let (key_line, key_line_idx) = find_key_line(lines, key, *search_start_line);
        let end_line = find_value_end_line(lines, key_line_idx, value);

        *search_start_line = end_line as usize;

        let kind = entity_kind(value);
        let type_name = toml_type_name(value);
        let sig = format!("\"{}\": {}", key, type_name);

        // Extract raw text for struct_hash
        let raw = hasher::extract_raw_bytes(source, key_line as usize, end_line as usize);
        let struct_hash = hasher::struct_hash(raw.as_bytes());

        // Compute body_hash from raw source lines
        let body_hash = hasher::body_hash_raw(source, key_line as usize, end_line as usize);

        // Compute sig_hash
        let sig_hash = hasher::sig_hash(Some(&sig));

        entities.push(Entity {
            file: file_path.to_string(),
            name: key.clone(),
            kind: kind.to_string(),
            line_start: key_line,
            line_end: end_line,
            parent: parent.map(|s| s.to_string()),
            qualified_name: crate::entity::compose_qualified_name(parent, key),
            sig: Some(sig),
            meta: None,
            body_hash,
            sig_hash,
            struct_hash,
            visibility: None,
            rank: None,
            blast_radius: None,
            doc: None,
            heritage: Vec::new(),
        });

        // Recurse into nested tables
        if let toml::Value::Table(nested_map) = value {
            let mut child_search_start = key_line_idx + 1;
            extract_table_entities(
                source,
                file_path,
                nested_map,
                Some(key),
                lines,
                &mut child_search_start,
                entities,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_toml() {
        let source = "name = \"myapp\"\nversion = \"1.0.0\"\n";
        let (entities, refs) = parse_toml_file(source, "test.toml").unwrap();
        assert!(refs.is_empty());
        assert_eq!(entities.len(), 2);

        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"name"));
        assert!(names.contains(&"version"));

        let name_entity = entities.iter().find(|e| e.name == "name").unwrap();
        assert_eq!(name_entity.kind, "property");
        assert_eq!(name_entity.sig.as_deref(), Some("\"name\": string"));
        assert!(name_entity.parent.is_none());
        assert_eq!(name_entity.line_start, 1);
        assert_eq!(name_entity.line_end, 1);
    }

    #[test]
    fn parse_nested_tables() {
        let source = "[settings]\ndebug = true\n\n[settings.theme]\ncolor = \"dark\"\n";
        let (entities, _) = parse_toml_file(source, "test.toml").unwrap();

        let settings = entities.iter().find(|e| e.name == "settings").unwrap();
        assert_eq!(settings.kind, "object");
        assert!(settings.parent.is_none());

        let theme = entities.iter().find(|e| e.name == "theme").unwrap();
        assert_eq!(theme.kind, "object");
        assert_eq!(theme.parent.as_deref(), Some("settings"));

        let color = entities.iter().find(|e| e.name == "color").unwrap();
        assert_eq!(color.kind, "property");
        assert_eq!(color.parent.as_deref(), Some("theme"));

        let debug = entities.iter().find(|e| e.name == "debug").unwrap();
        assert_eq!(debug.kind, "property");
        assert_eq!(debug.parent.as_deref(), Some("settings"));
    }

    #[test]
    fn parse_arrays() {
        let source = "tags = [\"fast\", \"structural\"]\nitems = [1, 2, 3]\n";
        let (entities, _) = parse_toml_file(source, "test.toml").unwrap();
        let tags = entities.iter().find(|e| e.name == "tags").unwrap();
        assert_eq!(tags.kind, "array");
        assert_eq!(tags.sig.as_deref(), Some("\"tags\": array"));
    }

    #[test]
    fn parse_all_value_types() {
        let source = "str_val = \"hello\"\nnum_val = 42\nfloat_val = 3.14\nbool_val = true\n";
        let (entities, _) = parse_toml_file(source, "test.toml").unwrap();
        assert_eq!(entities.len(), 4);

        let sigs: Vec<(&str, &str)> = entities.iter()
            .map(|e| (e.name.as_str(), e.sig.as_deref().unwrap()))
            .collect();
        assert!(sigs.contains(&("str_val", "\"str_val\": string")));
        assert!(sigs.contains(&("num_val", "\"num_val\": integer")));
        assert!(sigs.contains(&("float_val", "\"float_val\": float")));
        assert!(sigs.contains(&("bool_val", "\"bool_val\": boolean")));
    }

    #[test]
    fn hashes_are_present_and_16_chars() {
        let source = "key = \"value\"\n";
        let (entities, _) = parse_toml_file(source, "test.toml").unwrap();
        assert_eq!(entities.len(), 1);
        let e = &entities[0];
        assert_eq!(e.struct_hash.len(), 16);
        assert!(e.struct_hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(e.body_hash.is_some());
        assert_eq!(e.body_hash.as_ref().unwrap().len(), 16);
        assert!(e.sig_hash.is_some());
        assert_eq!(e.sig_hash.as_ref().unwrap().len(), 16);
    }

    #[test]
    fn parse_empty_table() {
        let source = "";
        let (entities, refs) = parse_toml_file(source, "test.toml").unwrap();
        assert!(entities.is_empty());
        assert!(refs.is_empty());
    }

    #[test]
    fn parse_invalid_toml() {
        let result = parse_toml_file("[invalid\nno closing bracket", "test.toml");
        assert!(result.is_err());
    }

    #[test]
    fn entities_sorted_by_line() {
        let source = "z_last = 1\na_first = 2\n";
        let (entities, _) = parse_toml_file(source, "test.toml").unwrap();
        assert!(entities[0].line_start <= entities[1].line_start);
    }

    #[test]
    fn table_section_span() {
        let source = "[config]\na = 1\nb = 2\n\n[other]\nc = 3\n";
        let (entities, _) = parse_toml_file(source, "test.toml").unwrap();
        let config = entities.iter().find(|e| e.name == "config").unwrap();
        assert_eq!(config.line_start, 1);
        assert!(config.line_end >= 3);
    }

    #[test]
    fn duplicate_keys_in_different_parents() {
        let source = "[a]\nid = 1\n\n[b]\nid = 2\n";
        let (entities, _) = parse_toml_file(source, "test.toml").unwrap();
        let ids: Vec<&Entity> = entities.iter().filter(|e| e.name == "id").collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0].parent, ids[1].parent);
    }

    #[test]
    fn meta_is_always_none() {
        let source = "key = \"value\"\n";
        let (entities, _) = parse_toml_file(source, "test.toml").unwrap();
        assert!(entities[0].meta.is_none());
    }

    #[test]
    fn inline_table() {
        let source = "config = { a = 1, b = 2 }\n";
        let (entities, _) = parse_toml_file(source, "test.toml").unwrap();
        let config = entities.iter().find(|e| e.name == "config").unwrap();
        assert_eq!(config.kind, "object");
    }

    #[test]
    fn quoted_keys() {
        let source = "\"my.dotted.key\" = 1\n\"key with spaces\" = 2\n";
        let (entities, _) = parse_toml_file(source, "test.toml").unwrap();
        assert_eq!(entities.len(), 2);
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"my.dotted.key"));
        assert!(names.contains(&"key with spaces"));
    }

    #[test]
    fn multiline_array() {
        let source = "deps = [\n  \"a\",\n  \"b\",\n  \"c\",\n]\n";
        let (entities, _) = parse_toml_file(source, "test.toml").unwrap();
        let deps = entities.iter().find(|e| e.name == "deps").unwrap();
        assert_eq!(deps.kind, "array");
        assert_eq!(deps.line_start, 1);
        assert!(deps.line_end >= 5);
    }

    #[test]
    fn unchanged_section_has_stable_body_hash() {
        let source_a = "[project]\nname = \"myapp\"\nversion = \"1.0.0\"\n\n[tool]\nbuilder = \"cargo\"\n";
        let source_b = "[project]\nname = \"myapp\"\nversion = \"2.0.0\"\n\n[tool]\nbuilder = \"cargo\"\n";
        let (entities_a, _) = parse_toml_file(source_a, "test.toml").unwrap();
        let (entities_b, _) = parse_toml_file(source_b, "test.toml").unwrap();
        let tool_a = entities_a.iter().find(|e| e.name == "tool").unwrap();
        let tool_b = entities_b.iter().find(|e| e.name == "tool").unwrap();
        assert_eq!(tool_a.body_hash, tool_b.body_hash,
            "unchanged TOML section should have identical body_hash");
        assert_eq!(tool_a.struct_hash, tool_b.struct_hash);
    }
}
