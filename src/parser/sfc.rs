//! Single File Component (SFC) preprocessor.
//!
//! Extracts `<script>` blocks from Vue, Svelte, and Astro files so they can
//! be fed to the existing TypeScript/JavaScript tree-sitter extractors.
//!
//! No external dependencies — uses simple byte-level scanning.

/// A script block extracted from an SFC file.
pub struct ScriptBlock {
    /// The script content (without the surrounding tags).
    pub content: Vec<u8>,
    /// The language to parse with: `"typescript"` or `"javascript"`.
    pub lang: &'static str,
    /// 1-based line number where the content starts in the original file.
    pub start_line: u32,
}

/// A component-use site found in a `<template>` block — `<MyComp />` /
/// `<my-comp>...</my-comp>`. Capitalised tag names AND kebab-case
/// names that resolve to PascalCase via Vue's name-conversion rule
/// surface as component refs.
pub struct ComponentTag {
    /// PascalCase form of the tag name (e.g. `<my-comp>` → `MyComp`).
    pub name: String,
    /// 1-based line number where the tag appears.
    pub line: u32,
}

/// Extract user-component use sites from the `<template>` block of a
/// Vue/Svelte/Astro file. Distinguishes from DOM intrinsics by the
/// PascalCase / kebab-case-with-dash convention: `<MyComp>` and
/// `<my-comp>` both qualify; `<div>` and `<span>` do not.
///
/// Returns `(PascalCaseName, line)` pairs. The PascalCase form is what
/// the script-block exports as the component identifier (via
/// `import Foo from './Foo.vue'`), so refs emitted under this name
/// resolve to the component entity.
pub fn extract_template_component_tags(source: &[u8]) -> Vec<ComponentTag> {
    let text = match std::str::from_utf8(source) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    // Locate the <template> block. Vue / Svelte both have one; Astro's
    // entire body is the template, so we treat the whole file as
    // template when no <template> wrapper exists.
    let (region, base_line) = match locate_template_region(text) {
        Some(r) => r,
        None => (text, 1u32),
    };
    let mut out: Vec<ComponentTag> = Vec::new();
    let mut line_num = base_line;
    for line in region.lines() {
        // Walk every `<` and check whether the following chars form a
        // component-shaped tag. Cheaper than a regex on every line.
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'<' {
                // Skip closing tags / comments / doctype / CDATA.
                if i + 1 < bytes.len() {
                    let next = bytes[i + 1];
                    if next == b'/' || next == b'!' || next == b'?' {
                        i += 1;
                        continue;
                    }
                }
                // Read the tag name: [A-Za-z][A-Za-z0-9_.\-]*
                let start = i + 1;
                let mut end = start;
                while end < bytes.len() {
                    let b = bytes[end];
                    if b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-' {
                        end += 1;
                    } else {
                        break;
                    }
                }
                if end > start {
                    let raw_tag = &line[start..end];
                    if let Some(pascal) = component_tag_to_pascal(raw_tag) {
                        out.push(ComponentTag {
                            name: pascal,
                            line: line_num,
                        });
                    }
                }
                i = end.max(i + 1);
            } else {
                i += 1;
            }
        }
        line_num += 1;
    }
    out
}

/// Locate the outer `<template>...</template>` block in a Vue/Svelte
/// file. Vue allows nested `<template v-if>` inside the outer template,
/// so we balance-count opens against closes to find the matching outer
/// close (otherwise the first inner `</template>` truncates the region
/// and template tags after it go uncounted). Returns the inner content
/// + the 1-based line number where the inner content starts.
fn locate_template_region(text: &str) -> Option<(&str, u32)> {
    let lower = text.to_ascii_lowercase();
    let open_idx = lower.find("<template")?;
    // End of the outer opening tag.
    let open_end = text[open_idx..].find('>').map(|p| open_idx + p + 1)?;
    // Walk forward balancing `<template` vs `</template>`. We start
    // INSIDE the outer tag (depth=1) and look for the position where
    // depth drops back to 0.
    let mut depth: i32 = 1;
    let mut cursor = open_end;
    let bytes = lower.as_bytes();
    while cursor < bytes.len() {
        // Find the next `<template` or `</template>` — whichever is closer.
        let next_open = lower[cursor..].find("<template").map(|p| cursor + p);
        let next_close = lower[cursor..].find("</template>").map(|p| cursor + p);
        match (next_open, next_close) {
            (None, None) => return None,
            (Some(o), Some(c)) if o < c => {
                depth += 1;
                cursor = o + "<template".len();
            }
            (Some(_), Some(c)) => {
                depth -= 1;
                if depth == 0 {
                    let inner = &text[open_end..c];
                    let start_line =
                        1 + text[..open_end].bytes().filter(|b| *b == b'\n').count() as u32;
                    return Some((inner, start_line));
                }
                cursor = c + "</template>".len();
            }
            (Some(o), None) => {
                // Open without ever closing — malformed; bail.
                let _ = o;
                return None;
            }
            (None, Some(c)) => {
                depth -= 1;
                if depth == 0 {
                    let inner = &text[open_end..c];
                    let start_line =
                        1 + text[..open_end].bytes().filter(|b| *b == b'\n').count() as u32;
                    return Some((inner, start_line));
                }
                cursor = c + "</template>".len();
            }
        }
    }
    None
}

/// Decide whether a raw tag name (`MyComp`, `my-comp`, `MyComp.Slot`,
/// `div`, `slot-name`) refers to a user component and, if so, return
/// its PascalCase form. Returns None for HTML/Svelte built-ins.
fn component_tag_to_pascal(raw: &str) -> Option<String> {
    let trimmed = raw.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let first = trimmed.chars().next()?;
    // PascalCase tags are component refs as-is. `MyComp` / `MyComp.Slot`
    if first.is_ascii_uppercase() {
        return Some(trimmed.to_string());
    }
    // kebab-case with at least one dash → convert to PascalCase. Vue's
    // own resolution rule (`<my-comp>` ↔ component MyComp). Skip
    // dash-less lowercase tags (`<div>` etc.).
    if trimmed.contains('-') {
        // Filter Vue built-ins (transition, keep-alive, ...)
        if matches!(
            trimmed,
            "keep-alive"
                | "router-link"
                | "router-view"
        ) {
            return None;
        }
        let parts: Vec<String> = trimmed
            .split('-')
            .filter(|p| !p.is_empty())
            .map(|p| {
                let mut chars = p.chars();
                match chars.next() {
                    Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
                    None => String::new(),
                }
            })
            .collect();
        let pascal = parts.join("");
        if pascal.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false) {
            return Some(pascal);
        }
    }
    None
}

/// Extract script blocks from an SFC file based on its extension.
///
/// Returns an empty vec if no script blocks are found (e.g. template-only
/// components).
pub fn extract_script_blocks(source: &[u8], extension: &str) -> Vec<ScriptBlock> {
    match extension {
        "html" => extract_html_script_tags(source, "javascript"),
        "vue" => extract_vue_scripts(source),
        "svelte" => extract_svelte_scripts(source),
        "astro" => extract_astro_scripts(source),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Vue: <script> and <script setup>, with optional lang="ts"
// ---------------------------------------------------------------------------

fn extract_vue_scripts(source: &[u8]) -> Vec<ScriptBlock> {
    extract_html_script_tags(source, "javascript")
}

// ---------------------------------------------------------------------------
// Svelte: <script> with optional lang="ts"
// ---------------------------------------------------------------------------

fn extract_svelte_scripts(source: &[u8]) -> Vec<ScriptBlock> {
    extract_html_script_tags(source, "javascript")
}

// ---------------------------------------------------------------------------
// Astro: --- frontmatter --- (always TS) + optional <script> blocks
// ---------------------------------------------------------------------------

fn extract_astro_scripts(source: &[u8]) -> Vec<ScriptBlock> {
    let mut blocks = Vec::new();
    let text = String::from_utf8_lossy(source);

    // Extract frontmatter: content between first `---` and second `---`
    if let Some(first) = text.find("---") {
        let after_first = first + 3;
        if let Some(rest) = text.get(after_first..)
            && let Some(second) = rest.find("---")
        {
            // Skip leading newline after opening ---
            let mut fm_start = after_first;
            if text.as_bytes().get(fm_start) == Some(&b'\n') {
                fm_start += 1;
            } else if text.as_bytes().get(fm_start) == Some(&b'\r')
                && text.as_bytes().get(fm_start + 1) == Some(&b'\n')
            {
                fm_start += 2;
            }
            let fm_end = after_first + second;
            if let Some(frontmatter) = text.get(fm_start..fm_end) {
                // Count lines before the frontmatter content starts
                let start_line = text.get(..fm_start).map(count_newlines_in).unwrap_or(0) + 1;

                if !frontmatter.trim().is_empty() {
                    blocks.push(ScriptBlock {
                        content: frontmatter.as_bytes().to_vec(),
                        lang: "typescript",
                        start_line: start_line as u32,
                    });
                }
            }
        }
    }

    // Also extract any <script> tags in the body
    blocks.extend(extract_html_script_tags(source, "typescript"));

    blocks
}

// ---------------------------------------------------------------------------
// Shared: extract <script> ... </script> blocks from HTML-like content
// ---------------------------------------------------------------------------

/// Extract all `<script ...>...</script>` blocks from HTML-like source.
///
/// `default_lang` is used when no `lang="..."` attribute is present.
fn extract_html_script_tags(source: &[u8], default_lang: &str) -> Vec<ScriptBlock> {
    let mut blocks = Vec::new();
    let text = String::from_utf8_lossy(source);
    let text_lower = text.to_ascii_lowercase();

    let mut search_from = 0;

    while let Some(rest) = text_lower.get(search_from..) {
        let Some(pos) = rest.find("<script") else {
            break;
        };
        let tag_start = search_from + pos;

        // Make sure it's actually a tag (next char after "script" should be whitespace or >)
        let after_script = tag_start + 7; // len("<script")
        let Some(&next_char) = text.as_bytes().get(after_script) else {
            break;
        };
        if next_char != b' '
            && next_char != b'\t'
            && next_char != b'\n'
            && next_char != b'\r'
            && next_char != b'>'
        {
            search_from = after_script;
            continue;
        }

        // Find the closing > of the opening tag
        let Some(rest_after_script) = text.get(after_script..) else {
            break;
        };
        let tag_close = match rest_after_script.find('>') {
            Some(pos) => after_script + pos,
            None => break,
        };

        // Extract the opening tag attributes
        let Some(open_tag) = text.get(tag_start..=tag_close) else {
            break;
        };

        // Detect language from lang="..." attribute
        let lang = detect_script_lang(open_tag, default_lang);

        // Content starts after the > (skip leading newline if present)
        let mut content_start = tag_close + 1;
        if text.as_bytes().get(content_start) == Some(&b'\n') {
            content_start += 1;
        } else if text.as_bytes().get(content_start) == Some(&b'\r')
            && text.as_bytes().get(content_start + 1) == Some(&b'\n')
        {
            content_start += 2;
        }

        // Find the matching </script> — content ends where the close tag begins
        let Some(rest_content) = text_lower.get(content_start..) else {
            break;
        };
        let content_end = match rest_content.find("</script") {
            Some(pos) => content_start + pos,
            None => break,
        };

        let Some(content) = text.get(content_start..content_end) else {
            break;
        };

        // Calculate the 1-based start line of the content
        let start_line = text
            .get(..content_start)
            .map(count_newlines_in)
            .unwrap_or(0)
            + 1;

        if !content.trim().is_empty() {
            blocks.push(ScriptBlock {
                content: content.as_bytes().to_vec(),
                lang,
                start_line: start_line as u32,
            });
        }

        // Move past the closing tag
        search_from = content_end;
        // Skip past </script>
        let Some(rest_close) = text_lower.get(search_from..) else {
            break;
        };
        if let Some(pos) = rest_close.find('>') {
            search_from += pos + 1;
        } else {
            break;
        }
    }

    blocks
}

/// Detect the script language from an opening `<script ...>` tag.
///
/// Looks for `lang="ts"`, `lang="typescript"`, `lang='ts'`, etc.
/// Returns `"typescript"` or `"javascript"`.
fn detect_script_lang(open_tag: &str, default_lang: &str) -> &'static str {
    let lower = open_tag.to_ascii_lowercase();

    // Match lang="ts" or lang="typescript" (with either quote style)
    if let Some(pos) = lower.find("lang=") {
        let after_eq = pos + 5;
        let rest = lower.get(after_eq..).unwrap_or("");
        let rest = rest.trim_start_matches(['"', '\'']);

        if rest.starts_with("ts") || rest.starts_with("typescript") {
            return "typescript";
        }
        if rest.starts_with("js") || rest.starts_with("javascript") {
            return "javascript";
        }
    }

    // No lang attribute — use the default
    if default_lang == "typescript" {
        "typescript"
    } else {
        "javascript"
    }
}

/// Count the number of newline characters in a string slice.
fn count_newlines_in(s: &str) -> usize {
    s.bytes().filter(|&b| b == b'\n').count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vue_template_component_tags_pascal_and_kebab() {
        // QA pass on gitea: Vue files with nested `<template v-if>`
        // inside outer template confused the region locator and tags
        // after the first inner close were dropped. Balance-count fix.
        let source = b"<script setup lang=\"ts\">\nconst x = 1\n</script>\n<template>\n  <div>\n    <SvgIcon name=\"x\"/>\n    <template v-if=\"flag\">\n      <ActionStatusIcon/>\n    </template>\n    <relative-time :datetime=\"d\"/>\n    <div>\n      <SvgIcon name=\"y\"/>\n    </div>\n  </div>\n</template>\n";
        let tags = extract_template_component_tags(source);
        let names: Vec<&str> = tags.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"SvgIcon"), "two SvgIcon uses must surface; got {:?}", names);
        assert_eq!(
            names.iter().filter(|n| **n == "SvgIcon").count(),
            2,
            "both SvgIcon tags should emit (one before and one after the nested template)"
        );
        assert!(names.contains(&"ActionStatusIcon"), "tag inside inner template must surface; got {:?}", names);
        assert!(names.contains(&"RelativeTime"), "kebab-case `<relative-time>` should normalise to RelativeTime; got {:?}", names);
        // Negative: lowercase HTML elements (`<div>`) must not emit.
        assert!(!names.contains(&"Div"), "`<div>` must not emit");
    }

    #[test]
    fn test_vue_basic() {
        let source = b"<template>\n  <div>hello</div>\n</template>\n\n<script setup lang=\"ts\">\nimport { ref } from 'vue'\nconst msg = ref('hi')\n</script>\n";
        let blocks = extract_script_blocks(source, "vue");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].lang, "typescript");
        assert_eq!(blocks[0].start_line, 6);
        let content = String::from_utf8_lossy(&blocks[0].content);
        assert!(content.contains("import { ref }"));
        assert!(content.contains("const msg"));
    }

    #[test]
    fn test_vue_two_scripts() {
        let source = b"<script>\nexport default { name: 'Foo' }\n</script>\n\n<script setup lang=\"ts\">\nconst x = 1\n</script>\n";
        let blocks = extract_script_blocks(source, "vue");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].lang, "javascript");
        assert_eq!(blocks[0].start_line, 2);
        assert_eq!(blocks[1].lang, "typescript");
        assert_eq!(blocks[1].start_line, 6);
    }

    #[test]
    fn test_vue_no_script() {
        let source = b"<template>\n  <div>hello</div>\n</template>\n";
        let blocks = extract_script_blocks(source, "vue");
        assert!(blocks.is_empty());
    }

    #[test]
    fn test_astro_frontmatter() {
        let source = b"---\nimport Layout from './Layout.astro'\nconst title = 'Hello'\n---\n\n<Layout title={title}>\n  <h1>Hello</h1>\n</Layout>\n";
        let blocks = extract_script_blocks(source, "astro");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].lang, "typescript");
        assert_eq!(blocks[0].start_line, 2);
        let content = String::from_utf8_lossy(&blocks[0].content);
        assert!(content.contains("import Layout"));
    }

    #[test]
    fn test_svelte_basic() {
        let source = b"<script lang=\"ts\">\n  let count = 0\n  function inc() { count++ }\n</script>\n\n<button on:click={inc}>{count}</button>\n";
        let blocks = extract_script_blocks(source, "svelte");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].lang, "typescript");
        assert_eq!(blocks[0].start_line, 2);
    }

    #[test]
    fn test_html_script_extraction() {
        let source = b"<!DOCTYPE html>\n<html>\n<head>\n<script>\nfunction greet(name) {\n  return 'Hello ' + name;\n}\n</script>\n</head>\n<body></body>\n</html>\n";
        let blocks = extract_script_blocks(source, "html");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].lang, "javascript");
        assert_eq!(blocks[0].start_line, 5);
        let content = String::from_utf8_lossy(&blocks[0].content);
        assert!(content.contains("function greet"));
    }

    #[test]
    fn test_html_no_script() {
        let source = b"<!DOCTYPE html>\n<html><body><p>Hello</p></body></html>\n";
        let blocks = extract_script_blocks(source, "html");
        assert!(blocks.is_empty());
    }

    #[test]
    fn test_html_typescript_script() {
        let source = b"<html>\n<body>\n<script lang=\"ts\">\nconst x: number = 42;\n</script>\n</body>\n</html>\n";
        let blocks = extract_script_blocks(source, "html");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].lang, "typescript");
    }

    #[test]
    fn test_detect_lang_ts() {
        assert_eq!(
            detect_script_lang("<script lang=\"ts\">", "javascript"),
            "typescript"
        );
        assert_eq!(
            detect_script_lang("<script lang='typescript'>", "javascript"),
            "typescript"
        );
        assert_eq!(
            detect_script_lang("<script setup lang=\"ts\">", "javascript"),
            "typescript"
        );
    }

    #[test]
    fn test_detect_lang_default() {
        assert_eq!(detect_script_lang("<script>", "javascript"), "javascript");
        assert_eq!(
            detect_script_lang("<script setup>", "javascript"),
            "javascript"
        );
        assert_eq!(detect_script_lang("<script>", "typescript"), "typescript");
    }
}
