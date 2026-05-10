/// Extract metaprogramming markers from an entity's source lines.
/// Returns None if no markers found.
pub fn extract_markers(
    source: &str,
    line_start: usize,
    line_end: usize,
    language: &str,
) -> Option<Vec<String>> {
    let all_lines: Vec<&str> = source.lines().collect();
    let start_idx = line_start.saturating_sub(1);
    let end_idx = line_end.min(all_lines.len());
    let entity_lines = &all_lines[start_idx..end_idx];

    let markers = match language {
        "python" => extract_python_markers(entity_lines),
        "rust" => extract_rust_markers(entity_lines),
        "java" | "csharp" | "kotlin" => extract_annotation_markers(entity_lines, language),
        "typescript" | "javascript" | "tsx" => extract_annotation_markers(entity_lines, language),
        "ruby" => extract_ruby_markers(entity_lines),
        _ => vec![],
    };

    if markers.is_empty() { None } else { Some(markers) }
}

fn extract_python_markers(lines: &[&str]) -> Vec<String> {
    let mut markers = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.starts_with('@') {
            let name = trimmed[1..]
                .split('(').next().unwrap_or("")
                .trim().to_string();
            if !name.is_empty() {
                markers.push(name);
            }
        } else if trimmed.starts_with("def ") || trimmed.starts_with("class ")
                || trimmed.starts_with("async def ") {
            break;
        }
    }
    markers
}

fn extract_rust_markers(lines: &[&str]) -> Vec<String> {
    let mut markers = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.starts_with("#[") || trimmed.starts_with("#![") {
            let attr_content = trimmed.trim_start_matches("#![")
                .trim_start_matches("#[")
                .trim_end_matches(']');

            if attr_content.starts_with("derive(") {
                let inner = attr_content.trim_start_matches("derive(")
                    .trim_end_matches(')');
                for trait_name in inner.split(',') {
                    let name = trait_name.trim().to_string();
                    if !name.is_empty() {
                        markers.push(name);
                    }
                }
            } else {
                let name = attr_content.split('(').next().unwrap_or("")
                    .trim().to_string();
                if !name.is_empty() {
                    markers.push(name);
                }
            }
        } else if !trimmed.is_empty() && !trimmed.starts_with("//") {
            break;
        }
    }
    markers
}

fn extract_annotation_markers(lines: &[&str], language: &str) -> Vec<String> {
    let mut markers = Vec::new();
    let prefix = match language {
        "csharp" => '[',
        _ => '@',
    };
    for line in lines {
        let trimmed = line.trim();
        if trimmed.starts_with(prefix) {
            let rest = &trimmed[prefix.len_utf8()..];
            let name = if language == "csharp" {
                rest.trim_end_matches(']').split('(').next().unwrap_or("").trim().to_string()
            } else {
                rest.split('(').next().unwrap_or("").trim().to_string()
            };
            if !name.is_empty() {
                markers.push(name);
            }
        } else if !trimmed.is_empty() {
            break;
        }
    }
    markers
}

fn extract_ruby_markers(lines: &[&str]) -> Vec<String> {
    let dsl_methods = [
        "has_many", "belongs_to", "has_one", "has_and_belongs_to_many",
        "validates", "validate", "scope", "attr_accessor", "attr_reader",
        "attr_writer", "delegate", "before_action", "after_action",
    ];
    let mut markers = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        for method in &dsl_methods {
            if trimmed.starts_with(method)
                && trimmed[method.len()..].starts_with(|c: char| c == ' ' || c == '(' || c == ':') {
                markers.push(method.to_string());
            }
        }
    }
    markers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_dataclass() {
        let src = "@dataclass\nclass Config:\n    host: str\n";
        let markers = extract_markers(src, 1, 3, "python");
        assert_eq!(markers, Some(vec!["dataclass".to_string()]));
    }

    #[test]
    fn python_stacked_decorators() {
        let src = "@app.route('/')\n@login_required\ndef handler():\n    pass\n";
        let markers = extract_markers(src, 1, 4, "python");
        assert_eq!(markers, Some(vec!["app.route".to_string(), "login_required".to_string()]));
    }

    #[test]
    fn python_no_decorators() {
        let src = "def plain():\n    pass\n";
        let markers = extract_markers(src, 1, 2, "python");
        assert!(markers.is_none());
    }

    #[test]
    fn rust_derive() {
        let src = "#[derive(Serialize, Deserialize, Clone)]\npub struct Config {\n    host: String,\n}\n";
        let markers = extract_markers(src, 1, 4, "rust");
        assert_eq!(markers, Some(vec!["Serialize".to_string(), "Deserialize".to_string(), "Clone".to_string()]));
    }

    #[test]
    fn rust_async_trait() {
        let src = "#[async_trait]\nimpl Service for MyService {\n}\n";
        let markers = extract_markers(src, 1, 3, "rust");
        assert_eq!(markers, Some(vec!["async_trait".to_string()]));
    }

    #[test]
    fn rust_derive_plus_other() {
        let src = "#[derive(Clone)]\n#[serde(rename_all = \"camelCase\")]\npub struct Foo {\n}\n";
        let markers = extract_markers(src, 1, 4, "rust");
        assert_eq!(markers, Some(vec!["Clone".to_string(), "serde".to_string()]));
    }

    #[test]
    fn java_annotation() {
        let src = "@Override\npublic void run() {\n    doWork();\n}\n";
        let markers = extract_markers(src, 1, 4, "java");
        assert_eq!(markers, Some(vec!["Override".to_string()]));
    }

    #[test]
    fn typescript_decorator() {
        let src = "@Component({})\nclass AppComponent {\n}\n";
        let markers = extract_markers(src, 1, 3, "typescript");
        assert_eq!(markers, Some(vec!["Component".to_string()]));
    }

    #[test]
    fn go_no_markers() {
        let src = "func foo() {\n}\n";
        let markers = extract_markers(src, 1, 2, "go");
        assert!(markers.is_none());
    }

    #[test]
    fn ruby_dsl_methods() {
        let src = "class User < ApplicationRecord\n  has_many :posts\n  validates :name, presence: true\nend\n";
        let markers = extract_markers(src, 1, 4, "ruby");
        assert_eq!(markers, Some(vec!["has_many".to_string(), "validates".to_string()]));
    }
}
