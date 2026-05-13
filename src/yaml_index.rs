use crate::entity::{Entity, Reference};
use crate::hasher;

/// Parse a YAML file and extract nested keys as entities.
pub fn parse_yaml_file(
    source: &str,
    file_path: &str,
) -> Result<(Vec<Entity>, Vec<Reference>), String> {
    let value: serde_yml::Value =
        serde_yml::from_str(source).map_err(|e| format!("YAML parse error: {}", e))?;

    let mut entities = Vec::new();
    let lines: Vec<&str> = source.lines().collect();

    if let serde_yml::Value::Mapping(map) = &value {
        let mut search_start_line = 0usize; // 0-indexed line index
        extract_mapping_entities(
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

/// Find the 1-indexed line number where a YAML key appears, searching from search_start_line (0-indexed).
/// Returns (1-indexed line number, 0-indexed line index) of the key line.
fn find_key_line(
    lines: &[&str],
    key: &str,
    search_start_line: usize,
) -> (u32, usize) {
    for i in search_start_line..lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();

        // Try unquoted form: key:
        if trimmed.starts_with(key) {
            let after = &trimmed[key.len()..];
            if after.starts_with(':') {
                // Make sure it's not a substring of a longer word.
                // Check the character before key in the trimmed line.
                // Since we used trim_start and then starts_with, the key is at position 0 in trimmed.
                // But we need to verify the key isn't part of a longer word.
                // Check that what comes before key in the original line is only whitespace.
                let indent = line.len() - trimmed.len();
                // The character before key in the line is at position indent-1, which is whitespace or start.
                // The character after key is ':', so it's a valid key. Good.
                let _ = indent;
                return ((i + 1) as u32, i);
            }
        }

        // Try double-quoted form: "key":
        let dq_needle = format!("\"{}\"", key);
        if let Some(pos) = trimmed.find(&dq_needle) {
            if pos == 0 {
                let after = &trimmed[dq_needle.len()..];
                let after_trimmed = after.trim_start();
                if after_trimmed.starts_with(':') {
                    return ((i + 1) as u32, i);
                }
            }
        }

        // Try single-quoted form: 'key':
        let sq_needle = format!("'{}'", key);
        if let Some(pos) = trimmed.find(&sq_needle) {
            if pos == 0 {
                let after = &trimmed[sq_needle.len()..];
                let after_trimmed = after.trim_start();
                if after_trimmed.starts_with(':') {
                    return ((i + 1) as u32, i);
                }
            }
        }
    }

    // Fallback: return line 1 if not found
    (1, 0)
}

/// Get the indentation level (number of leading spaces) of a line.
fn indent_level(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Find the end line of a YAML value.
/// Returns the 1-indexed end line number.
fn find_value_end_line(
    lines: &[&str],
    key_line_idx: usize,
    value: &serde_yml::Value,
) -> u32 {
    let key_line = lines[key_line_idx];
    let key_indent = indent_level(key_line);

    match value {
        serde_yml::Value::Mapping(_) | serde_yml::Value::Sequence(_) => {
            // Check if it's flow-style (inline { } or [ ])
            let trimmed = key_line.trim_start();
            // Find the colon and what follows
            let after_colon = find_after_colon(trimmed);
            let after_colon_trimmed = after_colon.trim();

            if after_colon_trimmed.starts_with('{') || after_colon_trimmed.starts_with('[') {
                // Flow style: track braces/brackets on this line and subsequent lines
                let (open, close) = if after_colon_trimmed.starts_with('{') {
                    (b'{', b'}')
                } else {
                    (b'[', b']')
                };
                return find_flow_end_line(lines, key_line_idx, after_colon_trimmed, open, close);
            }

            // Block style: scan forward until we hit a line at equal or lesser indentation
            find_block_end_line(lines, key_line_idx, key_indent)
        }
        _ => {
            // Scalar: check for multi-line indicators (| or >)
            let trimmed = key_line.trim_start();
            let after_colon = find_after_colon(trimmed);
            let after_colon_trimmed = after_colon.trim();

            if after_colon_trimmed.starts_with('|') || after_colon_trimmed.starts_with('>') {
                // Multi-line scalar: scan forward like block style
                return find_block_end_line(lines, key_line_idx, key_indent);
            }

            // Simple scalar: end line = key line
            (key_line_idx + 1) as u32
        }
    }
}

/// Extract the text after the first colon in a line.
fn find_after_colon(line: &str) -> &str {
    if let Some(colon_pos) = line.find(':') {
        &line[colon_pos + 1..]
    } else {
        ""
    }
}

/// Find the end line for a flow-style value ({...} or [...]).
fn find_flow_end_line(
    lines: &[&str],
    start_line_idx: usize,
    first_content: &str,
    open: u8,
    close: u8,
) -> u32 {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut string_char = b'"';

    // Process from the first content onward
    let bytes = first_content.as_bytes();
    let mut pos = 0;
    while pos < bytes.len() {
        let b = bytes[pos];
        if in_string {
            if b == b'\\' {
                pos += 1; // skip escaped character
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
                    pos += 1; // skip escaped character
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

/// Find the end line for a block-style value (mapping or sequence).
/// Scans forward from the key line until a non-empty line at equal or lesser indentation.
fn find_block_end_line(
    lines: &[&str],
    key_line_idx: usize,
    key_indent: usize,
) -> u32 {
    let mut last_content_line = key_line_idx;

    for i in (key_line_idx + 1)..lines.len() {
        let line = lines[i];
        // Skip empty lines and comment-only lines
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }

        let current_indent = indent_level(line);
        if current_indent <= key_indent {
            // Found a line at equal or lesser indentation - block ended before this
            break;
        }

        last_content_line = i;
    }

    (last_content_line + 1) as u32
}

/// Return the YAML type name for a value.
fn yaml_type_name(value: &serde_yml::Value) -> &'static str {
    match value {
        serde_yml::Value::Null => "null",
        serde_yml::Value::Bool(_) => "boolean",
        serde_yml::Value::Number(_) => "number",
        serde_yml::Value::String(_) => "string",
        serde_yml::Value::Sequence(_) => "array",
        serde_yml::Value::Mapping(_) => "object",
        serde_yml::Value::Tagged(tagged) => yaml_type_name(&tagged.value),
    }
}

/// Return the entity kind for a value.
fn entity_kind(value: &serde_yml::Value) -> &'static str {
    match value {
        serde_yml::Value::Mapping(_) => "object",
        serde_yml::Value::Sequence(_) => "array",
        serde_yml::Value::Tagged(tagged) => entity_kind(&tagged.value),
        _ => "property",
    }
}

/// Extract the string representation of a YAML mapping key.
fn key_as_string(key: &serde_yml::Value) -> String {
    match key.as_str() {
        Some(s) => s.to_string(),
        None => format!("{:?}", key),
    }
}

/// Recursively extract entities from a YAML mapping.
fn extract_mapping_entities(
    source: &str,
    file_path: &str,
    map: &serde_yml::Mapping,
    parent: Option<&str>,
    lines: &[&str],
    search_start_line: &mut usize,
    entities: &mut Vec<Entity>,
) {
    for (key_val, value) in map {
        let key = key_as_string(key_val);
        let (key_line, key_line_idx) = find_key_line(lines, &key, *search_start_line);
        let end_line = find_value_end_line(lines, key_line_idx, value);

        // Update search_start_line so next key search starts after this value's end
        *search_start_line = end_line as usize; // end_line is 1-indexed, so this is the next line's 0-index

        let kind = entity_kind(value);
        let type_name = yaml_type_name(value);
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
            qualified_name: crate::entity::compose_qualified_name(parent, &key),
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
            alias: None,        });

        // Recurse into nested mappings
        let inner_value = match value {
            serde_yml::Value::Tagged(tagged) => &tagged.value,
            other => other,
        };
        if let serde_yml::Value::Mapping(nested_map) = inner_value {
            // For recursion, child keys start on the line after the parent key
            let mut child_search_start = key_line_idx + 1;
            extract_mapping_entities(
                source,
                file_path,
                nested_map,
                Some(&key),
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
    fn parse_simple_yaml() {
        let source = "name: myapp\nversion: 1.0.0\n";
        let (entities, refs) = parse_yaml_file(source, "test.yaml").unwrap();
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
    fn parse_nested_mappings() {
        let source = "settings:\n  theme:\n    color: dark\n  debug: true\n";
        let (entities, _) = parse_yaml_file(source, "test.yaml").unwrap();
        assert_eq!(entities.len(), 4);

        let settings = entities.iter().find(|e| e.name == "settings").unwrap();
        assert_eq!(settings.kind, "object");
        assert!(settings.parent.is_none());
        assert_eq!(settings.line_start, 1);

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
    fn parse_sequences() {
        let source = "tags:\n  - fast\n  - structural\nitems:\n  - 1\n  - 2\n";
        let (entities, _) = parse_yaml_file(source, "test.yaml").unwrap();
        let tags = entities.iter().find(|e| e.name == "tags").unwrap();
        assert_eq!(tags.kind, "array");
        assert_eq!(tags.sig.as_deref(), Some("\"tags\": array"));
    }

    #[test]
    fn parse_all_value_types() {
        let source = "str_val: hello\nnum_val: 42\nfloat_val: 3.14\nbool_val: true\nnull_val: null\n";
        let (entities, _) = parse_yaml_file(source, "test.yaml").unwrap();
        assert_eq!(entities.len(), 5);

        let sigs: Vec<(&str, &str)> = entities.iter()
            .map(|e| (e.name.as_str(), e.sig.as_deref().unwrap()))
            .collect();
        assert!(sigs.contains(&("str_val", "\"str_val\": string")));
        assert!(sigs.contains(&("num_val", "\"num_val\": number")));
        assert!(sigs.contains(&("float_val", "\"float_val\": number")));
        assert!(sigs.contains(&("bool_val", "\"bool_val\": boolean")));
        assert!(sigs.contains(&("null_val", "\"null_val\": null")));
    }

    #[test]
    fn hashes_are_present_and_16_chars() {
        let source = "key: value\n";
        let (entities, _) = parse_yaml_file(source, "test.yaml").unwrap();
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
    fn parse_empty_mapping() {
        let source = "{}";
        let (entities, refs) = parse_yaml_file(source, "test.yaml").unwrap();
        assert!(entities.is_empty());
        assert!(refs.is_empty());
    }

    #[test]
    fn parse_invalid_yaml() {
        let result = parse_yaml_file(":\n  - :\n  - :\n    -", "test.yaml");
        assert!(result.is_err());
    }

    #[test]
    fn entities_sorted_by_line() {
        let source = "z_last: 1\na_first: 2\n";
        let (entities, _) = parse_yaml_file(source, "test.yaml").unwrap();
        assert!(entities[0].line_start <= entities[1].line_start);
    }

    #[test]
    fn multiline_mapping_span() {
        let source = "config:\n  a: 1\n  b: 2\nother: 3\n";
        let (entities, _) = parse_yaml_file(source, "test.yaml").unwrap();
        let config = entities.iter().find(|e| e.name == "config").unwrap();
        assert_eq!(config.line_start, 1);
        assert!(config.line_end >= 3);
    }

    #[test]
    fn duplicate_keys_in_different_parents() {
        let source = "a:\n  id: 1\nb:\n  id: 2\n";
        let (entities, _) = parse_yaml_file(source, "test.yaml").unwrap();
        let ids: Vec<&Entity> = entities.iter().filter(|e| e.name == "id").collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0].parent, ids[1].parent);
    }

    #[test]
    fn parse_root_sequence_returns_empty() {
        let source = "- 1\n- 2\n- 3\n";
        let (entities, _) = parse_yaml_file(source, "test.yaml").unwrap();
        assert!(entities.is_empty());
    }

    #[test]
    fn meta_is_always_none() {
        let source = "key: value\n";
        let (entities, _) = parse_yaml_file(source, "test.yaml").unwrap();
        assert!(entities[0].meta.is_none());
    }

    #[test]
    fn keys_with_special_characters() {
        let source = "\"my.dotted.key\": 1\n\"key with spaces\": 2\n";
        let (entities, _) = parse_yaml_file(source, "test.yaml").unwrap();
        assert_eq!(entities.len(), 2);
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"my.dotted.key"));
        assert!(names.contains(&"key with spaces"));
    }

    #[test]
    fn comments_are_ignored_in_parsing() {
        let source = "# This is a comment\nname: value\n# Another comment\nother: data\n";
        let (entities, _) = parse_yaml_file(source, "test.yaml").unwrap();
        assert_eq!(entities.len(), 2);
    }

    #[test]
    fn flow_style_mapping() {
        let source = "config: {a: 1, b: 2}\n";
        let (entities, _) = parse_yaml_file(source, "test.yaml").unwrap();
        let config = entities.iter().find(|e| e.name == "config").unwrap();
        assert_eq!(config.kind, "object");
    }
}
