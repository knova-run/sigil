use crate::entity::{Entity, Reference};
use crate::hasher;

/// Truncate a string to at most 60 characters, appending "..." if truncated.
/// Uses char boundaries to avoid panicking on multi-byte UTF-8.
fn truncate_name(s: &str) -> String {
    if s.chars().count() <= 60 {
        s.to_string()
    } else {
        let boundary = s.char_indices().nth(57).map(|(i, _)| i).unwrap_or(s.len());
        format!("{}...", &s[..boundary])
    }
}

/// Get the current parent heading name from the heading stack.
fn current_parent(heading_stack: &[(usize, String, usize)]) -> Option<String> {
    heading_stack.last().map(|(_, name, _)| name.clone())
}

/// Close all headings in the stack at level >= the given level, creating section entities.
/// Returns the entities for the closed sections.
fn close_headings(
    heading_stack: &mut Vec<(usize, String, usize)>,
    level: usize,
    end_line: u32,
    source: &str,
    file_path: &str,
    lines: &[&str],
    entities: &mut Vec<Entity>,
) {
    while let Some(&(top_level, _, _)) = heading_stack.last() {
        if top_level >= level {
            let (_, name, line_start) = heading_stack.pop().unwrap();

            // Determine parent: the next item on the stack (if any)
            let parent = current_parent(heading_stack);

            // sig = the full heading line
            let sig = lines[line_start - 1].to_string();

            let raw = hasher::extract_raw_bytes(source, line_start, end_line as usize);
            let struct_hash = hasher::struct_hash(raw.as_bytes());
            let body_hash = hasher::body_hash_raw(source, line_start, end_line as usize);
            let sig_hash = hasher::sig_hash(Some(&sig));

            let qualified_name = crate::entity::compose_qualified_name(parent.as_deref(), &name);
            entities.push(Entity {
                file: file_path.to_string(),
                name,
                kind: "section".to_string(),
                line_start: line_start as u32,
                line_end: end_line,
                parent,
                qualified_name,
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
                alias: None,            });
        } else {
            break;
        }
    }
}

/// Flush accumulated content (blockquote, list, or paragraph) into an entity.
fn flush_accumulator(
    acc_kind: &mut String,
    acc_lines: &mut Vec<usize>,
    acc_first_content: &mut String,
    acc_sig: &mut Option<String>,
    source: &str,
    file_path: &str,
    heading_stack: &[(usize, String, usize)],
    entities: &mut Vec<Entity>,
) {
    if acc_lines.is_empty() {
        return;
    }

    let line_start = acc_lines[0] as u32;
    let line_end = *acc_lines.last().unwrap() as u32;
    let parent = current_parent(heading_stack);
    let name = truncate_name(acc_first_content);
    let sig = acc_sig.take();

    let raw = hasher::extract_raw_bytes(source, line_start as usize, line_end as usize);
    let struct_hash = hasher::struct_hash(raw.as_bytes());
    let body_hash = hasher::body_hash_raw(source, line_start as usize, line_end as usize);
    let sig_hash = hasher::sig_hash(sig.as_deref());

    let qualified_name = crate::entity::compose_qualified_name(parent.as_deref(), &name);
    entities.push(Entity {
        file: file_path.to_string(),
        name,
        kind: acc_kind.to_string(),
        line_start,
        line_end,
        parent,
        qualified_name,
        sig,
        meta: None,
        body_hash,
        sig_hash,
        struct_hash,
        visibility: None,
        rank: None,
        blast_radius: None,
        doc: None,
        heritage: Vec::new(),
        alias: None,    });

    acc_kind.clear();
    acc_lines.clear();
    *acc_first_content = String::new();
}

/// Detect if a line is a list item. Returns Some(("ordered"|"unordered", content_after_marker))
/// or None if not a list item.
fn detect_list_item(line: &str) -> Option<(&'static str, &str)> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("- ") {
        Some(("unordered", &trimmed[2..]))
    } else if trimmed.starts_with("* ") {
        Some(("unordered", &trimmed[2..]))
    } else {
        // Check for ordered list: digits followed by ". "
        let mut i = 0;
        let bytes = trimmed.as_bytes();
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i > 0 && i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1] == b' ' {
            Some(("ordered", &trimmed[i + 2..]))
        } else {
            None
        }
    }
}

/// Check if a line is a fenced code block delimiter (``` or ~~~).
/// Returns Some((fence_char, fence_length, language_tag)) if it is.
fn detect_fence_open(line: &str) -> Option<(char, usize, Option<String>)> {
    let trimmed = line.trim_start();
    for fence_char in ['`', '~'] {
        if trimmed.starts_with(&format!("{}{}{}", fence_char, fence_char, fence_char)) {
            let fence_len = trimmed.chars().take_while(|&c| c == fence_char).count();
            let rest = &trimmed[fence_len..].trim();
            let lang = if rest.is_empty() {
                None
            } else {
                // Language tag is the first word
                Some(rest.split_whitespace().next().unwrap_or("").to_string())
            };
            return Some((fence_char, fence_len, lang));
        }
    }
    None
}

/// Check if a line closes a fenced code block.
fn detect_fence_close(line: &str, fence_char: char, fence_len: usize) -> bool {
    let trimmed = line.trim_start();
    if !trimmed.starts_with(fence_char) {
        return false;
    }
    let count = trimmed.chars().take_while(|&c| c == fence_char).count();
    if count < fence_len {
        return false;
    }
    // After the fence chars, only whitespace is allowed
    trimmed[count..].trim().is_empty()
}

/// Check if a line starts a table (line starts with | and next line is separator).
fn is_table_separator(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.contains('|') && trimmed.contains('-')
        && trimmed.chars().all(|c| c == '|' || c == '-' || c == ':' || c == ' ')
}

/// Parse ATX heading. Returns Some((level, heading_text)) or None.
fn parse_atx_heading(line: &str) -> Option<(usize, String)> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return None;
    }
    let level = trimmed.chars().take_while(|&c| c == '#').count();
    if level > 6 {
        return None;
    }
    let rest = &trimmed[level..];
    // Must be followed by a space or be end of line
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    let text = rest.trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some((level, text))
}

/// Parse a markdown file and extract structural entities.
pub fn parse_markdown_file(
    source: &str,
    file_path: &str,
) -> Result<(Vec<Entity>, Vec<Reference>), String> {
    if source.trim().is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    let lines: Vec<&str> = source.lines().collect();
    let total_lines = lines.len();
    let mut entities = Vec::new();

    // Heading stack: (level, name, line_start)  -- all 1-indexed
    let mut heading_stack: Vec<(usize, String, usize)> = Vec::new();

    // State machine states
    #[derive(PartialEq)]
    enum State {
        Normal,
        InFrontMatter,
        InFencedCode {
            fence_char: char,
            fence_len: usize,
            lang: Option<String>,
            start_line: usize,       // 1-indexed
            first_content: String,
        },
        InTable {
            start_line: usize,        // 1-indexed
            first_row_content: String,
        },
    }

    let mut state = State::Normal;

    // Accumulator for blockquote/list/paragraph
    let mut acc_kind = String::new();
    let mut acc_lines: Vec<usize> = Vec::new(); // 1-indexed line numbers
    let mut acc_first_content = String::new();
    let mut acc_sig: Option<String> = None;

    // Check for front matter on line 1
    let mut i = 0;
    if !lines.is_empty() && lines[0].trim() == "---" {
        state = State::InFrontMatter;
        i = 1;
    }

    // Collect front matter lines
    if state == State::InFrontMatter {
        let mut fm_lines: Vec<&str> = Vec::new();
        let mut fm_end_line = 0usize; // 0-indexed
        let mut found_close = false;

        while i < total_lines {
            if lines[i].trim() == "---" {
                fm_end_line = i;
                found_close = true;
                i += 1;
                break;
            }
            fm_lines.push(lines[i]);
            i += 1;
        }

        if found_close && !fm_lines.is_empty() {
            let yaml_body = fm_lines.join("\n");
            let fm_line_start: u32 = 1;
            let fm_line_end = (fm_end_line + 1) as u32; // 1-indexed, the closing ---

            // Create the frontmatter container entity
            let raw = hasher::extract_raw_bytes(source, fm_line_start as usize, fm_line_end as usize);
            let struct_hash = hasher::struct_hash(raw.as_bytes());
            let body_hash = hasher::body_hash_raw(source, fm_line_start as usize, fm_line_end as usize);

            entities.push(Entity {
                file: file_path.to_string(),
                name: "frontmatter".to_string(),
                kind: "frontmatter".to_string(),
                line_start: fm_line_start,
                line_end: fm_line_end,
                parent: None,
                qualified_name: None,
                sig: None,
                meta: None,
                body_hash,
                sig_hash: None,
                struct_hash,
                visibility: None,
                rank: None,
                blast_radius: None,
                doc: None,
                heritage: Vec::new(),
                alias: None,            });

            // Delegate YAML parsing
            if let Ok((yaml_entities, _)) = crate::yaml_index::parse_yaml_file(&yaml_body, file_path) {
                for mut ye in yaml_entities {
                    // Offset: YAML line 1 maps to file line 2 (after opening ---)
                    ye.line_start += 1;
                    ye.line_end += 1;
                    ye.parent = Some("frontmatter".to_string());
                    entities.push(ye);
                }
            }
        }

        state = State::Normal;
    }

    // Main state machine loop
    while i < total_lines {
        let line = lines[i];
        let line_num = i + 1; // 1-indexed

        match &mut state {
            State::Normal => {
                let trimmed = line.trim();

                // Blank line: flush any accumulator
                if trimmed.is_empty() {
                    flush_accumulator(
                        &mut acc_kind, &mut acc_lines, &mut acc_first_content,
                        &mut acc_sig, source, file_path, &heading_stack, &mut entities,
                    );
                    i += 1;
                    continue;
                }

                // Check for ATX heading
                if let Some((level, text)) = parse_atx_heading(line) {
                    // Flush any accumulator
                    flush_accumulator(
                        &mut acc_kind, &mut acc_lines, &mut acc_first_content,
                        &mut acc_sig, source, file_path, &heading_stack, &mut entities,
                    );

                    // Close headings at >= this level
                    // The end_line for closed headings is the line before this heading
                    let end_line = if line_num > 1 { (line_num - 1) as u32 } else { 1 };
                    close_headings(
                        &mut heading_stack, level, end_line,
                        source, file_path, &lines, &mut entities,
                    );

                    // Push this heading onto the stack
                    heading_stack.push((level, text, line_num));

                    i += 1;
                    continue;
                }

                // Check for fenced code block start
                if let Some((fence_char, fence_len, lang)) = detect_fence_open(line) {
                    // Flush any accumulator
                    flush_accumulator(
                        &mut acc_kind, &mut acc_lines, &mut acc_first_content,
                        &mut acc_sig, source, file_path, &heading_stack, &mut entities,
                    );

                    state = State::InFencedCode {
                        fence_char,
                        fence_len,
                        lang,
                        start_line: line_num,
                        first_content: String::new(),
                    };
                    i += 1;
                    continue;
                }

                // Check for table start: line starts with | and next line is separator
                if trimmed.starts_with('|') && i + 1 < total_lines && is_table_separator(lines[i + 1]) {
                    // Flush any accumulator
                    flush_accumulator(
                        &mut acc_kind, &mut acc_lines, &mut acc_first_content,
                        &mut acc_sig, source, file_path, &heading_stack, &mut entities,
                    );

                    state = State::InTable {
                        start_line: line_num,
                        first_row_content: truncate_name(trimmed),
                    };
                    i += 1;
                    continue;
                }

                // Check for blockquote
                if trimmed.starts_with('>') {
                    let content = if trimmed.starts_with("> ") {
                        &trimmed[2..]
                    } else {
                        &trimmed[1..]
                    };

                    if acc_kind == "blockquote" {
                        // Continue existing blockquote
                        acc_lines.push(line_num);
                    } else {
                        // Flush previous accumulator and start new blockquote
                        flush_accumulator(
                            &mut acc_kind, &mut acc_lines, &mut acc_first_content,
                            &mut acc_sig, source, file_path, &heading_stack, &mut entities,
                        );
                        acc_kind = "blockquote".to_string();
                        acc_lines.push(line_num);
                        acc_first_content = content.to_string();
                        acc_sig = None;
                    }
                    i += 1;
                    continue;
                }

                // Check for list item
                if let Some((list_type, content)) = detect_list_item(line) {
                    if acc_kind == "list" {
                        // Continue existing list
                        acc_lines.push(line_num);
                    } else {
                        // Flush previous accumulator and start new list
                        flush_accumulator(
                            &mut acc_kind, &mut acc_lines, &mut acc_first_content,
                            &mut acc_sig, source, file_path, &heading_stack, &mut entities,
                        );
                        acc_kind = "list".to_string();
                        acc_lines.push(line_num);
                        acc_first_content = content.to_string();
                        acc_sig = Some(list_type.to_string());
                    }
                    i += 1;
                    continue;
                }

                // Check for list continuation (indented line while in a list)
                if acc_kind == "list" && line.starts_with("  ") {
                    acc_lines.push(line_num);
                    i += 1;
                    continue;
                }

                // Check for horizontal rule (---, ***, ___) - just skip it
                if is_horizontal_rule(trimmed) {
                    flush_accumulator(
                        &mut acc_kind, &mut acc_lines, &mut acc_first_content,
                        &mut acc_sig, source, file_path, &heading_stack, &mut entities,
                    );
                    i += 1;
                    continue;
                }

                // Otherwise it's a paragraph line
                if acc_kind == "paragraph" {
                    acc_lines.push(line_num);
                } else {
                    flush_accumulator(
                        &mut acc_kind, &mut acc_lines, &mut acc_first_content,
                        &mut acc_sig, source, file_path, &heading_stack, &mut entities,
                    );
                    acc_kind = "paragraph".to_string();
                    acc_lines.push(line_num);
                    acc_first_content = trimmed.to_string();
                    acc_sig = None;
                }
                i += 1;
            }

            State::InFencedCode {
                fence_char,
                fence_len,
                lang,
                start_line,
                first_content,
            } => {
                // Check for closing fence
                if detect_fence_close(line, *fence_char, *fence_len) {
                    let code_start = *start_line;
                    let code_end = line_num;
                    let parent = current_parent(&heading_stack);

                    let sig = lang.clone().filter(|s| !s.is_empty());
                    let name = truncate_name(if first_content.is_empty() { "" } else { first_content });

                    let raw = hasher::extract_raw_bytes(source, code_start, code_end);
                    let struct_hash = hasher::struct_hash(raw.as_bytes());
                    let body_hash = hasher::body_hash_raw(source, code_start, code_end);
                    let sig_hash = hasher::sig_hash(sig.as_deref());

                    let qualified_name = crate::entity::compose_qualified_name(parent.as_deref(), &name);
                    entities.push(Entity {
                        file: file_path.to_string(),
                        name,
                        kind: "code_block".to_string(),
                        line_start: code_start as u32,
                        line_end: code_end as u32,
                        parent,
                        qualified_name,
                        sig,
                        meta: None,
                        body_hash,
                        sig_hash,
                        struct_hash,
                        visibility: None,
                        rank: None,
                        blast_radius: None,
                        doc: None,
                        heritage: Vec::new(),
                        alias: None,                    });

                    state = State::Normal;
                } else {
                    // Accumulate content
                    if first_content.is_empty() && !line.trim().is_empty() {
                        *first_content = line.trim().to_string();
                    }
                }
                i += 1;
            }

            State::InTable {
                start_line,
                first_row_content,
            } => {
                let trimmed = line.trim();
                if !trimmed.starts_with('|') {
                    // End of table
                    let table_start = *start_line;
                    let table_end = line_num - 1; // previous line was last table row
                    let parent = current_parent(&heading_stack);
                    let name = first_row_content.clone();

                    let raw = hasher::extract_raw_bytes(source, table_start, table_end);
                    let struct_hash = hasher::struct_hash(raw.as_bytes());
                    let body_hash = hasher::body_hash_raw(source, table_start, table_end);

                    let qualified_name = crate::entity::compose_qualified_name(parent.as_deref(), &name);
                    entities.push(Entity {
                        file: file_path.to_string(),
                        name,
                        kind: "table".to_string(),
                        line_start: table_start as u32,
                        line_end: table_end as u32,
                        parent,
                        qualified_name,
                        sig: None,
                        meta: None,
                        body_hash,
                        sig_hash: None,
                        struct_hash,
                        visibility: None,
                        rank: None,
                        blast_radius: None,
                        doc: None,
                        heritage: Vec::new(),
                        alias: None,                    });

                    state = State::Normal;
                    // Don't increment i -- re-process this line in Normal state
                    continue;
                }
                i += 1;
            }

            State::InFrontMatter => {
                // Should not reach here; front matter is handled before the main loop
                i += 1;
            }
        }
    }

    // EOF: flush any remaining accumulator
    flush_accumulator(
        &mut acc_kind, &mut acc_lines, &mut acc_first_content,
        &mut acc_sig, source, file_path, &heading_stack, &mut entities,
    );

    // Handle unclosed fenced code block at EOF (before draining heading stack)
    if let State::InFencedCode { start_line, lang, first_content, .. } = &state {
        let parent = current_parent(&heading_stack);
        let sig = lang.clone().filter(|s| !s.is_empty());
        let name = truncate_name(if first_content.is_empty() { "" } else { first_content });

        let raw = hasher::extract_raw_bytes(source, *start_line, total_lines);
        let struct_hash = hasher::struct_hash(raw.as_bytes());
        let body_hash = hasher::body_hash_raw(source, *start_line, total_lines);
        let sig_hash = hasher::sig_hash(sig.as_deref());

        let qualified_name = crate::entity::compose_qualified_name(parent.as_deref(), &name);
        entities.push(Entity {
            file: file_path.to_string(),
            name,
            kind: "code_block".to_string(),
            line_start: *start_line as u32,
            line_end: total_lines as u32,
            parent,
            qualified_name,
            sig,
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
    }

    // Handle unclosed table at EOF (before draining heading stack)
    if let State::InTable { start_line, first_row_content } = &state {
        let parent = current_parent(&heading_stack);


        let name = first_row_content.clone();

        let raw = hasher::extract_raw_bytes(source, *start_line, total_lines);
        let struct_hash = hasher::struct_hash(raw.as_bytes());
        let body_hash = hasher::body_hash_raw(source, *start_line, total_lines);

        let qualified_name = crate::entity::compose_qualified_name(parent.as_deref(), &name);
        entities.push(Entity {
            file: file_path.to_string(),
            name,
            kind: "table".to_string(),
            line_start: *start_line as u32,
            line_end: total_lines as u32,
            parent,
            qualified_name,
            sig: None,
            meta: None,
            body_hash,
            sig_hash: None,
            struct_hash,
            visibility: None,
            rank: None,
            blast_radius: None,
            doc: None,
            heritage: Vec::new(),
            alias: None,        });
    }

    // Close any remaining open headings (after handling unclosed blocks so they get correct parents)
    let end_line = total_lines as u32;
    close_headings(
        &mut heading_stack, 1, end_line,
        source, file_path, &lines, &mut entities,
    );

    entities.sort_by(|a: &Entity, b: &Entity| a.line_start.cmp(&b.line_start));
    Ok((entities, Vec::new()))
}

/// Check if a line is a horizontal rule (---, ***, ___ with optional spaces).
fn is_horizontal_rule(trimmed: &str) -> bool {
    if trimmed.len() < 3 {
        return false;
    }
    let stripped: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    if stripped.len() < 3 {
        return false;
    }
    let first = stripped.chars().next().unwrap();
    if first != '-' && first != '*' && first != '_' {
        return false;
    }
    stripped.chars().all(|c| c == first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_file() {
        let (entities, refs) = parse_markdown_file("", "test.md").unwrap();
        assert!(entities.is_empty());
        assert!(refs.is_empty());
    }

    #[test]
    fn parse_heading_hierarchy() {
        let source = "# Top\n\nSome text.\n\n## Sub A\n\nContent A.\n\n## Sub B\n\nContent B.\n\n### Deep\n\nDeep content.\n";
        let (entities, _) = parse_markdown_file(source, "test.md").unwrap();

        let sections: Vec<&Entity> = entities.iter().filter(|e| e.kind == "section").collect();
        assert_eq!(sections.len(), 4);

        let top = sections.iter().find(|e| e.name == "Top").unwrap();
        assert!(top.parent.is_none());
        assert_eq!(top.line_start, 1);
        assert_eq!(top.sig.as_deref(), Some("# Top"));

        let sub_a = sections.iter().find(|e| e.name == "Sub A").unwrap();
        assert_eq!(sub_a.parent.as_deref(), Some("Top"));

        let sub_b = sections.iter().find(|e| e.name == "Sub B").unwrap();
        assert_eq!(sub_b.parent.as_deref(), Some("Top"));

        let deep = sections.iter().find(|e| e.name == "Deep").unwrap();
        assert_eq!(deep.parent.as_deref(), Some("Sub B"));
    }

    #[test]
    fn parse_heading_only_file() {
        let source = "# A\n## B\n## C\n";
        let (entities, _) = parse_markdown_file(source, "test.md").unwrap();
        let sections: Vec<&Entity> = entities.iter().filter(|e| e.kind == "section").collect();
        assert_eq!(sections.len(), 3);
    }

    #[test]
    fn parse_code_blocks() {
        let source = "# Doc\n\n```python\ndef hello():\n    print(\"hi\")\n```\n\n```\nno language\n```\n";
        let (entities, _) = parse_markdown_file(source, "test.md").unwrap();

        let code_blocks: Vec<&Entity> = entities.iter().filter(|e| e.kind == "code_block").collect();
        assert_eq!(code_blocks.len(), 2);

        let py_block = &code_blocks[0];
        assert_eq!(py_block.sig.as_deref(), Some("python"));
        assert_eq!(py_block.parent.as_deref(), Some("Doc"));
        assert!(py_block.name.contains("def hello()"));

        let no_lang = &code_blocks[1];
        assert!(no_lang.sig.is_none());
        assert!(no_lang.name.contains("no language"));
    }

    #[test]
    fn parse_nested_fence_characters() {
        let source = "# Doc\n\n````\n```\nnested fence\n```\n````\n";
        let (entities, _) = parse_markdown_file(source, "test.md").unwrap();
        let code_blocks: Vec<&Entity> = entities.iter().filter(|e| e.kind == "code_block").collect();
        assert_eq!(code_blocks.len(), 1, "nested fence markers should not split the block");
    }

    #[test]
    fn parse_tilde_fence() {
        let source = "# Doc\n\n~~~bash\necho hello\n~~~\n";
        let (entities, _) = parse_markdown_file(source, "test.md").unwrap();
        let code_blocks: Vec<&Entity> = entities.iter().filter(|e| e.kind == "code_block").collect();
        assert_eq!(code_blocks.len(), 1);
        assert_eq!(code_blocks[0].sig.as_deref(), Some("bash"));
    }

    #[test]
    fn parse_front_matter() {
        let source = "---\ntitle: Getting Started\nauthor: Jane\ntags:\n  - rust\n  - cli\n---\n\n# Content\n\nSome text.\n";
        let (entities, _) = parse_markdown_file(source, "test.md").unwrap();

        let fm = entities.iter().find(|e| e.kind == "frontmatter").unwrap();
        assert_eq!(fm.name, "frontmatter");
        assert!(fm.parent.is_none());
        assert_eq!(fm.line_start, 1);

        let title = entities.iter().find(|e| e.name == "title").unwrap();
        assert_eq!(title.parent.as_deref(), Some("frontmatter"));

        let tags = entities.iter().find(|e| e.name == "tags").unwrap();
        assert_eq!(tags.parent.as_deref(), Some("frontmatter"));
        assert_eq!(tags.kind, "array");

        assert_eq!(title.line_start, 2);
    }

    #[test]
    fn parse_tables() {
        let source = "# Config\n\n| Option | Default |\n|--------|--------|\n| verbose | false |\n| color | true |\n\nSome text after.\n";
        let (entities, _) = parse_markdown_file(source, "test.md").unwrap();

        let tables: Vec<&Entity> = entities.iter().filter(|e| e.kind == "table").collect();
        assert_eq!(tables.len(), 1);
        assert!(tables[0].name.contains("Option"));
        assert_eq!(tables[0].parent.as_deref(), Some("Config"));
        assert!(tables[0].sig.is_none());
    }

    #[test]
    fn parse_blockquotes() {
        let source = "# Doc\n\n> This is a quote\n> spanning multiple lines.\n\nSome text.\n";
        let (entities, _) = parse_markdown_file(source, "test.md").unwrap();

        let bqs: Vec<&Entity> = entities.iter().filter(|e| e.kind == "blockquote").collect();
        assert_eq!(bqs.len(), 1);
        assert!(bqs[0].name.contains("This is a quote"));
        assert_eq!(bqs[0].parent.as_deref(), Some("Doc"));
    }

    #[test]
    fn parse_lists() {
        let source = "# Doc\n\n- item one\n- item two\n- item three\n\n1. first\n2. second\n";
        let (entities, _) = parse_markdown_file(source, "test.md").unwrap();

        let lists: Vec<&Entity> = entities.iter().filter(|e| e.kind == "list").collect();
        assert_eq!(lists.len(), 2);

        let unordered = &lists[0];
        assert_eq!(unordered.sig.as_deref(), Some("unordered"));
        assert!(unordered.name.contains("item one"));

        let ordered = &lists[1];
        assert_eq!(ordered.sig.as_deref(), Some("ordered"));
        assert!(ordered.name.contains("first"));
    }

    #[test]
    fn parse_paragraphs() {
        let source = "# Doc\n\nFirst paragraph text.\n\nSecond paragraph text.\n";
        let (entities, _) = parse_markdown_file(source, "test.md").unwrap();

        let paras: Vec<&Entity> = entities.iter().filter(|e| e.kind == "paragraph").collect();
        assert_eq!(paras.len(), 2);
        assert!(paras[0].name.contains("First paragraph"));
        assert!(paras[1].name.contains("Second paragraph"));
        assert_eq!(paras[0].parent.as_deref(), Some("Doc"));
    }

    #[test]
    fn parse_list_continuation() {
        let source = "# Doc\n\n- item one\n  continued on next line\n- item two\n";
        let (entities, _) = parse_markdown_file(source, "test.md").unwrap();

        let lists: Vec<&Entity> = entities.iter().filter(|e| e.kind == "list").collect();
        assert_eq!(lists.len(), 1, "continuation lines should not create a separate list");
    }

    #[test]
    fn parse_mixed_document() {
        let source = "---\ntitle: Test\n---\n\n# Introduction\n\nThis is a guide.\n\n## Setup\n\n```bash\nnpm install\n```\n\n| Col A | Col B |\n|-------|-------|\n| 1     | 2     |\n\n> Important note here.\n\n- step one\n- step two\n\n## Next Steps\n\nFinal paragraph.\n";
        let (entities, _) = parse_markdown_file(source, "test.md").unwrap();

        let kinds: Vec<&str> = entities.iter().map(|e| e.kind.as_str()).collect();
        assert!(kinds.contains(&"frontmatter"), "missing frontmatter");
        assert!(kinds.contains(&"section"), "missing section");
        assert!(kinds.contains(&"code_block"), "missing code_block");
        assert!(kinds.contains(&"table"), "missing table");
        assert!(kinds.contains(&"blockquote"), "missing blockquote");
        assert!(kinds.contains(&"list"), "missing list");
        assert!(kinds.contains(&"paragraph"), "missing paragraph");
    }

    #[test]
    fn parse_no_headings() {
        let source = "Just a paragraph.\n\n```rust\nlet x = 1;\n```\n";
        let (entities, _) = parse_markdown_file(source, "test.md").unwrap();

        for e in &entities {
            assert!(e.parent.is_none(), "entity {} should have no parent", e.name);
        }
    }

    #[test]
    fn hashes_are_present_and_16_chars() {
        let source = "# Heading\n\nSome text.\n";
        let (entities, _) = parse_markdown_file(source, "test.md").unwrap();
        for e in &entities {
            assert_eq!(e.struct_hash.len(), 16, "struct_hash wrong length for {}", e.name);
            assert!(e.struct_hash.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn meta_is_always_none() {
        let source = "# Heading\n\nText.\n";
        let (entities, _) = parse_markdown_file(source, "test.md").unwrap();
        for e in &entities {
            assert!(e.meta.is_none(), "meta should be None for {}", e.name);
        }
    }

    #[test]
    fn entities_sorted_by_line() {
        let source = "# A\n\nParagraph.\n\n## B\n\n```bash\necho hi\n```\n";
        let (entities, _) = parse_markdown_file(source, "test.md").unwrap();
        for w in entities.windows(2) {
            assert!(w[0].line_start <= w[1].line_start,
                "{} (line {}) should come before {} (line {})",
                w[0].name, w[0].line_start, w[1].name, w[1].line_start);
        }
    }

    #[test]
    fn horizontal_rule_not_frontmatter() {
        let source = "# Doc\n\nSome text.\n\n---\n\nMore text.\n";
        let (entities, _) = parse_markdown_file(source, "test.md").unwrap();
        let fm: Vec<&Entity> = entities.iter().filter(|e| e.kind == "frontmatter").collect();
        assert!(fm.is_empty(), "--- in middle of file should not be treated as frontmatter");
    }
}
