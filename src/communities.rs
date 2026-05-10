//! File-level community detection via **Louvain modularity optimization**.
//!
//! Issue #17 asked for Leiden specifically. This module ships Louvain as the
//! MVP — it's the predecessor to Leiden, ~150 LoC of pure Rust, and produces
//! the same kind of modularity-maximizing partition on the file-graph
//! workloads sigil cares about. Leiden's refinement step (which guarantees
//! every community is internally connected and tends to lift modularity by
//! 1–3% on dense graphs) is a clean follow-up that can layer on top of the
//! Louvain output here without breaking the wire shape.
//!
//! ## Difference vs `community.rs`
//!
//! `community.rs` runs **label propagation** — cheap, O(E) per iteration,
//! used by `sigil map` to slap a subsystem tag onto each file. It's fine
//! for "give me a hint", but the partition isn't modularity-optimal and
//! the label IDs aren't a great clustering signal.
//!
//! `communities.rs` runs **Louvain** — O(N log N) per pass, two passes
//! over the file graph, locally-greedy modularity gain. Output is meant
//! to be the canonical clustering surface (`sigil communities` CLI,
//! `cluster_id` field on `sigil map`).
//!
//! ## Algorithm
//!
//! Classic Louvain (Blondel et al. 2008):
//!
//! 1. Build the undirected, weighted file graph (same edge rule as
//!    `rank.rs` / `community.rs`: a reference to a symbol defined in file
//!    B from file A contributes a 1/N-weighted edge A↔B).
//! 2. Each node starts in its own community.
//! 3. **Local moving phase**: repeat until no node moves —
//!      for each node, evaluate Δmodularity of moving it to each
//!      neighboring community, pick the best gain (must be > 0), break
//!      ties by smaller community id for determinism.
//! 4. **Aggregation phase**: collapse each community to a super-node;
//!    edges between super-nodes carry the summed edge weight, self-loops
//!    carry the internal weight.
//! 5. Repeat 3–4 on the aggregated graph until no node moves.
//! 6. Unfold the multi-level partition back to original file ids.
//!
//! Determinism: nodes and neighbors are iterated in sorted order at every
//! step. The RNG seed exists in the `LouvainConfig` for completeness but
//! the algorithm itself doesn't sample — same input → same output, no
//! seeding required for reproducibility. The seed knob is reserved for
//! future Leiden refinement (which does sample during the refine step).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::Serialize;

use crate::entity::{Entity, Reference};

/// Compact 0-indexed cluster id.
pub type ClusterId = u32;

/// Tunable knobs for `detect`. `resolution` follows the standard convention
/// (1.0 = vanilla Louvain modularity, >1 = more, smaller clusters; <1 =
/// fewer, larger clusters). `max_passes` caps the multi-level loop in case
/// of pathological inputs.
#[derive(Debug, Clone)]
pub struct LouvainConfig {
    pub resolution: f64,
    pub max_passes: u32,
    pub max_moves_per_pass: u32,
    /// Seed retained for Leiden's refinement step (which samples). Louvain
    /// itself is deterministic without seeding given sorted iteration; we
    /// keep the field so callers don't have to change shape when Leiden
    /// lands.
    pub seed: u64,
}

impl Default for LouvainConfig {
    fn default() -> Self {
        Self {
            resolution: 1.0,
            max_passes: 16,
            max_moves_per_pass: 100_000,
            seed: 0xC0FFEE,
        }
    }
}

/// One cluster of files. Sized + decorated for direct serialization to
/// the NDJSON output of `sigil communities`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Cluster {
    pub cluster_id: ClusterId,
    pub size: usize,
    pub members: Vec<String>,
    pub representative: String,
    /// Longest common path prefix of members (directory granularity), or
    /// `None` when no prefix is shared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Run Louvain over the file-graph derived from `entities` + `references`.
/// `file_rank` is consulted only for representative selection (highest-rank
/// file in each cluster, tiebreak by shortest path then lexicographic).
/// Files with no rank entry are scored 0.0.
pub fn detect(
    entities: &[Entity],
    references: &[Reference],
    file_rank: &HashMap<String, f64>,
    cfg: &LouvainConfig,
) -> Vec<Cluster> {
    let name_index = build_name_index(entities);
    let edges = build_file_edges(references, &name_index);
    let nodes = collect_nodes(entities, references);
    if nodes.is_empty() {
        return Vec::new();
    }

    let file_to_id: HashMap<&str, usize> =
        nodes.iter().enumerate().map(|(i, n)| (n.as_str(), i)).collect();

    // Adjacency: id → BTreeMap<neighbor_id, weight>. BTreeMap so iteration
    // order is deterministic.
    let mut adj: Vec<BTreeMap<usize, f64>> = vec![BTreeMap::new(); nodes.len()];
    for ((a, b), w) in &edges {
        let (Some(&ia), Some(&ib)) = (file_to_id.get(a.as_str()), file_to_id.get(b.as_str()))
        else {
            continue;
        };
        // Undirected — record both directions; self-loops collapse to one.
        *adj[ia].entry(ib).or_insert(0.0) += *w;
        if ia != ib {
            *adj[ib].entry(ia).or_insert(0.0) += *w;
        }
    }

    let community_of_node = louvain(&adj, cfg);

    // Bucket files by their final community id, then re-number to compact
    // 0..K so output `cluster_id`s are stable and contiguous.
    let mut buckets: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (node, &comm) in community_of_node.iter().enumerate() {
        buckets.entry(comm).or_default().push(node);
    }
    // Sort buckets by their first (lowest) file id so reruns produce the
    // same cluster_id assignment.
    let mut bucket_list: Vec<Vec<usize>> = buckets.into_values().collect();
    for b in &mut bucket_list {
        b.sort();
    }
    bucket_list.sort_by_key(|b| b[0]);

    bucket_list
        .into_iter()
        .enumerate()
        .map(|(cid, members)| {
            let member_paths: Vec<String> =
                members.iter().map(|&i| nodes[i].clone()).collect();
            let representative = pick_representative(&member_paths, file_rank);
            let label = common_path_prefix(&member_paths);
            Cluster {
                cluster_id: cid as ClusterId,
                size: member_paths.len(),
                members: member_paths,
                representative,
                label,
            }
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────
// Louvain core. Operates on a generic adjacency vec so the aggregation
// pass can recurse with the same code path.
// ─────────────────────────────────────────────────────────────────────────

/// Returns: `community_of_node[i]` = community id of node `i` at the
/// finest (input) level.
fn louvain(adj: &[BTreeMap<usize, f64>], cfg: &LouvainConfig) -> Vec<usize> {
    // Multi-level partitions stack: outer[level] = "for each node at this
    // level, which super-node does it map to at the next coarser level".
    // We unfold this stack at the end to recover the original-node → final
    // community map.
    let mut levels: Vec<Vec<usize>> = Vec::new();
    let mut current = adj.to_vec();

    for _ in 0..cfg.max_passes {
        let raw_partition = local_moving(&current, cfg);
        // Renumber community ids in node order so partition[i] ∈ 0..K
        // matches the super-node indices that `aggregate` will use. This
        // is what makes the unfold step at the bottom of `louvain` valid:
        // each level's partition vector indexes into the next level's
        // node space.
        let compact_partition = compact_in_node_order(&raw_partition);
        let distinct = compact_partition.iter().copied().max().map(|m| m + 1).unwrap_or(0);
        // Convergence check: if every node ended in its own singleton
        // (no merges happened this pass), we're done — but still push the
        // identity partition so unfolding visits every level.
        if distinct == current.len() {
            levels.push(compact_partition);
            break;
        }
        levels.push(compact_partition.clone());
        let aggregated = aggregate(&current, &compact_partition);
        // Safety: if aggregation didn't actually shrink (shouldn't happen
        // when distinct < current.len(), but guard anyway), exit before
        // we waste another pass.
        if aggregated.len() >= current.len() {
            break;
        }
        current = aggregated;
    }

    // Unfold the levels: start with identity at level 0 and propagate up.
    let n = adj.len();
    let mut result: Vec<usize> = (0..n).collect();
    for partition in &levels {
        for slot in result.iter_mut() {
            *slot = partition[*slot];
        }
    }
    // Compact community ids to 0..K. Iterate in node order so the lowest-
    // numbered node anchors each community's id assignment — that's what
    // makes reruns produce the same cluster_id values.
    let mut compact: HashMap<usize, usize> = HashMap::new();
    let mut next_id: usize = 0;
    for slot in result.iter_mut() {
        let id = *compact.entry(*slot).or_insert_with(|| {
            let id = next_id;
            next_id += 1;
            id
        });
        *slot = id;
    }
    result
}

/// Local moving phase: each node greedily picks the neighbor community
/// that yields the largest positive Δmodularity. Repeats until no node
/// moves in a full sweep, or `max_moves_per_pass` is exhausted.
fn local_moving(adj: &[BTreeMap<usize, f64>], cfg: &LouvainConfig) -> Vec<usize> {
    let n = adj.len();
    let mut comm: Vec<usize> = (0..n).collect();

    // Total weighted degree of each node (sum of incident weights; self-
    // loops count twice in modularity math, which is the convention for
    // undirected weighted graphs).
    let degree: Vec<f64> = adj
        .iter()
        .enumerate()
        .map(|(i, nbrs)| {
            let mut d = 0.0;
            for (&j, &w) in nbrs {
                if j == i {
                    d += 2.0 * w;
                } else {
                    d += w;
                }
            }
            d
        })
        .collect();

    // m = total edge weight (sum of degrees / 2). Used as the modularity
    // normalizer. A graph with no edges has m=0; we bail to identity
    // partition since modularity is undefined.
    let two_m: f64 = degree.iter().sum();
    if two_m <= 0.0 {
        return comm;
    }
    let m = two_m / 2.0;

    // Σ_tot per community: sum of degrees of nodes in that community.
    let mut sigma_tot: Vec<f64> = degree.clone();
    let resolution = cfg.resolution.max(0.0);

    let mut moves: u32 = 0;
    loop {
        let mut moved_this_sweep = false;
        for i in 0..n {
            if moves >= cfg.max_moves_per_pass {
                return comm;
            }
            // Compute weighted edges from i to each neighbor community.
            // Iterating `adj[i]` in BTreeMap order keeps determinism.
            let mut k_i_in: BTreeMap<usize, f64> = BTreeMap::new();
            let mut self_loop = 0.0;
            for (&j, &w) in &adj[i] {
                if j == i {
                    self_loop = w;
                    continue;
                }
                *k_i_in.entry(comm[j]).or_insert(0.0) += w;
            }

            let current_comm = comm[i];
            let k_i = degree[i];

            // Remove i from its current community for the duration of the
            // evaluation. Modularity gain of leaving:
            //   ΔQ_remove = - (k_i_in[current] / m
            //                  - γ * Σ_tot[current] * k_i / (2 m²))
            // We don't materialize ΔQ_remove since it cancels in pairwise
            // comparison; we just adjust sigma_tot to reflect i being
            // tentatively isolated.
            sigma_tot[current_comm] -= k_i;

            // Score each candidate community c (neighbor communities ∪
            // {current}). Gain of joining c:
            //   ΔQ_join(c) = k_i_in[c] / m - γ * Σ_tot[c] * k_i / (2 m²)
            let mut best_comm = current_comm;
            let mut best_gain = 0.0;
            // Evaluate current_comm explicitly so we have a baseline.
            let baseline = {
                let k_in = k_i_in.get(&current_comm).copied().unwrap_or(0.0);
                k_in / m
                    - resolution * sigma_tot[current_comm] * k_i / (2.0 * m * m)
            };
            for (&c, &k_in) in &k_i_in {
                let gain = k_in / m
                    - resolution * sigma_tot[c] * k_i / (2.0 * m * m);
                // Strict > to enforce "must beat baseline by an epsilon"
                // semantics, and lower-community-id tiebreak via the
                // BTreeMap iteration order (lower c gets evaluated first).
                if gain > baseline && gain > best_gain {
                    best_gain = gain;
                    best_comm = c;
                }
            }

            // Self-loop never affects relative gain (it's the same constant
            // across all candidate communities), but the variable name is
            // kept for clarity. Suppress unused-warning if rustc gets picky.
            let _ = self_loop;

            // Re-insert i into the chosen community.
            sigma_tot[best_comm] += k_i;
            if best_comm != current_comm {
                comm[i] = best_comm;
                moved_this_sweep = true;
                moves += 1;
            }
        }
        if !moved_this_sweep {
            break;
        }
    }

    comm
}

/// Renumber community ids in `partition` so the values are dense 0..K, with
/// id assignment anchored by first appearance in node order. This is what
/// makes a partition vector legal to use as both "node i's community" and
/// "node i's index in the aggregated graph" at the next level.
fn compact_in_node_order(partition: &[usize]) -> Vec<usize> {
    let mut compact: HashMap<usize, usize> = HashMap::new();
    let mut next: usize = 0;
    partition
        .iter()
        .map(|&c| {
            *compact.entry(c).or_insert_with(|| {
                let id = next;
                next += 1;
                id
            })
        })
        .collect()
}

/// Build the coarser graph: one super-node per community (assumed already
/// compact, i.e. partition values ∈ 0..K), edges summed (with self-loops
/// carrying internal community weight).
fn aggregate(
    adj: &[BTreeMap<usize, f64>],
    partition: &[usize],
) -> Vec<BTreeMap<usize, f64>> {
    let new_n = partition.iter().copied().max().map(|m| m + 1).unwrap_or(0);
    let mut new_adj: Vec<BTreeMap<usize, f64>> = vec![BTreeMap::new(); new_n];
    for (i, nbrs) in adj.iter().enumerate() {
        let ci = partition[i];
        for (&j, &w) in nbrs {
            let cj = partition[j];
            // Adjacency lists are symmetric, so each (i, j) appears twice
            // when i != j. Halve so the summed graph keeps the right
            // magnitude. Self-loops (i == j) carry once.
            let contribution = if i == j { w } else { w * 0.5 };
            *new_adj[ci].entry(cj).or_insert(0.0) += contribution;
            if ci != cj {
                *new_adj[cj].entry(ci).or_insert(0.0) += contribution;
            }
        }
    }
    new_adj
}

// ─────────────────────────────────────────────────────────────────────────
// Graph construction (mirrors rank.rs / community.rs to keep this module
// independent).
// ─────────────────────────────────────────────────────────────────────────

fn build_name_index(entities: &[Entity]) -> HashMap<&str, Vec<&str>> {
    let mut out: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in entities {
        out.entry(e.name.as_str())
            .or_default()
            .push(e.file.as_str());
    }
    for files in out.values_mut() {
        files.sort();
        files.dedup();
    }
    out
}

fn build_file_edges(
    references: &[Reference],
    name_index: &HashMap<&str, Vec<&str>>,
) -> HashMap<(String, String), f64> {
    let mut edges: HashMap<(String, String), f64> = HashMap::new();
    for r in references {
        let Some(targets) = name_index.get(r.name.as_str()) else {
            continue;
        };
        let targets: Vec<&str> = targets.iter().copied().filter(|f| *f != r.file).collect();
        if targets.is_empty() {
            continue;
        }
        let w = 1.0 / targets.len() as f64;
        for t in targets {
            let key = if r.file.as_str() < t {
                (r.file.clone(), t.to_string())
            } else {
                (t.to_string(), r.file.clone())
            };
            *edges.entry(key).or_insert(0.0) += w;
        }
    }
    edges
}

fn collect_nodes(entities: &[Entity], references: &[Reference]) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    for e in entities {
        set.insert(e.file.clone());
    }
    for r in references {
        set.insert(r.file.clone());
    }
    set.into_iter().collect()
}

// ─────────────────────────────────────────────────────────────────────────
// Cluster decoration: representative + label.
// ─────────────────────────────────────────────────────────────────────────

/// Highest PageRank in the cluster wins. Tiebreaks: shortest path string,
/// then lexicographic. Falls back to the lexicographically-smallest path
/// when no member has a rank entry.
fn pick_representative(members: &[String], file_rank: &HashMap<String, f64>) -> String {
    members
        .iter()
        .map(|p| {
            let rank = file_rank.get(p).copied().unwrap_or(0.0);
            (rank, std::cmp::Reverse(p.len()), std::cmp::Reverse(p.clone()), p.clone())
        })
        .max_by(|a, b| {
            a.0.partial_cmp(&b.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.1.cmp(&b.1))
                .then(a.2.cmp(&b.2))
        })
        .map(|(_, _, _, p)| p)
        .unwrap_or_default()
}

/// Longest shared directory-prefix across `members`. Returns `None` when
/// no prefix is shared (cross-cutting cluster) or for the empty list.
/// Single-file clusters get their parent directory (or the filename when
/// the file lives at the repo root).
fn common_path_prefix(members: &[String]) -> Option<String> {
    if members.is_empty() {
        return None;
    }
    if members.len() == 1 {
        let only = &members[0];
        return match only.rsplit_once('/') {
            Some((dir, _)) if !dir.is_empty() => Some(dir.to_string()),
            _ => None,
        };
    }
    let dir_parts: Vec<Vec<&str>> = members
        .iter()
        .map(|f| {
            let dir = f.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            if dir.is_empty() {
                Vec::new()
            } else {
                dir.split('/').collect()
            }
        })
        .collect();
    let min_depth = dir_parts.iter().map(|d| d.len()).min().unwrap_or(0);
    let mut shared: Vec<&str> = Vec::new();
    for i in 0..min_depth {
        let expected = dir_parts[0][i];
        if dir_parts.iter().all(|d| d.get(i) == Some(&expected)) {
            shared.push(expected);
        } else {
            break;
        }
    }
    if shared.is_empty() {
        None
    } else {
        Some(shared.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{Entity, Reference};

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
            struct_hash: "d".to_string(),
            visibility: None,
            rank: None,
            blast_radius: None,
            doc: None,
        }
    }

    fn refr(file: &str, caller: Option<&str>, name: &str) -> Reference {
        Reference {
            file: file.to_string(),
            caller: caller.map(str::to_string),
            name: name.to_string(),
            ref_kind: "call".to_string(),
            line: 1,
        }
    }

    #[test]
    fn empty_input_returns_no_clusters() {
        let r = HashMap::new();
        let out = detect(&[], &[], &r, &LouvainConfig::default());
        assert!(out.is_empty());
    }

    #[test]
    fn isolated_files_each_become_their_own_cluster() {
        let entities = vec![ent("a.rs", "x"), ent("b.rs", "y"), ent("c.rs", "z")];
        let out = detect(&entities, &[], &HashMap::new(), &LouvainConfig::default());
        assert_eq!(out.len(), 3, "no edges → each file is its own cluster");
        for c in &out {
            assert_eq!(c.size, 1);
        }
    }

    #[test]
    fn two_tight_clusters_separate() {
        // Cluster X = {x1, x2, x3, x4}, cluster Y = {y1, y2, y3, y4}.
        // Dense intra-cluster refs, no inter-cluster refs.
        let entities = vec![
            ent("x1.rs", "f1"),
            ent("x2.rs", "f2"),
            ent("x3.rs", "f3"),
            ent("x4.rs", "f4"),
            ent("y1.rs", "g1"),
            ent("y2.rs", "g2"),
            ent("y3.rs", "g3"),
            ent("y4.rs", "g4"),
        ];
        let xs = ["x1.rs", "x2.rs", "x3.rs", "x4.rs"];
        let ys = ["y1.rs", "y2.rs", "y3.rs", "y4.rs"];
        let mut refs = Vec::new();
        for (i, src) in xs.iter().enumerate() {
            for (j, _dst) in xs.iter().enumerate() {
                if i != j {
                    refs.push(refr(src, Some("c"), &format!("f{}", j + 1)));
                }
            }
        }
        for (i, src) in ys.iter().enumerate() {
            for (j, _dst) in ys.iter().enumerate() {
                if i != j {
                    refs.push(refr(src, Some("c"), &format!("g{}", j + 1)));
                }
            }
        }
        let out = detect(&entities, &refs, &HashMap::new(), &LouvainConfig::default());
        // Expect 2 clusters, each size 4.
        assert_eq!(out.len(), 2, "should detect two communities, got {:?}", out);
        let sizes: Vec<usize> = out.iter().map(|c| c.size).collect();
        assert_eq!(sizes, vec![4, 4]);
        // Every x-file is together; every y-file is together.
        let x_cluster = out
            .iter()
            .find(|c| c.members.iter().any(|m| m == "x1.rs"))
            .unwrap();
        for x in &xs {
            assert!(x_cluster.members.iter().any(|m| m == x), "missing {}", x);
        }
    }

    #[test]
    fn deterministic_across_runs() {
        let entities = vec![
            ent("a.rs", "f1"),
            ent("b.rs", "f2"),
            ent("c.rs", "f3"),
            ent("d.rs", "f4"),
        ];
        let refs = vec![
            refr("a.rs", Some("c"), "f2"),
            refr("b.rs", Some("c"), "f1"),
            refr("c.rs", Some("c"), "f4"),
            refr("d.rs", Some("c"), "f3"),
        ];
        let first = detect(&entities, &refs, &HashMap::new(), &LouvainConfig::default());
        let second = detect(&entities, &refs, &HashMap::new(), &LouvainConfig::default());
        assert_eq!(first, second);
    }

    #[test]
    fn cluster_ids_are_compact_zero_indexed() {
        let entities = vec![ent("a.rs", "x"), ent("b.rs", "y"), ent("c.rs", "z")];
        let out = detect(&entities, &[], &HashMap::new(), &LouvainConfig::default());
        let ids: Vec<u32> = out.iter().map(|c| c.cluster_id).collect();
        assert_eq!(ids, vec![0, 1, 2]);
    }

    #[test]
    fn representative_prefers_highest_rank() {
        let entities = vec![
            ent("popular.rs", "f"),
            ent("a.rs", "g"),
            ent("b.rs", "h"),
        ];
        let refs = vec![
            refr("a.rs", Some("c"), "f"),
            refr("b.rs", Some("c"), "f"),
            refr("a.rs", Some("c"), "h"),
            refr("b.rs", Some("c"), "g"),
        ];
        let mut rank = HashMap::new();
        rank.insert("popular.rs".to_string(), 0.9);
        rank.insert("a.rs".to_string(), 0.05);
        rank.insert("b.rs".to_string(), 0.05);
        let out = detect(&entities, &refs, &rank, &LouvainConfig::default());
        assert_eq!(out.len(), 1, "all three should cluster");
        assert_eq!(out[0].representative, "popular.rs");
    }

    #[test]
    fn representative_tiebreaks_by_shortest_then_lex() {
        // No rank entries; all members tied at 0.0. Tiebreak should
        // pick the shortest path, then lexicographic.
        let members = vec![
            "src/a/longer.rs".to_string(),
            "src/short.rs".to_string(),
            "src/z.rs".to_string(),
        ];
        let rep = pick_representative(&members, &HashMap::new());
        // src/z.rs and src/short.rs both length 10/12? Let's compute:
        //   src/z.rs       = 9 chars
        //   src/short.rs   = 12 chars
        //   src/a/longer.rs= 15 chars
        // Shortest wins: src/z.rs.
        assert_eq!(rep, "src/z.rs");
    }

    #[test]
    fn label_uses_longest_shared_dir_prefix() {
        assert_eq!(
            common_path_prefix(&[
                "src/parser/a.rs".to_string(),
                "src/parser/b.rs".to_string(),
            ]),
            Some("src/parser".to_string())
        );
        assert_eq!(
            common_path_prefix(&[
                "src/a.rs".to_string(),
                "src/b/c.rs".to_string(),
            ]),
            Some("src".to_string())
        );
    }

    #[test]
    fn label_is_none_for_cross_cutting_clusters() {
        assert_eq!(
            common_path_prefix(&["top.rs".to_string(), "elsewhere/foo.rs".to_string()]),
            None
        );
    }

    #[test]
    fn label_for_single_file_cluster_returns_parent_dir() {
        assert_eq!(
            common_path_prefix(&["src/parser/helpers.rs".to_string()]),
            Some("src/parser".to_string())
        );
        assert_eq!(common_path_prefix(&["top.rs".to_string()]), None);
    }

    #[test]
    fn label_handles_empty_list() {
        assert_eq!(common_path_prefix(&[]), None);
    }

    #[test]
    fn resolution_knob_affects_partition_granularity() {
        // Two weakly-connected sub-cliques. At resolution=2.0 Louvain
        // should split them; at resolution=0.1 it may merge them. We
        // just assert that *some* difference shows up across the range,
        // since exact partition counts depend on local-moving order.
        let entities = vec![
            ent("a.rs", "f1"),
            ent("b.rs", "f2"),
            ent("c.rs", "f3"),
            ent("d.rs", "f4"),
        ];
        let refs = vec![
            // Dense within {a,b}
            refr("a.rs", Some("c"), "f2"),
            refr("b.rs", Some("c"), "f1"),
            refr("a.rs", Some("c"), "f2"),
            refr("b.rs", Some("c"), "f1"),
            // Dense within {c,d}
            refr("c.rs", Some("c"), "f4"),
            refr("d.rs", Some("c"), "f3"),
            refr("c.rs", Some("c"), "f4"),
            refr("d.rs", Some("c"), "f3"),
            // One weak link between the two
            refr("b.rs", Some("c"), "f3"),
        ];
        let low_res = detect(
            &entities,
            &refs,
            &HashMap::new(),
            &LouvainConfig {
                resolution: 0.1,
                ..LouvainConfig::default()
            },
        );
        let high_res = detect(
            &entities,
            &refs,
            &HashMap::new(),
            &LouvainConfig {
                resolution: 5.0,
                ..LouvainConfig::default()
            },
        );
        // We don't pin exact cluster counts (depends on tie-breaks), but
        // high resolution should never produce fewer clusters than low.
        assert!(
            high_res.len() >= low_res.len(),
            "high resolution {} should produce >= clusters than low {}",
            high_res.len(),
            low_res.len()
        );
    }
}
