use serde::{Serialize, Deserialize};
use std::collections::{BTreeMap, HashSet};
use crate::diff_json::{DiffResult, ChangeKind, EntityDiff};
use crate::entity::Entity;
use crate::change_detail::DetailKind;
use crate::inline_diff;

/// Check if an EntityDiff involves a derived entity (meta contains "derived").
fn is_derived(diff: &EntityDiff) -> bool {
    let check_meta = |e: &Entity| -> bool {
        e.meta.as_ref().map_or(false, |m| m.contains(&"derived".to_string()))
    };
    diff.new.as_ref().map_or(false, |e| check_meta(e))
        || diff.old.as_ref().map_or(false, |e| check_meta(e))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub base_ref: String,
    pub head_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    pub generated_at: String,
    pub sigil_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputSummary {
    pub files_changed: usize,
    pub patterns: usize,
    pub moves: usize,
    pub added: usize,
    pub removed: usize,
    pub modified: usize,
    pub renamed: usize,
    pub formatting_only: usize,
    pub has_breaking: bool,
    pub natural_language: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_line: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallerInfo {
    pub file: String,
    pub line: u32,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BreakingEntry {
    pub entity: String,
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_callers: Option<Vec<CallerInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callers_in_diff: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputPattern {
    pub id: String,
    #[serde(rename = "type")]
    pub pattern_type: String,
    pub entity_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_glob: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_glob: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_name: Option<String>,
    pub file_count: usize,
    pub files: Vec<String>,
    pub entities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveEntry {
    pub entity: String,
    pub kind: String,
    pub from_file: String,
    pub to_file: String,
    pub from_line: u32,
    pub to_line: u32,
    pub breaking: bool,
    pub confidence: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenChange {
    #[serde(rename = "type")]
    pub change_type: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnippetContext {
    pub base_snippet: String,
    pub head_snippet: String,
    pub language: String,
    pub snippet_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hunks: Option<Vec<crate::inline_diff::DiffLine>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputEntity {
    pub change: String,
    pub name: String,
    pub kind: String,
    pub line: u32,
    pub line_end: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sig_changed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_changed: Option<bool>,
    pub breaking: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breaking_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pattern_ref: Option<String>,
    pub token_changes: Vec<TokenChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<SnippetContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSummary {
    pub added: usize,
    pub modified: usize,
    pub removed: usize,
    pub renamed: usize,
    pub formatting_only: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSection {
    pub file: String,
    pub summary: FileSummary,
    pub entities: Vec<OutputEntity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffOutput {
    pub meta: Meta,
    pub summary: OutputSummary,
    pub breaking: Vec<BreakingEntry>,
    pub patterns: Vec<OutputPattern>,
    pub moves: Vec<MoveEntry>,
    pub files: Vec<FileSection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub groups: Option<Vec<crate::grouping::ChangeGroup>>,
}

impl DiffOutput {
    pub fn from_result(result: &DiffResult, include_context: bool, context_lines: usize) -> Self {
        // Build pattern index: map entity names to pattern IDs
        let mut pattern_map: BTreeMap<String, String> = BTreeMap::new();
        let mut output_patterns: Vec<OutputPattern> = Vec::new();

        for (idx, pat) in result.patterns.iter().enumerate() {
            let pat_id = format!("pat_{}", idx + 1);
            let pattern_type = if pat.change == ChangeKind::Renamed {
                "rename".to_string()
            } else {
                "body_identical".to_string()
            };

            // Find entity_kind from the first matching entity
            let entity_kind = result.entities.iter()
                .find(|e| pat.entity_names.contains(&e.name))
                .map(|e| e.kind.clone())
                .unwrap_or_default();

            // For body_identical patterns, use the first entity_names entry
            let entity_name = if pattern_type == "body_identical" {
                pat.entity_names.first().cloned()
            } else {
                None
            };

            // Tag all matching entity names with this pattern ID
            for name in &pat.entity_names {
                pattern_map.insert(name.clone(), pat_id.clone());
            }

            output_patterns.push(OutputPattern {
                id: pat_id,
                pattern_type,
                entity_kind,
                from_glob: None,
                to_glob: None,
                entity_name,
                file_count: pat.files.len(),
                files: pat.files.clone(),
                entities: pat.entity_names.clone(),
            });
        }

        // Separate moves from file entities
        let mut moves: Vec<MoveEntry> = Vec::new();
        let mut breaking_entries: Vec<BreakingEntry> = Vec::new();

        // Use BTreeMap for deterministic file ordering
        let mut file_entities: BTreeMap<String, Vec<OutputEntity>> = BTreeMap::new();

        // Collect parent names that have non-derived children in the diff,
        // so we can suppress redundant parent entities for JSON files.
        let mut parents_with_children: HashSet<(String, String)> = HashSet::new(); // (file, parent_name)
        for diff in &result.entities {
            if is_derived(diff) { continue; }
            let entity = diff.new.as_ref().or(diff.old.as_ref());
            if let Some(e) = entity {
                if let Some(ref parent) = e.parent {
                    parents_with_children.insert((diff.file.clone(), parent.clone()));
                }
            }
        }

        for diff in &result.entities {
            // Skip derived entities
            if is_derived(diff) {
                continue;
            }

            // For JSON: suppress parent object/array entities when their children
            // already appear in the diff (the children carry the specific detail).
            let is_json = diff.file.ends_with(".json");
            if is_json {
                let entity_name = &diff.name;
                let is_parent_with_children = parents_with_children.contains(&(diff.file.clone(), entity_name.clone()));
                if is_parent_with_children && (diff.kind == "object" || diff.kind == "array") {
                    continue;
                }
            }

            // Determine if this is a true cross-file move vs same-file move
            let is_cross_file_move = diff.change == ChangeKind::Moved
                && diff.old_file.as_ref().map_or(false, |of| of != &diff.file);

            if is_cross_file_move {
                // Cross-file moves go to moves[]
                let old_file = diff.old_file.clone().unwrap_or_default();
                let from_line = diff.old.as_ref().map(|e| e.line_start).unwrap_or(0);
                let to_line = diff.new.as_ref().map(|e| e.line_start).unwrap_or(0);

                // Confidence: if sig and body unchanged → 1.0, otherwise lower
                let confidence = match (diff.sig_changed, diff.body_changed) {
                    (Some(false), Some(false)) | (None, None) => 1.0,
                    (Some(true), Some(false)) | (Some(false), Some(true)) => 0.8,
                    _ => 0.6,
                };

                moves.push(MoveEntry {
                    entity: diff.name.clone(),
                    kind: diff.kind.clone(),
                    from_file: old_file.clone(),
                    to_file: diff.file.clone(),
                    from_line,
                    to_line,
                    breaking: diff.breaking,
                    confidence,
                });

                // Breaking entries for moves use destination file
                if diff.breaking {
                    breaking_entries.push(BreakingEntry {
                        entity: diff.name.clone(),
                        kind: diff.kind.clone(),
                        file: diff.file.clone(),
                        line: to_line,
                        reason: breaking_reason_for(diff),
                        external_callers: None,
                        callers_in_diff: None,
                    });
                }
            } else {
                // Same-file moves get reclassified as modified
                let change_str = if diff.change == ChangeKind::Moved {
                    "modified".to_string()
                } else {
                    change_kind_to_string(&diff.change)
                };

                let line = diff.new.as_ref()
                    .or(diff.old.as_ref())
                    .map(|e| e.line_start)
                    .unwrap_or(0);
                let line_end = diff.new.as_ref()
                    .or(diff.old.as_ref())
                    .map(|e| e.line_end)
                    .unwrap_or(0);

                let token_changes = extract_token_changes(diff);
                let pattern_ref = pattern_map.get(&diff.name).cloned();

                let context = if include_context {
                    build_snippet_context(diff, result, context_lines)
                } else {
                    None
                };

                let breaking_reason = if diff.breaking {
                    Some(breaking_reason_for(diff))
                } else {
                    None
                };

                // For JSON: use qualified name (parent.name) for clarity
                let display_name = if is_json {
                    let entity = diff.new.as_ref().or(diff.old.as_ref());
                    if let Some(e) = entity {
                        if let Some(ref parent) = e.parent {
                            format!("{}.{}", parent, diff.name)
                        } else {
                            diff.name.clone()
                        }
                    } else {
                        diff.name.clone()
                    }
                } else {
                    diff.name.clone()
                };

                let output_entity = OutputEntity {
                    change: change_str,
                    name: display_name,
                    kind: diff.kind.clone(),
                    line,
                    line_end,
                    sig_changed: diff.sig_changed,
                    body_changed: diff.body_changed,
                    breaking: diff.breaking,
                    breaking_reason: breaking_reason.clone(),
                    pattern_ref,
                    token_changes,
                    old_name: diff.old_name.clone(),
                    context,
                };

                file_entities.entry(diff.file.clone())
                    .or_default()
                    .push(output_entity);

                if diff.breaking {
                    breaking_entries.push(BreakingEntry {
                        entity: diff.name.clone(),
                        kind: diff.kind.clone(),
                        file: diff.file.clone(),
                        line,
                        reason: breaking_reason.unwrap_or_default(),
                        external_callers: None,
                        callers_in_diff: None,
                    });
                }
            }
        }

        // Build file sections with per-file summaries
        let mut files: Vec<FileSection> = Vec::new();
        for (file, entities) in &file_entities {
            let mut summary = FileSummary {
                added: 0,
                modified: 0,
                removed: 0,
                renamed: 0,
                formatting_only: 0,
            };
            for e in entities {
                match e.change.as_str() {
                    "added" => summary.added += 1,
                    "modified" => summary.modified += 1,
                    "removed" => summary.removed += 1,
                    "renamed" => summary.renamed += 1,
                    "formatting_only" => summary.formatting_only += 1,
                    _ => {}
                }
            }
            files.push(FileSection {
                file: file.clone(),
                summary,
                entities: entities.clone(),
            });
        }

        // Compute unique files changed
        let mut all_files: HashSet<String> = HashSet::new();
        for f in &files {
            all_files.insert(f.file.clone());
        }
        for m in &moves {
            all_files.insert(m.from_file.clone());
            all_files.insert(m.to_file.clone());
        }

        // Compute output summary counts from file entities
        let (mut added, mut modified, mut removed, mut renamed, mut formatting_only) = (0, 0, 0, 0, 0);
        for f in &files {
            added += f.summary.added;
            modified += f.summary.modified;
            removed += f.summary.removed;
            renamed += f.summary.renamed;
            formatting_only += f.summary.formatting_only;
        }

        let has_breaking = !breaking_entries.is_empty();

        let natural_language = build_natural_language(
            output_patterns.len(),
            moves.len(),
            added,
            removed,
            modified,
            renamed,
            formatting_only,
            has_breaking,
        );

        let summary_line = build_summary_line(&output_patterns, &breaking_entries, &files);

        let summary = OutputSummary {
            files_changed: all_files.len(),
            patterns: output_patterns.len(),
            moves: moves.len(),
            added,
            removed,
            modified,
            renamed,
            formatting_only,
            has_breaking,
            natural_language,
            summary_line,
        };

        let meta = Meta {
            base_ref: result.base_ref.clone(),
            head_ref: result.head_ref.clone(),
            base_sha: None,
            head_sha: None,
            generated_at: String::new(),
            sigil_version: env!("CARGO_PKG_VERSION").to_string(),
        };

        DiffOutput {
            meta,
            summary,
            breaking: breaking_entries,
            patterns: output_patterns,
            moves,
            files,
            groups: None,
        }
    }
}

/// Convert ChangeKind to snake_case string for output.
fn change_kind_to_string(kind: &ChangeKind) -> String {
    match kind {
        ChangeKind::Added => "added".to_string(),
        ChangeKind::Removed => "removed".to_string(),
        ChangeKind::Modified => "modified".to_string(),
        ChangeKind::Moved => "moved".to_string(),
        ChangeKind::Renamed => "renamed".to_string(),
        ChangeKind::FormattingOnly => "formatting_only".to_string(),
    }
}

/// Derive a human-readable breaking reason for an entity diff.
fn breaking_reason_for(diff: &EntityDiff) -> String {
    match &diff.change {
        ChangeKind::Removed => "public entity removed".to_string(),
        ChangeKind::Renamed => "public entity renamed".to_string(),
        ChangeKind::Moved => {
            if diff.sig_changed == Some(true) {
                "moved with signature change".to_string()
            } else {
                "public entity moved".to_string()
            }
        }
        ChangeKind::Modified => "public signature changed".to_string(),
        _ => "breaking change".to_string(),
    }
}

/// Extract token changes from EntityDiff's change_details.
fn extract_token_changes(diff: &EntityDiff) -> Vec<TokenChange> {
    let details = match &diff.change_details {
        Some(d) => d,
        None => return Vec::new(),
    };

    let mut changes = Vec::new();
    for detail in details {
        let (change_type, from, to) = match detail.kind {
            DetailKind::ValueChanged => {
                let (from, to) = parse_arrow_description(&detail.description);
                ("value_changed".to_string(), from, to)
            }
            DetailKind::IdentifierChanged => {
                let (from, to) = parse_arrow_description(&detail.description);
                ("identifier_renamed".to_string(), from, to)
            }
            DetailKind::ArgumentAdded => {
                ("param_added".to_string(), String::new(), detail.description.clone())
            }
            DetailKind::ArgumentRemoved => {
                ("param_removed".to_string(), detail.description.clone(), String::new())
            }
            // Skip line-level and comment changes
            DetailKind::LineAdded | DetailKind::LineRemoved | DetailKind::Comment => continue,
        };
        changes.push(TokenChange { change_type, from, to });
    }
    changes
}

/// Parse "old → new" description format into (from, to) pair.
fn parse_arrow_description(desc: &str) -> (String, String) {
    let separator = " \u{2192} ";
    if let Some(idx) = desc.find(separator) {
        let from = desc[..idx].to_string();
        let to = desc[idx + separator.len()..].to_string();
        (from, to)
    } else {
        (desc.to_string(), String::new())
    }
}

/// Build a snippet context from entity diff source maps.
fn build_snippet_context(diff: &EntityDiff, result: &DiffResult, context_lines: usize) -> Option<SnippetContext> {
    let old_sources = result.old_sources.as_ref()?;
    let new_sources = result.new_sources.as_ref()?;

    let (old_entity, new_entity) = match (&diff.old, &diff.new) {
        (Some(o), Some(n)) => (o, n),
        _ => return None,
    };

    // Skip formatting_only
    if diff.change == ChangeKind::FormattingOnly {
        return None;
    }

    let old_src = old_sources.get(&old_entity.file)?;
    let new_src = new_sources.get(&new_entity.file)?;

    let base_snippet = inline_diff::extract_entity_text(
        old_src, old_entity.line_start, old_entity.line_end,
    );
    let head_snippet = inline_diff::extract_entity_text(
        new_src, new_entity.line_start, new_entity.line_end,
    );

    let language = detect_language_from_file(&new_entity.file);

    let snippet_kind = match (diff.sig_changed, diff.body_changed) {
        (Some(true), Some(false)) => "signature".to_string(),
        (Some(false), Some(true)) => "diff".to_string(),
        (Some(true), Some(true)) => "full".to_string(),
        _ => "full".to_string(),
    };

    let hunks = inline_diff::compute_inline_diff_hunked(&base_snippet, &head_snippet, context_lines);

    Some(SnippetContext {
        base_snippet,
        head_snippet,
        language,
        snippet_kind,
        hunks,
    })
}

/// Detect language string from file extension.
fn detect_language_from_file(file: &str) -> String {
    let ext = file.rsplit('.').next().unwrap_or("");
    match ext {
        "py" => "python",
        "rs" => "rust",
        "js" => "javascript",
        "ts" => "typescript",
        "tsx" => "typescript",
        "jsx" => "javascript",
        "go" => "go",
        "rb" => "ruby",
        "java" => "java",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "cs" => "csharp",
        "swift" => "swift",
        "kt" => "kotlin",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        _ => ext,
    }.to_string()
}

/// Build natural language summary string.
fn build_natural_language(
    patterns: usize,
    moves: usize,
    added: usize,
    removed: usize,
    modified: usize,
    renamed: usize,
    formatting_only: usize,
    has_breaking: bool,
) -> String {
    let mut phrases: Vec<String> = Vec::new();

    if patterns > 0 {
        phrases.push(format!(
            "{} cross-file pattern{}",
            patterns,
            if patterns == 1 { "" } else { "s" }
        ));
    }
    if moves > 0 {
        phrases.push(format!(
            "{} entity move{}",
            moves,
            if moves == 1 { "" } else { "s" }
        ));
    }

    let mut counts: Vec<String> = Vec::new();
    if added > 0 { counts.push(format!("{} added", added)); }
    if modified > 0 { counts.push(format!("{} modified", modified)); }
    if removed > 0 { counts.push(format!("{} removed", removed)); }
    if renamed > 0 { counts.push(format!("{} renamed", renamed)); }
    if formatting_only > 0 { counts.push(format!("{} formatting only", formatting_only)); }

    if !counts.is_empty() {
        phrases.push(counts.join(", "));
    }

    if phrases.is_empty() {
        return "No structural changes.".to_string();
    }

    let mut result = phrases.join(", ");

    if has_breaking {
        result.push_str(" with breaking changes");
    }

    result.push('.');
    result
}

fn build_summary_line(
    patterns: &[OutputPattern],
    breaking: &[BreakingEntry],
    files: &[FileSection],
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();

    // Patterns first (most notable cross-file changes)
    for pat in patterns.iter().take(2) {
        if let (Some(from), Some(to)) = (&pat.from_glob, &pat.to_glob) {
            parts.push(format!("renamed {} \u{2192} {} across {} files", from, to, pat.file_count));
        } else if let Some(name) = &pat.entity_name {
            parts.push(format!("{} modified across {} files", name, pat.file_count));
        }
    }

    // Breaking changes
    for b in breaking.iter().take(2) {
        parts.push(format!("{} ({})", b.entity, b.reason));
    }
    if breaking.len() > 2 {
        parts.push(format!("+{} more breaking", breaking.len() - 2));
    }

    // Top modified entities (if no patterns/breaking yet)
    if parts.is_empty() {
        for section in files.iter().take(3) {
            let names: Vec<&str> = section.entities.iter()
                .filter(|e| e.change != "formatting_only")
                .take(3)
                .map(|e| e.name.as_str())
                .collect();
            if !names.is_empty() {
                parts.push(format!("{} in {}", names.join(", "), section.file));
            }
        }
    }

    if parts.is_empty() {
        return None;
    }

    Some(parts.join("; "))
}

/// Enrich breaking entries with caller information from the sigil index.
/// `callers_fn` maps an entity name to a list of (file, line, caller_name) tuples.
/// `diff_files` is the set of files touched by the diff.
pub fn enrich_breaking_with_callers(
    breaking: &mut Vec<BreakingEntry>,
    callers_fn: &dyn Fn(&str) -> Vec<(String, u32, String)>,
    diff_files: &std::collections::HashSet<String>,
) {
    for entry in breaking.iter_mut() {
        let all_callers = callers_fn(&entry.entity);
        let mut in_diff = 0usize;
        let mut external = Vec::new();

        for (file, line, name) in all_callers {
            if diff_files.contains(&file) {
                in_diff += 1;
            } else {
                external.push(CallerInfo { file, line, name });
            }
        }

        entry.callers_in_diff = Some(in_diff);
        entry.external_callers = Some(external);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff_json::{DiffResult, CrossFilePattern, EntityDiff, ChangeKind};
    use crate::change_detail::{ChangeDetail, DetailKind};

    fn make_entity(file: &str, name: &str, kind: &str, line_start: u32, line_end: u32) -> Entity {
        Entity {
            file: file.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            line_start,
            line_end,
            parent: None,
            qualified_name: None,
            sig: Some(format!("def {}():", name)),
            meta: None,
            body_hash: Some("bh1".to_string()),
            sig_hash: Some("sh1".to_string()),
            struct_hash: "st1".to_string(),
            visibility: None,
            rank: None,
            blast_radius: None,
            doc: None,
        }
    }

    fn make_diff_result(entities: Vec<EntityDiff>, patterns: Vec<CrossFilePattern>) -> DiffResult {
        let summary = DiffResult::compute_summary(&entities);
        DiffResult {
            base_ref: "abc123".to_string(),
            head_ref: "def456".to_string(),
            base_sha: None,
            head_sha: None,
            entities,
            patterns,
            summary,
            old_sources: None,
            new_sources: None,
        }
    }

    #[test]
    fn basic_conversion_added_modified_removed() {
        let entities = vec![
            EntityDiff {
                change: ChangeKind::Added,
                name: "new_func".into(),
                kind: "function".into(),
                file: "a.py".into(),
                old_file: None,
                old_name: None,
                sig_changed: None,
                body_changed: None,
                breaking: false,
                breaking_reason: None,
                old: None,
                new: Some(make_entity("a.py", "new_func", "function", 10, 20)),
                inline_diff: None,
                change_details: None,
            },
            EntityDiff {
                change: ChangeKind::Modified,
                name: "mod_func".into(),
                kind: "function".into(),
                file: "a.py".into(),
                old_file: None,
                old_name: None,
                sig_changed: Some(true),
                body_changed: Some(false),
                breaking: true,
                breaking_reason: Some("sig_changed".into()),
                old: Some(make_entity("a.py", "mod_func", "function", 1, 5)),
                new: Some(make_entity("a.py", "mod_func", "function", 1, 5)),
                inline_diff: None,
                change_details: None,
            },
            EntityDiff {
                change: ChangeKind::Removed,
                name: "old_func".into(),
                kind: "function".into(),
                file: "b.py".into(),
                old_file: None,
                old_name: None,
                sig_changed: None,
                body_changed: None,
                breaking: true,
                breaking_reason: Some("removed".into()),
                old: Some(make_entity("b.py", "old_func", "function", 5, 15)),
                new: None,
                inline_diff: None,
                change_details: None,
            },
        ];

        let result = make_diff_result(entities, vec![]);
        let output = DiffOutput::from_result(&result, false, 3);

        assert_eq!(output.summary.added, 1);
        assert_eq!(output.summary.modified, 1);
        assert_eq!(output.summary.removed, 1);
        assert!(output.summary.has_breaking);
        assert_eq!(output.files.len(), 2);
        assert_eq!(output.moves.len(), 0);

        // Check file sections are sorted (BTreeMap ordering)
        assert_eq!(output.files[0].file, "a.py");
        assert_eq!(output.files[1].file, "b.py");
        assert_eq!(output.files[0].entities.len(), 2);
        assert_eq!(output.files[1].entities.len(), 1);

        // Check per-file summary
        assert_eq!(output.files[0].summary.added, 1);
        assert_eq!(output.files[0].summary.modified, 1);
        assert_eq!(output.files[1].summary.removed, 1);
    }

    #[test]
    fn move_separation_cross_file() {
        let entities = vec![
            EntityDiff {
                change: ChangeKind::Moved,
                name: "moved_func".into(),
                kind: "function".into(),
                file: "new.py".into(),
                old_file: Some("old.py".into()),
                old_name: None,
                sig_changed: None,
                body_changed: None,
                breaking: false,
                breaking_reason: None,
                old: Some(make_entity("old.py", "moved_func", "function", 1, 10)),
                new: Some(make_entity("new.py", "moved_func", "function", 5, 15)),
                inline_diff: None,
                change_details: None,
            },
        ];

        let result = make_diff_result(entities, vec![]);
        let output = DiffOutput::from_result(&result, false, 3);

        assert_eq!(output.moves.len(), 1);
        assert_eq!(output.moves[0].entity, "moved_func");
        assert_eq!(output.moves[0].from_file, "old.py");
        assert_eq!(output.moves[0].to_file, "new.py");
        assert_eq!(output.moves[0].from_line, 1);
        assert_eq!(output.moves[0].to_line, 5);
        assert!((output.moves[0].confidence - 1.0).abs() < f64::EPSILON);
        // Should NOT appear in files[]
        assert!(output.files.is_empty());
    }

    #[test]
    fn same_file_move_reclassified_as_modified() {
        let entities = vec![
            EntityDiff {
                change: ChangeKind::Moved,
                name: "func".into(),
                kind: "function".into(),
                file: "a.py".into(),
                old_file: Some("a.py".into()), // same file
                old_name: None,
                sig_changed: Some(true),
                body_changed: Some(false),
                breaking: false,
                breaking_reason: None,
                old: Some(make_entity("a.py", "func", "function", 1, 5)),
                new: Some(make_entity("a.py", "func", "function", 10, 15)),
                inline_diff: None,
                change_details: None,
            },
        ];

        let result = make_diff_result(entities, vec![]);
        let output = DiffOutput::from_result(&result, false, 3);

        // Should NOT be in moves
        assert_eq!(output.moves.len(), 0);
        // Should be in files as "modified"
        assert_eq!(output.files.len(), 1);
        assert_eq!(output.files[0].entities[0].change, "modified");
    }

    #[test]
    fn same_file_move_no_old_file_reclassified() {
        let entities = vec![
            EntityDiff {
                change: ChangeKind::Moved,
                name: "func".into(),
                kind: "function".into(),
                file: "a.py".into(),
                old_file: None, // no old_file means same-file
                old_name: None,
                sig_changed: None,
                body_changed: None,
                breaking: false,
                breaking_reason: None,
                old: Some(make_entity("a.py", "func", "function", 1, 5)),
                new: Some(make_entity("a.py", "func", "function", 10, 15)),
                inline_diff: None,
                change_details: None,
            },
        ];

        let result = make_diff_result(entities, vec![]);
        let output = DiffOutput::from_result(&result, false, 3);

        assert_eq!(output.moves.len(), 0);
        assert_eq!(output.files.len(), 1);
        assert_eq!(output.files[0].entities[0].change, "modified");
    }

    #[test]
    fn pattern_id_assignment_and_tagging() {
        let entities = vec![
            EntityDiff {
                change: ChangeKind::Modified,
                name: "init".into(),
                kind: "function".into(),
                file: "a.py".into(),
                old_file: None,
                old_name: None,
                sig_changed: Some(true),
                body_changed: Some(false),
                breaking: false,
                breaking_reason: None,
                old: Some(make_entity("a.py", "init", "function", 1, 5)),
                new: Some(make_entity("a.py", "init", "function", 1, 5)),
                inline_diff: None,
                change_details: None,
            },
            EntityDiff {
                change: ChangeKind::Modified,
                name: "init".into(),
                kind: "function".into(),
                file: "b.py".into(),
                old_file: None,
                old_name: None,
                sig_changed: Some(true),
                body_changed: Some(false),
                breaking: false,
                breaking_reason: None,
                old: Some(make_entity("b.py", "init", "function", 1, 5)),
                new: Some(make_entity("b.py", "init", "function", 1, 5)),
                inline_diff: None,
                change_details: None,
            },
        ];

        let patterns = vec![
            CrossFilePattern {
                description: "same modified applied to init across 2 files".into(),
                entity_names: vec!["init".into(), "init".into()],
                files: vec!["a.py".into(), "b.py".into()],
                change: ChangeKind::Modified,
            },
        ];

        let result = make_diff_result(entities, patterns);
        let output = DiffOutput::from_result(&result, false, 3);

        assert_eq!(output.patterns.len(), 1);
        assert_eq!(output.patterns[0].id, "pat_1");
        assert_eq!(output.patterns[0].pattern_type, "body_identical");
        assert_eq!(output.patterns[0].entity_kind, "function");
        assert_eq!(output.patterns[0].file_count, 2);

        // Check entities are tagged with pattern_ref
        for file_section in &output.files {
            for entity in &file_section.entities {
                if entity.name == "init" {
                    assert_eq!(entity.pattern_ref, Some("pat_1".to_string()));
                }
            }
        }
    }

    #[test]
    fn rename_pattern_type() {
        let patterns = vec![
            CrossFilePattern {
                description: "same renamed applied to foo across 2 files".into(),
                entity_names: vec!["foo".into()],
                files: vec!["a.py".into(), "b.py".into()],
                change: ChangeKind::Renamed,
            },
        ];

        let entities = vec![
            EntityDiff {
                change: ChangeKind::Renamed,
                name: "foo".into(),
                kind: "function".into(),
                file: "a.py".into(),
                old_file: None,
                old_name: Some("bar".into()),
                sig_changed: None,
                body_changed: None,
                breaking: true,
                breaking_reason: Some("renamed".into()),
                old: Some(make_entity("a.py", "bar", "function", 1, 5)),
                new: Some(make_entity("a.py", "foo", "function", 1, 5)),
                inline_diff: None,
                change_details: None,
            },
        ];

        let result = make_diff_result(entities, patterns);
        let output = DiffOutput::from_result(&result, false, 3);

        assert_eq!(output.patterns[0].pattern_type, "rename");
        assert!(output.patterns[0].entity_name.is_none());
    }

    #[test]
    fn breaking_array_populated() {
        let entities = vec![
            EntityDiff {
                change: ChangeKind::Removed,
                name: "old_api".into(),
                kind: "function".into(),
                file: "api.py".into(),
                old_file: None,
                old_name: None,
                sig_changed: None,
                body_changed: None,
                breaking: true,
                breaking_reason: Some("removed".into()),
                old: Some(make_entity("api.py", "old_api", "function", 10, 20)),
                new: None,
                inline_diff: None,
                change_details: None,
            },
            EntityDiff {
                change: ChangeKind::Modified,
                name: "process".into(),
                kind: "function".into(),
                file: "api.py".into(),
                old_file: None,
                old_name: None,
                sig_changed: Some(true),
                body_changed: Some(false),
                breaking: true,
                breaking_reason: Some("sig_changed".into()),
                old: Some(make_entity("api.py", "process", "function", 25, 40)),
                new: Some(make_entity("api.py", "process", "function", 25, 40)),
                inline_diff: None,
                change_details: None,
            },
        ];

        let result = make_diff_result(entities, vec![]);
        let output = DiffOutput::from_result(&result, false, 3);

        assert_eq!(output.breaking.len(), 2);
        assert_eq!(output.breaking[0].entity, "old_api");
        assert_eq!(output.breaking[0].reason, "public entity removed");
        assert_eq!(output.breaking[1].entity, "process");
        assert_eq!(output.breaking[1].reason, "public signature changed");
    }

    #[test]
    fn breaking_entry_for_moved_entity_uses_destination_file() {
        let entities = vec![
            EntityDiff {
                change: ChangeKind::Moved,
                name: "func".into(),
                kind: "function".into(),
                file: "new.py".into(),
                old_file: Some("old.py".into()),
                old_name: None,
                sig_changed: Some(true),
                body_changed: Some(false),
                breaking: true,
                breaking_reason: Some("moved".into()),
                old: Some(make_entity("old.py", "func", "function", 1, 5)),
                new: Some(make_entity("new.py", "func", "function", 10, 20)),
                inline_diff: None,
                change_details: None,
            },
        ];

        let result = make_diff_result(entities, vec![]);
        let output = DiffOutput::from_result(&result, false, 3);

        assert_eq!(output.breaking.len(), 1);
        assert_eq!(output.breaking[0].file, "new.py"); // destination file
        assert_eq!(output.breaking[0].line, 10);
    }

    #[test]
    fn natural_language_various_combinations() {
        // Only added
        let nl = build_natural_language(0, 0, 3, 0, 0, 0, 0, false);
        assert_eq!(nl, "3 added.");

        // Mixed
        let nl = build_natural_language(0, 0, 1, 2, 3, 0, 0, false);
        assert_eq!(nl, "1 added, 3 modified, 2 removed.");

        // With patterns and moves
        let nl = build_natural_language(2, 1, 0, 0, 1, 0, 0, false);
        assert_eq!(nl, "2 cross-file patterns, 1 entity move, 1 modified.");

        // With breaking
        let nl = build_natural_language(0, 0, 0, 1, 0, 0, 0, true);
        assert_eq!(nl, "1 removed with breaking changes.");

        // Singular patterns/moves
        let nl = build_natural_language(1, 1, 0, 0, 0, 0, 0, false);
        assert_eq!(nl, "1 cross-file pattern, 1 entity move.");
    }

    #[test]
    fn empty_diff_no_structural_changes() {
        let result = make_diff_result(vec![], vec![]);
        let output = DiffOutput::from_result(&result, false, 3);

        assert!(output.files.is_empty());
        assert!(output.moves.is_empty());
        assert!(output.patterns.is_empty());
        assert!(output.breaking.is_empty());
        assert_eq!(output.summary.natural_language, "No structural changes.");
        assert_eq!(output.summary.files_changed, 0);
    }

    #[test]
    fn token_change_mapping_from_change_detail() {
        let entities = vec![
            EntityDiff {
                change: ChangeKind::Modified,
                name: "func".into(),
                kind: "function".into(),
                file: "a.py".into(),
                old_file: None,
                old_name: None,
                sig_changed: Some(true),
                body_changed: Some(false),
                breaking: false,
                breaking_reason: None,
                old: Some(make_entity("a.py", "func", "function", 1, 5)),
                new: Some(make_entity("a.py", "func", "function", 1, 5)),
                inline_diff: None,
                change_details: Some(vec![
                    ChangeDetail {
                        kind: DetailKind::ValueChanged,
                        description: "true \u{2192} false".to_string(),
                    },
                    ChangeDetail {
                        kind: DetailKind::IdentifierChanged,
                        description: "validate_card \u{2192} check_card".to_string(),
                    },
                    ChangeDetail {
                        kind: DetailKind::ArgumentAdded,
                        description: "+ key=None".to_string(),
                    },
                    ChangeDetail {
                        kind: DetailKind::ArgumentRemoved,
                        description: "- old_param".to_string(),
                    },
                    ChangeDetail {
                        kind: DetailKind::LineAdded,
                        description: "+ new line".to_string(),
                    },
                    ChangeDetail {
                        kind: DetailKind::Comment,
                        description: "comment updated".to_string(),
                    },
                ]),
            },
        ];

        let result = make_diff_result(entities, vec![]);
        let output = DiffOutput::from_result(&result, false, 3);

        let entity = &output.files[0].entities[0];
        // Should have 4 token changes (LineAdded and Comment are skipped)
        assert_eq!(entity.token_changes.len(), 4);

        assert_eq!(entity.token_changes[0].change_type, "value_changed");
        assert_eq!(entity.token_changes[0].from, "true");
        assert_eq!(entity.token_changes[0].to, "false");

        assert_eq!(entity.token_changes[1].change_type, "identifier_renamed");
        assert_eq!(entity.token_changes[1].from, "validate_card");
        assert_eq!(entity.token_changes[1].to, "check_card");

        assert_eq!(entity.token_changes[2].change_type, "param_added");
        assert_eq!(entity.token_changes[2].from, "");
        assert_eq!(entity.token_changes[2].to, "+ key=None");

        assert_eq!(entity.token_changes[3].change_type, "param_removed");
        assert_eq!(entity.token_changes[3].from, "- old_param");
        assert_eq!(entity.token_changes[3].to, "");
    }

    #[test]
    fn meta_fields_populated() {
        let result = make_diff_result(vec![], vec![]);
        let output = DiffOutput::from_result(&result, false, 3);

        assert_eq!(output.meta.base_ref, "abc123");
        assert_eq!(output.meta.head_ref, "def456");
        assert!(!output.meta.sigil_version.is_empty());
    }

    #[test]
    fn move_confidence_varies_with_changes() {
        // No changes: confidence 1.0
        let entities = vec![
            EntityDiff {
                change: ChangeKind::Moved,
                name: "f1".into(),
                kind: "function".into(),
                file: "new.py".into(),
                old_file: Some("old.py".into()),
                old_name: None,
                sig_changed: None,
                body_changed: None,
                breaking: false,
                breaking_reason: None,
                old: Some(make_entity("old.py", "f1", "function", 1, 5)),
                new: Some(make_entity("new.py", "f1", "function", 1, 5)),
                inline_diff: None,
                change_details: None,
            },
        ];
        let result = make_diff_result(entities, vec![]);
        let output = DiffOutput::from_result(&result, false, 3);
        assert!((output.moves[0].confidence - 1.0).abs() < f64::EPSILON);

        // Sig changed only: confidence 0.8
        let entities = vec![
            EntityDiff {
                change: ChangeKind::Moved,
                name: "f2".into(),
                kind: "function".into(),
                file: "new.py".into(),
                old_file: Some("old.py".into()),
                old_name: None,
                sig_changed: Some(true),
                body_changed: Some(false),
                breaking: false,
                breaking_reason: None,
                old: Some(make_entity("old.py", "f2", "function", 1, 5)),
                new: Some(make_entity("new.py", "f2", "function", 1, 5)),
                inline_diff: None,
                change_details: None,
            },
        ];
        let result = make_diff_result(entities, vec![]);
        let output = DiffOutput::from_result(&result, false, 3);
        assert!((output.moves[0].confidence - 0.8).abs() < f64::EPSILON);

        // Both changed: confidence 0.6
        let entities = vec![
            EntityDiff {
                change: ChangeKind::Moved,
                name: "f3".into(),
                kind: "function".into(),
                file: "new.py".into(),
                old_file: Some("old.py".into()),
                old_name: None,
                sig_changed: Some(true),
                body_changed: Some(true),
                breaking: false,
                breaking_reason: None,
                old: Some(make_entity("old.py", "f3", "function", 1, 5)),
                new: Some(make_entity("new.py", "f3", "function", 1, 5)),
                inline_diff: None,
                change_details: None,
            },
        ];
        let result = make_diff_result(entities, vec![]);
        let output = DiffOutput::from_result(&result, false, 3);
        assert!((output.moves[0].confidence - 0.6).abs() < f64::EPSILON);
    }

    #[test]
    fn formatting_only_counted() {
        let entities = vec![
            EntityDiff {
                change: ChangeKind::FormattingOnly,
                name: "fmt_func".into(),
                kind: "function".into(),
                file: "a.py".into(),
                old_file: None,
                old_name: None,
                sig_changed: None,
                body_changed: None,
                breaking: false,
                breaking_reason: None,
                old: Some(make_entity("a.py", "fmt_func", "function", 1, 5)),
                new: Some(make_entity("a.py", "fmt_func", "function", 1, 5)),
                inline_diff: None,
                change_details: None,
            },
        ];

        let result = make_diff_result(entities, vec![]);
        let output = DiffOutput::from_result(&result, false, 3);

        assert_eq!(output.summary.formatting_only, 1);
        assert_eq!(output.files[0].summary.formatting_only, 1);
        assert_eq!(output.files[0].entities[0].change, "formatting_only");
    }

    #[test]
    fn formatting_only_implies_exit_code_zero() {
        // When ALL entities are formatting_only, the structural counts
        // (added, removed, modified, moves, renamed) must all be 0
        // and has_breaking must be false — matching exit code 0 logic.
        let entities = vec![
            EntityDiff {
                change: ChangeKind::FormattingOnly,
                name: "fn_a".into(),
                kind: "function".into(),
                file: "a.py".into(),
                old_file: None,
                old_name: None,
                sig_changed: None,
                body_changed: None,
                breaking: false,
                breaking_reason: None,
                old: Some(make_entity("a.py", "fn_a", "function", 1, 5)),
                new: Some(make_entity("a.py", "fn_a", "function", 1, 5)),
                inline_diff: None,
                change_details: None,
            },
            EntityDiff {
                change: ChangeKind::FormattingOnly,
                name: "fn_b".into(),
                kind: "function".into(),
                file: "b.py".into(),
                old_file: None,
                old_name: None,
                sig_changed: None,
                body_changed: None,
                breaking: false,
                breaking_reason: None,
                old: Some(make_entity("b.py", "fn_b", "function", 1, 10)),
                new: Some(make_entity("b.py", "fn_b", "function", 1, 10)),
                inline_diff: None,
                change_details: None,
            },
        ];

        let result = make_diff_result(entities, vec![]);
        let output = DiffOutput::from_result(&result, false, 3);

        // Structural counts must be 0
        assert_eq!(output.summary.added, 0);
        assert_eq!(output.summary.removed, 0);
        assert_eq!(output.summary.modified, 0);
        assert_eq!(output.summary.moves, 0);
        assert_eq!(output.summary.renamed, 0);
        // formatting_only counted separately
        assert_eq!(output.summary.formatting_only, 2);
        // Not breaking
        assert!(!output.summary.has_breaking);
        // Exit code 0 condition: !has_breaking && (added + removed + modified + moves + renamed == 0)
        let s = &output.summary;
        let exit_code = if s.has_breaking { 2 }
            else if s.added + s.removed + s.modified + s.moves + s.renamed > 0 { 1 }
            else { 0 };
        assert_eq!(exit_code, 0);
    }

    #[test]
    fn parse_arrow_description_works() {
        let (from, to) = parse_arrow_description("true \u{2192} false");
        assert_eq!(from, "true");
        assert_eq!(to, "false");

        let (from, to) = parse_arrow_description("no arrow here");
        assert_eq!(from, "no arrow here");
        assert_eq!(to, "");
    }

    #[test]
    fn language_detection() {
        assert_eq!(detect_language_from_file("foo.py"), "python");
        assert_eq!(detect_language_from_file("bar.rs"), "rust");
        assert_eq!(detect_language_from_file("baz.ts"), "typescript");
        assert_eq!(detect_language_from_file("qux.go"), "go");
        assert_eq!(detect_language_from_file("file.json"), "json");
        assert_eq!(detect_language_from_file("file.yaml"), "yaml");
        assert_eq!(detect_language_from_file("file.unknown"), "unknown");
    }

    #[test]
    fn context_snippets_when_sources_present() {
        use std::collections::HashMap;

        let old_source = "def func():\n    return True\n";
        let new_source = "def func(x):\n    return True\n";

        let mut old_sources = HashMap::new();
        old_sources.insert("a.py".to_string(), old_source.to_string());
        let mut new_sources = HashMap::new();
        new_sources.insert("a.py".to_string(), new_source.to_string());

        let entities = vec![
            EntityDiff {
                change: ChangeKind::Modified,
                name: "func".into(),
                kind: "function".into(),
                file: "a.py".into(),
                old_file: None,
                old_name: None,
                sig_changed: Some(true),
                body_changed: Some(false),
                breaking: false,
                breaking_reason: None,
                old: Some(make_entity("a.py", "func", "function", 1, 2)),
                new: Some(make_entity("a.py", "func", "function", 1, 2)),
                inline_diff: None,
                change_details: None,
            },
        ];

        let summary = DiffResult::compute_summary(&entities);
        let result = DiffResult {
            base_ref: "abc".into(),
            head_ref: "def".into(),
            base_sha: None,
            head_sha: None,
            entities,
            patterns: vec![],
            summary,
            old_sources: Some(old_sources),
            new_sources: Some(new_sources),
        };

        let output = DiffOutput::from_result(&result, true, 3);
        let entity = &output.files[0].entities[0];

        assert!(entity.context.is_some());
        let ctx = entity.context.as_ref().unwrap();
        assert_eq!(ctx.snippet_kind, "signature");
        assert_eq!(ctx.language, "python");
        assert!(ctx.base_snippet.contains("return True"));
        assert!(ctx.head_snippet.contains("return True"));
    }

    #[test]
    fn derived_entities_excluded_from_output() {
        let old_entity = make_entity("a.json", "text", "property", 2, 2);
        let mut new_entity = make_entity("a.json", "text", "property", 2, 2);
        new_entity.struct_hash = "st_changed".to_string();

        let mut old_derived = make_entity("a.json", "_parsed_text", "property", 3, 3);
        old_derived.meta = Some(vec!["derived".to_string()]);
        let mut new_derived = make_entity("a.json", "_parsed_text", "property", 3, 3);
        new_derived.meta = Some(vec!["derived".to_string()]);
        new_derived.struct_hash = "st_derived_changed".to_string();

        let entities = vec![
            EntityDiff {
                change: ChangeKind::Modified,
                name: "text".into(),
                kind: "property".into(),
                file: "a.json".into(),
                old_file: None,
                old_name: None,
                sig_changed: Some(false),
                body_changed: Some(true),
                breaking: false,
                breaking_reason: None,
                old: Some(old_entity),
                new: Some(new_entity),
                inline_diff: None,
                change_details: None,
            },
            EntityDiff {
                change: ChangeKind::Modified,
                name: "_parsed_text".into(),
                kind: "property".into(),
                file: "a.json".into(),
                old_file: None,
                old_name: None,
                sig_changed: Some(false),
                body_changed: Some(true),
                breaking: false,
                breaking_reason: None,
                old: Some(old_derived),
                new: Some(new_derived),
                inline_diff: None,
                change_details: None,
            },
        ];

        let result = make_diff_result(entities, vec![]);
        let output = DiffOutput::from_result(&result, false, 3);

        // Only non-derived entity should appear
        assert_eq!(output.files.len(), 1);
        assert_eq!(output.files[0].entities.len(), 1);
        assert_eq!(output.files[0].entities[0].name, "text");

        // Summary should only count non-derived
        assert_eq!(output.summary.modified, 1);
    }
}
