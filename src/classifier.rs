use crate::entity::Entity;
use crate::matcher::{EntityMatch, MatchKind};
use crate::diff_json::{ChangeKind, EntityDiff};

/// Classify a matched entity pair into a structured diff entry.
pub fn classify(m: &EntityMatch) -> EntityDiff {
    match (&m.old, &m.new, &m.match_kind) {
        (None, Some(ne), MatchKind::Added) => EntityDiff {
            change: ChangeKind::Added,
            name: ne.name.clone(),
            kind: ne.kind.clone(),
            file: ne.file.clone(),
            old_file: None, old_name: None,
            sig_changed: None, body_changed: None,
            breaking: false,
            breaking_reason: None,
            old: None, new: Some(ne.clone()),
            inline_diff: None,
            change_details: None,
        },
        (Some(oe), None, MatchKind::Removed) => {
            let brk = is_public(oe);
            EntityDiff {
                change: ChangeKind::Removed,
                name: oe.name.clone(),
                kind: oe.kind.clone(),
                file: oe.file.clone(),
                old_file: None, old_name: None,
                sig_changed: None, body_changed: None,
                breaking: brk,
                breaking_reason: if brk { Some("removed".into()) } else { None },
                old: Some(oe.clone()), new: None,
                inline_diff: None,
                change_details: None,
            }
        },
        (Some(oe), Some(ne), MatchKind::Moved) => {
            let sig_changed = oe.sig_hash != ne.sig_hash;
            let body_changed = oe.body_hash != ne.body_hash;
            let brk = is_public(oe) && sig_changed;
            EntityDiff {
                change: ChangeKind::Moved,
                name: ne.name.clone(),
                kind: ne.kind.clone(),
                file: ne.file.clone(),
                old_file: Some(oe.file.clone()),
                old_name: None,
                sig_changed: if sig_changed || body_changed { Some(sig_changed) } else { None },
                body_changed: if sig_changed || body_changed { Some(body_changed) } else { None },
                breaking: brk,
                breaking_reason: if brk { Some("moved".into()) } else { None },
                old: Some(oe.clone()), new: Some(ne.clone()),
                inline_diff: None,
                change_details: None,
            }
        },
        (Some(oe), Some(ne), MatchKind::Renamed) => {
            let brk = is_public(oe);
            EntityDiff {
                change: ChangeKind::Renamed,
                name: ne.name.clone(),
                kind: ne.kind.clone(),
                file: ne.file.clone(),
                old_file: if oe.file != ne.file { Some(oe.file.clone()) } else { None },
                old_name: Some(oe.name.clone()),
                sig_changed: None, body_changed: None,
                breaking: brk,
                breaking_reason: if brk { Some("renamed".into()) } else { None },
                old: Some(oe.clone()), new: Some(ne.clone()),
                inline_diff: None,
                change_details: None,
            }
        },
        (Some(oe), Some(ne), MatchKind::ExactMatch) => {
            let sig_changed = oe.sig_hash != ne.sig_hash;
            let body_changed = oe.body_hash != ne.body_hash;

            if !sig_changed && !body_changed {
                // struct_hash differs but sig and body don't → formatting only
                return EntityDiff {
                    change: ChangeKind::FormattingOnly,
                    name: ne.name.clone(),
                    kind: ne.kind.clone(),
                    file: ne.file.clone(),
                    old_file: None, old_name: None,
                    sig_changed: None, body_changed: None,
                    breaking: false,
                    breaking_reason: None,
                    old: Some(oe.clone()), new: Some(ne.clone()),
                    inline_diff: None,
                    change_details: None,
                };
            }

            let brk = is_public(oe) && sig_changed;
            EntityDiff {
                change: ChangeKind::Modified,
                name: ne.name.clone(),
                kind: ne.kind.clone(),
                file: ne.file.clone(),
                old_file: None, old_name: None,
                sig_changed: Some(sig_changed),
                body_changed: Some(body_changed),
                breaking: brk,
                breaking_reason: if brk { Some("sig_changed".into()) } else { None },
                old: Some(oe.clone()), new: Some(ne.clone()),
                inline_diff: None,
                change_details: None,
            }
        },
        _ => unreachable!("invalid match combination"),
    }
}

/// Heuristic: entity is "public" if it has no parent (top-level) and is not an import.
fn is_public(entity: &Entity) -> bool {
    entity.parent.is_none() && entity.kind != "import"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::Entity;
    use crate::matcher::{EntityMatch, MatchKind};
    use crate::diff_json::ChangeKind;

    fn entity(sig_hash: Option<&str>, body_hash: Option<&str>, struct_hash: &str) -> Entity {
        Entity {
            file: "a.py".into(), name: "foo".into(), kind: "function".into(),
            line_start: 1, line_end: 5, parent: None,
            qualified_name: None,
            sig: Some("def foo():".into()), meta: None,
            body_hash: body_hash.map(|s| s.into()),
            sig_hash: sig_hash.map(|s| s.into()),
            struct_hash: struct_hash.into(),
            visibility: None,
            rank: None,
            blast_radius: None,
            doc: None,
            heritage: Vec::new(),
        }
    }

    #[test]
    fn formatting_only() {
        let old = entity(Some("sh1"), Some("bh1"), "st_old");
        let new = entity(Some("sh1"), Some("bh1"), "st_new");
        let m = EntityMatch { old: Some(old), new: Some(new), match_kind: MatchKind::ExactMatch, confidence: 1.0 };
        let d = classify(&m);
        assert_eq!(d.change, ChangeKind::FormattingOnly);
        assert!(!d.breaking);
    }

    #[test]
    fn sig_changed_only() {
        let old = entity(Some("sh1"), Some("bh1"), "st_old");
        let new = entity(Some("sh2"), Some("bh1"), "st_new");
        let m = EntityMatch { old: Some(old), new: Some(new), match_kind: MatchKind::ExactMatch, confidence: 1.0 };
        let d = classify(&m);
        assert_eq!(d.change, ChangeKind::Modified);
        assert_eq!(d.sig_changed, Some(true));
        assert_eq!(d.body_changed, Some(false));
    }

    #[test]
    fn body_changed_only() {
        let old = entity(Some("sh1"), Some("bh1"), "st_old");
        let new = entity(Some("sh1"), Some("bh2"), "st_new");
        let m = EntityMatch { old: Some(old), new: Some(new), match_kind: MatchKind::ExactMatch, confidence: 1.0 };
        let d = classify(&m);
        assert_eq!(d.change, ChangeKind::Modified);
        assert_eq!(d.sig_changed, Some(false));
        assert_eq!(d.body_changed, Some(true));
        assert!(!d.breaking);
    }

    #[test]
    fn both_changed() {
        let old = entity(Some("sh1"), Some("bh1"), "st_old");
        let new = entity(Some("sh2"), Some("bh2"), "st_new");
        let m = EntityMatch { old: Some(old), new: Some(new), match_kind: MatchKind::ExactMatch, confidence: 1.0 };
        let d = classify(&m);
        assert_eq!(d.change, ChangeKind::Modified);
        assert_eq!(d.sig_changed, Some(true));
        assert_eq!(d.body_changed, Some(true));
    }

    #[test]
    fn breaking_when_public_sig_changes() {
        let old = entity(Some("sh1"), Some("bh1"), "st_old");
        let new = entity(Some("sh2"), Some("bh1"), "st_new");
        let m = EntityMatch { old: Some(old), new: Some(new), match_kind: MatchKind::ExactMatch, confidence: 1.0 };
        let d = classify(&m);
        assert!(d.breaking);
    }

    #[test]
    fn not_breaking_when_private_sig_changes() {
        let mut old = entity(Some("sh1"), Some("bh1"), "st_old");
        old.parent = Some("SomeClass".into());
        let mut new = entity(Some("sh2"), Some("bh1"), "st_new");
        new.parent = Some("SomeClass".into());
        let m = EntityMatch { old: Some(old), new: Some(new), match_kind: MatchKind::ExactMatch, confidence: 1.0 };
        let d = classify(&m);
        assert!(!d.breaking);
    }

    #[test]
    fn added_entity() {
        let new = entity(Some("sh1"), Some("bh1"), "st1");
        let m = EntityMatch { old: None, new: Some(new), match_kind: MatchKind::Added, confidence: 1.0 };
        let d = classify(&m);
        assert_eq!(d.change, ChangeKind::Added);
        assert!(!d.breaking);
    }

    #[test]
    fn removed_entity_is_breaking() {
        let old = entity(Some("sh1"), Some("bh1"), "st1");
        let m = EntityMatch { old: Some(old), new: None, match_kind: MatchKind::Removed, confidence: 1.0 };
        let d = classify(&m);
        assert_eq!(d.change, ChangeKind::Removed);
        assert!(d.breaking);
    }

    #[test]
    fn moved_entity() {
        let mut old = entity(Some("sh1"), Some("bh1"), "st1");
        old.file = "old.py".into();
        let new = entity(Some("sh1"), Some("bh1"), "st1");
        let m = EntityMatch { old: Some(old), new: Some(new), match_kind: MatchKind::Moved, confidence: 1.0 };
        let d = classify(&m);
        assert_eq!(d.change, ChangeKind::Moved);
        assert_eq!(d.old_file, Some("old.py".to_string()));
    }

    #[test]
    fn renamed_entity_is_breaking() {
        let old = entity(Some("sh1"), Some("bh1"), "st1");
        let new = entity(Some("sh2"), Some("bh1"), "st2");
        let m = EntityMatch { old: Some(old), new: Some(new), match_kind: MatchKind::Renamed, confidence: 1.0 };
        let d = classify(&m);
        assert_eq!(d.change, ChangeKind::Renamed);
        assert!(d.breaking);
    }
}
