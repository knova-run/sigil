use crate::entity::Entity;
use std::collections::{HashMap, HashSet};

/// Compute normalized name similarity (0.0–1.0) using character-level diff ratio.
fn name_similarity(a: &str, b: &str) -> f64 {
    if a == b { return 1.0; }
    if a.is_empty() || b.is_empty() { return 0.0; }
    let diff = similar::TextDiff::from_chars(a, b);
    diff.ratio() as f64
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchKind {
    ExactMatch,   // same file + same parent + same name
    Moved,        // same parent + same name, different file
    Renamed,      // different name, same body_hash
    Added,        // only in new
    Removed,      // only in old
}

#[derive(Debug, Clone)]
pub struct EntityMatch {
    pub old: Option<Entity>,
    pub new: Option<Entity>,
    pub match_kind: MatchKind,
    #[allow(dead_code)]
    pub confidence: f64, // 0.0-1.0; always 1.0 until fuzzy matching is added
}

/// Match entities across two versions.
///
/// Strategy (layered):
/// 1. Exact match: same (file, parent, name) in both → ExactMatch
/// 2. Name match across files: same (parent, name), different file → Moved
/// 3. Body hash match: different name, same body_hash (non-null) → Renamed
/// 3.5. Fuzzy rename: same kind, same file, same parent, name similarity >= 0.6 → Renamed
/// 4. Remaining old → Removed, remaining new → Added
///
/// Entities with identical struct_hash are considered unchanged and excluded.
pub fn match_entities(old: &[Entity], new: &[Entity]) -> Vec<EntityMatch> {
    let mut matches = Vec::new();
    let mut used_old: HashSet<usize> = HashSet::new();
    let mut used_new: HashSet<usize> = HashSet::new();

    // Pass 1: Exact match (file + parent + name)
    let old_by_key: HashMap<(&str, Option<&str>, &str), usize> = old.iter().enumerate()
        .map(|(i, e)| ((e.file.as_str(), e.parent.as_deref(), e.name.as_str()), i))
        .collect();

    for (ni, ne) in new.iter().enumerate() {
        let key = (ne.file.as_str(), ne.parent.as_deref(), ne.name.as_str());
        if let Some(&oi) = old_by_key.get(&key) {
            if !used_old.contains(&oi) {
                let oe = &old[oi];
                // Skip unchanged entities
                if oe.struct_hash == ne.struct_hash {
                    used_old.insert(oi);
                    used_new.insert(ni);
                    continue;
                }
                matches.push(EntityMatch {
                    old: Some(oe.clone()),
                    new: Some(ne.clone()),
                    match_kind: MatchKind::ExactMatch,
                    confidence: 1.0,
                });
                used_old.insert(oi);
                used_new.insert(ni);
            }
        }
    }

    // Pass 2: Name match across files (Moved) — keyed by (parent, name)
    let remaining_old_by_name: HashMap<(Option<&str>, &str), usize> = old.iter().enumerate()
        .filter(|(i, _)| !used_old.contains(i))
        .map(|(i, e)| ((e.parent.as_deref(), e.name.as_str()), i))
        .collect();

    for (ni, ne) in new.iter().enumerate() {
        if used_new.contains(&ni) { continue; }
        if let Some(&oi) = remaining_old_by_name.get(&(ne.parent.as_deref(), ne.name.as_str())) {
            if !used_old.contains(&oi) {
                matches.push(EntityMatch {
                    old: Some(old[oi].clone()),
                    new: Some(ne.clone()),
                    match_kind: MatchKind::Moved,
                    confidence: 1.0,
                });
                used_old.insert(oi);
                used_new.insert(ni);
            }
        }
    }

    // Pass 3: Body hash match (Renamed)
    // Only for entities with non-null body_hash and non-import kind
    let remaining_old_by_body: HashMap<&str, usize> = old.iter().enumerate()
        .filter(|(i, e)| !used_old.contains(i) && e.body_hash.is_some() && e.kind != "import")
        .filter_map(|(i, e)| e.body_hash.as_deref().map(|bh| (bh, i)))
        .collect();

    for (ni, ne) in new.iter().enumerate() {
        if used_new.contains(&ni) { continue; }
        if ne.kind == "import" || ne.body_hash.is_none() { continue; }
        if let Some(&oi) = remaining_old_by_body.get(ne.body_hash.as_deref().unwrap()) {
            if !used_old.contains(&oi) {
                matches.push(EntityMatch {
                    old: Some(old[oi].clone()),
                    new: Some(ne.clone()),
                    match_kind: MatchKind::Renamed,
                    confidence: 1.0,
                });
                used_old.insert(oi);
                used_new.insert(ni);
            }
        }
    }

    // Pass 3.5: Fuzzy rename — similar name, same kind, same file
    {
        let remaining_old_indices: Vec<usize> = (0..old.len())
            .filter(|i| !used_old.contains(i))
            .filter(|i| old[*i].kind != "import")
            .collect();
        let remaining_new_indices: Vec<usize> = (0..new.len())
            .filter(|i| !used_new.contains(i))
            .filter(|i| new[*i].kind != "import")
            .collect();

        let mut candidates: Vec<(usize, usize, f64)> = Vec::new();
        for &oi in &remaining_old_indices {
            for &ni in &remaining_new_indices {
                let oe = &old[oi];
                let ne = &new[ni];
                if oe.kind != ne.kind || oe.file != ne.file || oe.parent != ne.parent { continue; }
                let sim = name_similarity(&oe.name, &ne.name);
                if sim >= 0.6 {
                    candidates.push((oi, ni, sim));
                }
            }
        }

        // Sort by similarity descending, greedily assign best matches
        candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        for (oi, ni, sim) in candidates {
            if used_old.contains(&oi) || used_new.contains(&ni) { continue; }
            matches.push(EntityMatch {
                old: Some(old[oi].clone()),
                new: Some(new[ni].clone()),
                match_kind: MatchKind::Renamed,
                confidence: sim,
            });
            used_old.insert(oi);
            used_new.insert(ni);
        }
    }

    // Pass 4: Remaining → Added / Removed
    for (oi, oe) in old.iter().enumerate() {
        if !used_old.contains(&oi) {
            matches.push(EntityMatch {
                old: Some(oe.clone()),
                new: None,
                match_kind: MatchKind::Removed,
                confidence: 1.0,
            });
        }
    }
    for (ni, ne) in new.iter().enumerate() {
        if !used_new.contains(&ni) {
            matches.push(EntityMatch {
                old: None,
                new: Some(ne.clone()),
                match_kind: MatchKind::Added,
                confidence: 1.0,
            });
        }
    }

    matches
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::Entity;

    fn entity(file: &str, name: &str, body_hash: Option<&str>, sig_hash: Option<&str>, struct_hash: &str) -> Entity {
        Entity {
            file: file.to_string(),
            name: name.to_string(),
            kind: "function".to_string(),
            line_start: 1, line_end: 5,
            parent: None,
            sig: Some(format!("def {}():", name)),
            meta: None,
            body_hash: body_hash.map(|s| s.to_string()),
            sig_hash: sig_hash.map(|s| s.to_string()),
            struct_hash: struct_hash.to_string(),
            visibility: None,
            rank: None,
            blast_radius: None,
            doc: None,
        }
    }

    #[test]
    fn exact_match_same_file_same_name() {
        let old = vec![entity("a.py", "foo", Some("bh1"), Some("sh1"), "st1")];
        let new = vec![entity("a.py", "foo", Some("bh2"), Some("sh1"), "st2")];
        let matches = match_entities(&old, &new);
        assert_eq!(matches.len(), 1);
        assert!(matches[0].old.is_some());
        assert!(matches[0].new.is_some());
        assert_eq!(matches[0].match_kind, MatchKind::ExactMatch);
    }

    #[test]
    fn moved_entity_detected() {
        let old = vec![entity("a.py", "foo", Some("bh1"), Some("sh1"), "st1")];
        let new = vec![entity("b.py", "foo", Some("bh1"), Some("sh1"), "st1")];
        let matches = match_entities(&old, &new);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].match_kind, MatchKind::Moved);
    }

    #[test]
    fn renamed_entity_detected_by_body_hash() {
        let old = vec![entity("a.py", "foo", Some("bh1"), Some("sh1"), "st1")];
        let new = vec![entity("a.py", "bar", Some("bh1"), Some("sh2"), "st2")];
        let matches = match_entities(&old, &new);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].match_kind, MatchKind::Renamed);
    }

    #[test]
    fn added_and_removed() {
        let old = vec![entity("a.py", "foo", Some("bh1"), Some("sh1"), "st1")];
        let new = vec![entity("a.py", "bar", Some("bh2"), Some("sh2"), "st2")];
        let matches = match_entities(&old, &new);
        let added = matches.iter().filter(|m| m.match_kind == MatchKind::Added).count();
        let removed = matches.iter().filter(|m| m.match_kind == MatchKind::Removed).count();
        assert_eq!(added, 1);
        assert_eq!(removed, 1);
    }

    #[test]
    fn imports_not_renamed() {
        let mut old_e = entity("a.py", "os", None, None, "st1");
        old_e.kind = "import".to_string();
        let mut new_e = entity("a.py", "sys", None, None, "st2");
        new_e.kind = "import".to_string();
        let matches = match_entities(&[old_e], &[new_e]);
        let added = matches.iter().filter(|m| m.match_kind == MatchKind::Added).count();
        let removed = matches.iter().filter(|m| m.match_kind == MatchKind::Removed).count();
        assert_eq!(added, 1);
        assert_eq!(removed, 1);
    }

    #[test]
    fn unchanged_entity_excluded() {
        let e = entity("a.py", "foo", Some("bh1"), Some("sh1"), "st1");
        let matches = match_entities(&[e.clone()], &[e]);
        assert!(matches.is_empty());
    }

    #[test]
    fn fuzzy_rename_detected_by_similar_name() {
        let old = vec![entity("a.py", "THREAD_STORAGE_KEY", Some("bh1"), Some("sh1"), "st1")];
        let new = vec![entity("a.py", "THREAD_STORAGE_PREFIX", Some("bh2"), Some("sh2"), "st2")];
        let matches = match_entities(&old, &new);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].match_kind, MatchKind::Renamed);
        assert!(matches[0].confidence < 1.0, "fuzzy rename should have confidence < 1.0");
    }

    #[test]
    fn fuzzy_rename_not_triggered_for_unrelated_names() {
        let old = vec![entity("a.py", "process_payment", Some("bh1"), Some("sh1"), "st1")];
        let new = vec![entity("a.py", "calculate_tax", Some("bh2"), Some("sh2"), "st2")];
        let matches = match_entities(&old, &new);
        let added = matches.iter().filter(|m| m.match_kind == MatchKind::Added).count();
        let removed = matches.iter().filter(|m| m.match_kind == MatchKind::Removed).count();
        assert_eq!(added, 1);
        assert_eq!(removed, 1);
    }

    #[test]
    fn fuzzy_rename_requires_same_kind() {
        let mut old_e = entity("a.py", "STORAGE_KEY", Some("bh1"), Some("sh1"), "st1");
        old_e.kind = "constant".to_string();
        let mut new_e = entity("a.py", "STORAGE_PREFIX", Some("bh2"), Some("sh2"), "st2");
        new_e.kind = "function".to_string();
        let matches = match_entities(&[old_e], &[new_e]);
        // Different kinds — should NOT match as rename
        assert_eq!(matches.len(), 2); // 1 added + 1 removed
    }

    #[test]
    fn fuzzy_rename_requires_same_file() {
        let old = vec![entity("a.py", "THREAD_STORAGE_KEY", Some("bh1"), Some("sh1"), "st1")];
        let new = vec![entity("b.py", "THREAD_STORAGE_PREFIX", Some("bh2"), Some("sh2"), "st2")];
        let matches = match_entities(&old, &new);
        // Different files — should NOT match as fuzzy rename (but may match as Moved if name match)
        // Since names are different AND files are different → Added + Removed
        let added = matches.iter().filter(|m| m.match_kind == MatchKind::Added).count();
        let removed = matches.iter().filter(|m| m.match_kind == MatchKind::Removed).count();
        assert_eq!(added, 1);
        assert_eq!(removed, 1);
    }

    #[test]
    fn exact_match_distinguishes_by_parent() {
        // Two entities with the same name "text" but different parents "body" and "header".
        // Old has body.text and header.text; new has body.text (changed) and header.text (changed).
        // They should NOT cross-match: body.text(old) should match body.text(new), etc.
        let mut old_body = entity("a.json", "text", Some("bh1"), Some("sh1"), "st1");
        old_body.parent = Some("body".to_string());
        let mut old_header = entity("a.json", "text", Some("bh2"), Some("sh2"), "st2");
        old_header.parent = Some("header".to_string());

        let mut new_body = entity("a.json", "text", Some("bh1_changed"), Some("sh1"), "st1_changed");
        new_body.parent = Some("body".to_string());
        let mut new_header = entity("a.json", "text", Some("bh2_changed"), Some("sh2"), "st2_changed");
        new_header.parent = Some("header".to_string());

        let matches = match_entities(&[old_body, old_header], &[new_body, new_header]);
        // Both should be ExactMatch (same file + same parent + same name)
        let exact = matches.iter().filter(|m| m.match_kind == MatchKind::ExactMatch).count();
        assert_eq!(exact, 2, "both body.text and header.text should exact-match their counterparts");
        // Verify correct pairing: body matches body, header matches header
        for m in &matches {
            let old_parent = m.old.as_ref().unwrap().parent.as_deref().unwrap();
            let new_parent = m.new.as_ref().unwrap().parent.as_deref().unwrap();
            assert_eq!(old_parent, new_parent, "parents should match: old={}, new={}", old_parent, new_parent);
        }
    }

    #[test]
    fn moved_entity_matches_by_parent_and_name() {
        // body.text in file a should match body.text in file b (as Moved), not header.text in file b.
        let mut old_body = entity("a.json", "text", Some("bh1"), Some("sh1"), "st1");
        old_body.parent = Some("body".to_string());

        let mut new_body = entity("b.json", "text", Some("bh1"), Some("sh1"), "st1_moved");
        new_body.parent = Some("body".to_string());
        let mut new_header = entity("b.json", "text", Some("bh2"), Some("sh2"), "st2");
        new_header.parent = Some("header".to_string());

        let matches = match_entities(&[old_body], &[new_body, new_header]);
        // old body.text should match new body.text as Moved
        let moved = matches.iter().filter(|m| m.match_kind == MatchKind::Moved).collect::<Vec<_>>();
        assert_eq!(moved.len(), 1, "should have exactly 1 Moved match");
        assert_eq!(moved[0].new.as_ref().unwrap().parent.as_deref(), Some("body"),
            "moved match should pair with body parent, not header");
        // new header.text should be Added
        let added = matches.iter().filter(|m| m.match_kind == MatchKind::Added).count();
        assert_eq!(added, 1);
    }

    #[test]
    fn fuzzy_rename_requires_same_parent() {
        // Entities with different parents should NOT match as fuzzy renames.
        let mut old_e = entity("a.json", "STORAGE_KEY", Some("bh1"), Some("sh1"), "st1");
        old_e.parent = Some("body".to_string());
        let mut new_e = entity("a.json", "STORAGE_PREFIX", Some("bh2"), Some("sh2"), "st2");
        new_e.parent = Some("header".to_string());

        let matches = match_entities(&[old_e], &[new_e]);
        // Different parents — should NOT match as fuzzy rename
        let added = matches.iter().filter(|m| m.match_kind == MatchKind::Added).count();
        let removed = matches.iter().filter(|m| m.match_kind == MatchKind::Removed).count();
        assert_eq!(added, 1);
        assert_eq!(removed, 1);
    }
}
