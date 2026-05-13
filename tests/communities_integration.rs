//! Integration tests for `sigil::communities` (Leiden modularity clustering).
//!
//! Built around a synthetic 8-file graph with two obvious clusters — four
//! files imp-cycling each other in cluster X, four files doing the same in
//! cluster Y, no cross-cluster references. Leiden must find them.
//!
//! These tests intentionally go through the library API rather than the
//! CLI fixture path because the algorithm is the focus — the CLI handler
//! is a thin shim over `communities::detect_leiden`.

use std::collections::HashMap;

use sigil::communities::{detect_leiden, LeidenConfig};
use sigil::entity::{Entity, Reference};

fn ent(file: &str, name: &str) -> Entity {
    Entity {
        file: file.to_string(),
        name: name.to_string(),
        kind: "function".to_string(),
        line_start: 1,
        line_end: 2,
        parent: None,
        qualified_name: None,
        sig: None,
        meta: None,
        body_hash: None,
        sig_hash: None,
        struct_hash: "deadbeefcafef00d".to_string(),
        visibility: None,
        rank: None,
        blast_radius: None,
        doc: None,
        heritage: Vec::new(),
        alias: None,    }
}

fn refr(file: &str, caller: &str, target: &str) -> Reference {
    Reference {
        file: file.to_string(),
        caller: Some(caller.to_string()),
        name: target.to_string(),
        ref_kind: "call".to_string(),
        line: 1,
        confidence: None,
        callee_id: None,
    }
}

/// Build the 8-file two-cluster fixture used by most tests here.
fn two_cluster_fixture() -> (Vec<Entity>, Vec<Reference>) {
    // Cluster X: x1..x4 each define fx1..fx4 and reference each other.
    // Cluster Y: y1..y4 likewise. No edges between clusters.
    let xs = ["x1.rs", "x2.rs", "x3.rs", "x4.rs"];
    let x_syms = ["fx1", "fx2", "fx3", "fx4"];
    let ys = ["y1.rs", "y2.rs", "y3.rs", "y4.rs"];
    let y_syms = ["fy1", "fy2", "fy3", "fy4"];

    let mut entities = Vec::new();
    for (file, sym) in xs.iter().zip(x_syms.iter()) {
        entities.push(ent(file, sym));
    }
    for (file, sym) in ys.iter().zip(y_syms.iter()) {
        entities.push(ent(file, sym));
    }

    let mut refs = Vec::new();
    // Dense intra-cluster edges: every file refs every other file's symbol.
    for (i, src) in xs.iter().enumerate() {
        for (j, target_sym) in x_syms.iter().enumerate() {
            if i != j {
                refs.push(refr(src, "main", target_sym));
            }
        }
    }
    for (i, src) in ys.iter().enumerate() {
        for (j, target_sym) in y_syms.iter().enumerate() {
            if i != j {
                refs.push(refr(src, "main", target_sym));
            }
        }
    }
    (entities, refs)
}

#[test]
fn finds_two_clusters_on_8_file_synthetic_graph() {
    let (entities, refs) = two_cluster_fixture();
    let clusters = detect_leiden(&entities, &refs, &HashMap::new(), &LeidenConfig::default());

    assert_eq!(
        clusters.len(),
        2,
        "expected exactly two clusters on the bipartite fixture, got {:?}",
        clusters.iter().map(|c| (c.cluster_id, &c.members)).collect::<Vec<_>>()
    );
    let sizes: Vec<usize> = clusters.iter().map(|c| c.size).collect();
    assert_eq!(sizes, vec![4, 4], "each cluster should hold exactly four files");

    // Verify x-files and y-files end up grouped — Leiden shouldn't mix them.
    let x_cluster = clusters
        .iter()
        .find(|c| c.members.iter().any(|m| m == "x1.rs"))
        .expect("x1.rs should land in some cluster");
    for x in ["x1.rs", "x2.rs", "x3.rs", "x4.rs"] {
        assert!(
            x_cluster.members.iter().any(|m| m == x),
            "{} should be in the x-cluster, got {:?}",
            x,
            x_cluster.members
        );
    }
    let y_cluster = clusters
        .iter()
        .find(|c| c.members.iter().any(|m| m == "y1.rs"))
        .expect("y1.rs should land in some cluster");
    for y in ["y1.rs", "y2.rs", "y3.rs", "y4.rs"] {
        assert!(
            y_cluster.members.iter().any(|m| m == y),
            "{} should be in the y-cluster, got {:?}",
            y,
            y_cluster.members
        );
    }
    assert_ne!(
        x_cluster.cluster_id, y_cluster.cluster_id,
        "x and y clusters must have distinct ids"
    );
}

#[test]
fn cluster_ids_are_compact_and_zero_indexed() {
    let (entities, refs) = two_cluster_fixture();
    let clusters = detect_leiden(&entities, &refs, &HashMap::new(), &LeidenConfig::default());
    let ids: Vec<u32> = clusters.iter().map(|c| c.cluster_id).collect();
    assert_eq!(ids, vec![0, 1], "expected {{0,1}}, got {:?}", ids);
}

#[test]
fn deterministic_across_runs() {
    let (entities, refs) = two_cluster_fixture();
    let first = detect_leiden(&entities, &refs, &HashMap::new(), &LeidenConfig::default());
    let second = detect_leiden(&entities, &refs, &HashMap::new(), &LeidenConfig::default());
    let third = detect_leiden(&entities, &refs, &HashMap::new(), &LeidenConfig::default());
    assert_eq!(first, second, "two consecutive runs must match");
    assert_eq!(second, third, "three consecutive runs must match");
}

#[test]
fn representative_prefers_highest_pagerank_in_cluster() {
    let (entities, refs) = two_cluster_fixture();
    let mut rank = HashMap::new();
    // Pin x2.rs as the x-cluster's top-ranked file and y4.rs as the
    // y-cluster's. Even-weights everywhere else.
    rank.insert("x1.rs".to_string(), 0.05);
    rank.insert("x2.rs".to_string(), 0.95);
    rank.insert("x3.rs".to_string(), 0.05);
    rank.insert("x4.rs".to_string(), 0.05);
    rank.insert("y1.rs".to_string(), 0.05);
    rank.insert("y2.rs".to_string(), 0.05);
    rank.insert("y3.rs".to_string(), 0.05);
    rank.insert("y4.rs".to_string(), 0.95);

    let clusters = detect_leiden(&entities, &refs, &rank, &LeidenConfig::default());
    let x_cluster = clusters
        .iter()
        .find(|c| c.members.iter().any(|m| m == "x1.rs"))
        .unwrap();
    let y_cluster = clusters
        .iter()
        .find(|c| c.members.iter().any(|m| m == "y1.rs"))
        .unwrap();
    assert_eq!(
        x_cluster.representative, "x2.rs",
        "x-cluster representative should be the highest-ranked member"
    );
    assert_eq!(
        y_cluster.representative, "y4.rs",
        "y-cluster representative should be the highest-ranked member"
    );
}

#[test]
fn labels_capture_common_path_prefix() {
    // Re-layer files under shared directories so the labels are non-trivial.
    let xs = [
        "src/parser/x1.rs",
        "src/parser/x2.rs",
        "src/parser/x3.rs",
        "src/parser/x4.rs",
    ];
    let ys = [
        "src/install/y1.rs",
        "src/install/y2.rs",
        "src/install/y3.rs",
        "src/install/y4.rs",
    ];
    let x_syms = ["fx1", "fx2", "fx3", "fx4"];
    let y_syms = ["fy1", "fy2", "fy3", "fy4"];

    let mut entities = Vec::new();
    for (file, sym) in xs.iter().zip(x_syms.iter()) {
        entities.push(ent(file, sym));
    }
    for (file, sym) in ys.iter().zip(y_syms.iter()) {
        entities.push(ent(file, sym));
    }
    let mut refs = Vec::new();
    for (i, src) in xs.iter().enumerate() {
        for (j, target_sym) in x_syms.iter().enumerate() {
            if i != j {
                refs.push(refr(src, "main", target_sym));
            }
        }
    }
    for (i, src) in ys.iter().enumerate() {
        for (j, target_sym) in y_syms.iter().enumerate() {
            if i != j {
                refs.push(refr(src, "main", target_sym));
            }
        }
    }

    let clusters = detect_leiden(&entities, &refs, &HashMap::new(), &LeidenConfig::default());
    assert_eq!(clusters.len(), 2);
    let labels: Vec<String> = clusters
        .iter()
        .map(|c| c.label.clone().unwrap_or_default())
        .collect();
    assert!(
        labels.iter().any(|l| l == "src/parser"),
        "expected `src/parser` label, got {:?}",
        labels
    );
    assert!(
        labels.iter().any(|l| l == "src/install"),
        "expected `src/install` label, got {:?}",
        labels
    );
}

#[test]
fn ndjson_serialization_round_trips() {
    let (entities, refs) = two_cluster_fixture();
    let clusters = detect_leiden(&entities, &refs, &HashMap::new(), &LeidenConfig::default());
    // Serialize the CLI's NDJSON shape and parse it back — guards the
    // wire format from drift if the Cluster struct grows fields later.
    for c in &clusters {
        let line = serde_json::to_string(c).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&line).expect("parse");
        assert!(parsed.get("cluster_id").is_some(), "missing cluster_id field");
        assert!(parsed.get("size").is_some(), "missing size field");
        assert!(parsed.get("members").is_some(), "missing members field");
        assert!(parsed.get("representative").is_some(), "missing representative");
        // `label` is optional via skip_serializing_if when None — in this
        // fixture every cluster has members under a common root ".", so we
        // don't pin its presence here (covered explicitly by other tests).
    }
}

#[test]
fn isolated_files_produce_singleton_clusters() {
    // Five files, no references. Each should land in its own cluster.
    let entities: Vec<Entity> = (0..5).map(|i| ent(&format!("f{}.rs", i), "x")).collect();
    let clusters = detect_leiden(&entities, &[], &HashMap::new(), &LeidenConfig::default());
    assert_eq!(clusters.len(), 5);
    for c in &clusters {
        assert_eq!(c.size, 1);
    }
}
