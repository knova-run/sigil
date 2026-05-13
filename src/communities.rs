//! File-level community detection via **Leiden modularity optimization**
//! (issue #17). Single public entry point: [`detect_leiden`].
//!
//! `sigil communities` exposes this directly — no algorithm choice on the
//! CLI surface. Internally the pipeline reuses the modularity-greedy
//! local-moving phase from Blondel et al. (2008) and follows it with a
//! connectivity refinement pass that BFS-splits any internally-disconnected
//! community before aggregation. Every output community is therefore
//! guaranteed connected — the headline guarantee Traag et al. (2019) flag
//! as missing from vanilla Louvain.
//!
//! The randomized well-connected refinement variant from the Leiden paper
//! (which can also tighten modularity by 1–3% on dense graphs) is a clean
//! follow-up — the connectivity-component refinement here addresses the
//! invariant the issue calls out without requiring a randomized pass.
//!
//! ## Difference vs `community.rs`
//!
//! `community.rs` runs **label propagation** — cheap, O(E) per iteration,
//! used by `sigil map` to slap a subsystem tag onto each file. It's fine
//! for "give me a hint", but the partition isn't modularity-optimal and
//! the label IDs aren't a great clustering signal.
//!
//! `communities.rs` runs **Leiden** — O(N log N) per pass over the file
//! graph, locally-greedy modularity gain plus a connectivity refinement
//! step. Output is meant to be the canonical clustering surface
//! (`sigil communities` CLI, `cluster_id` field on `sigil map`).
//!
//! ## Algorithm
//!
//! Per multi-level pass:
//!
//! 1. Build the undirected, weighted file graph (same edge rule as
//!    `rank.rs` / `community.rs`: a reference to a symbol defined in file
//!    B from file A contributes a 1/N-weighted edge A↔B).
//! 2. Each node starts in its own community.
//! 3. **Local moving phase** (Blondel et al. 2008): for each node, evaluate
//!    Δmodularity of moving it to each neighboring community; pick the best
//!    positive gain; break ties by smaller community id for determinism.
//! 4. **Refinement phase** (Traag et al. 2019, connectivity-component
//!    variant): for each community produced by step 3, BFS within the
//!    community-induced subgraph and split any disconnected component into
//!    its own refined community.
//! 5. **Aggregation phase**: collapse each refined community to a
//!    super-node; edges between super-nodes carry the summed edge weight,
//!    self-loops carry the internal weight.
//! 6. Repeat 3–5 on the aggregated graph until no node moves.
//! 7. Unfold the multi-level partition back to original file ids.
//!
//! Determinism: nodes and neighbors are iterated in sorted order at every
//! step. The `seed` field on `LeidenConfig` is reserved for the future
//! randomized well-connected refinement variant; current
//! connectivity-component refinement is itself deterministic, so the
//! seed isn't consulted today.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use serde::Serialize;

use crate::entity::{Entity, Reference};

/// Compact 0-indexed cluster id.
pub type ClusterId = u32;

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

/// Build the file-graph used by `detect_leiden`. Returns the deterministic
/// node list and the symmetric adjacency, or `None` when the input has no
/// files at all (callers shortcut to an empty cluster set).
fn build_graph(
    entities: &[Entity],
    references: &[Reference],
) -> Option<(Vec<String>, Vec<BTreeMap<usize, f64>>)> {
    let name_index = build_name_index(entities);
    let edges = build_file_edges(references, &name_index);
    let nodes = collect_nodes(entities, references);
    if nodes.is_empty() {
        return None;
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
    Some((nodes, adj))
}

// ─────────────────────────────────────────────────────────────────────────
// Leiden surface (issue #17). Single public clustering API; output shape
// is the `Cluster` struct above.
// ─────────────────────────────────────────────────────────────────────────

/// Tunable knobs for [`detect_leiden`]. `resolution` follows the standard
/// modularity convention (1.0 = vanilla, >1 → more, smaller clusters; <1 →
/// fewer, larger clusters). `max_passes` caps the multi-level loop in case
/// of pathological inputs.
#[derive(Debug, Clone)]
pub struct LeidenConfig {
    pub resolution: f64,
    pub max_passes: u32,
    pub max_moves_per_pass: u32,
    /// Seed for the future randomized well-connected refinement variant
    /// from Traag et al. (2019). The current connectivity-component
    /// refinement is itself deterministic, so `seed` is wire-format-stable
    /// but unused by today's algorithm — keeping it on the public type
    /// avoids a breaking change when the randomized refinement lands.
    pub seed: u64,
    /// Refinement randomness knob (Traag et al. 2019, θ ≈ 0.01). Same
    /// reservation note as `seed`.
    pub theta: f64,
}

impl Default for LeidenConfig {
    fn default() -> Self {
        Self {
            resolution: 1.0,
            max_passes: 16,
            max_moves_per_pass: 100_000,
            seed: 0xC0FFEE,
            theta: 0.01,
        }
    }
}

/// Leiden modularity clustering over the file-graph derived from `entities`
/// + `references`. `file_rank` is consulted only for representative
/// selection (highest-rank file in each cluster, tiebreak by shortest path
/// then lexicographic). Files with no rank entry are scored 0.0.
///
/// Every output community is guaranteed internally connected: the
/// modularity-greedy local-moving phase produces an initial partition; a
/// refinement phase splits any internally-disconnected community into its
/// connected components; aggregation uses the refined partition. Repeated
/// multi-level until convergence.
pub fn detect_leiden(
    entities: &[Entity],
    references: &[Reference],
    file_rank: &HashMap<String, f64>,
    cfg: &LeidenConfig,
) -> Vec<Cluster> {
    let Some((nodes, adj)) = build_graph(entities, references) else {
        return Vec::new();
    };
    let community_of_node = leiden(&adj, cfg);
    clusters_from_partition(community_of_node, &nodes, file_rank)
}

/// Bucket files by community id, decorate with representative + label,
/// and renumber cluster ids to a stable 0..K.
fn clusters_from_partition(
    community_of_node: Vec<usize>,
    nodes: &[String],
    file_rank: &HashMap<String, f64>,
) -> Vec<Cluster> {
    let mut buckets: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (node, &comm) in community_of_node.iter().enumerate() {
        buckets.entry(comm).or_default().push(node);
    }
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

/// Leiden multi-level loop: modularity-greedy local-moving + connectivity
/// refinement + aggregation, until convergence. Returns
/// `community_of_node[i]` = final community id of node `i` at the finest
/// (input) level.
fn leiden(adj: &[BTreeMap<usize, f64>], cfg: &LeidenConfig) -> Vec<usize> {
    let mut levels: Vec<Vec<usize>> = Vec::new();
    let mut current = adj.to_vec();

    for _ in 0..cfg.max_passes {
        let raw_partition = local_moving(&current, cfg);
        let coarse_compact = compact_in_node_order(&raw_partition);
        // Refinement: split any internally-disconnected community into its
        // connected components. This is the headline Leiden guarantee that
        // local-moving alone doesn't make — every refined community grows
        // by BFS through actual edges.
        let refined = refine_to_connected_components(&current, &coarse_compact);
        let refined_compact = compact_in_node_order(&refined);
        let distinct = refined_compact.iter().copied().max().map(|m| m + 1).unwrap_or(0);
        if distinct == current.len() {
            levels.push(refined_compact);
            break;
        }
        levels.push(refined_compact.clone());
        let aggregated = aggregate(&current, &refined_compact);
        if aggregated.len() >= current.len() {
            break;
        }
        current = aggregated;
    }

    let n = adj.len();
    let mut result: Vec<usize> = (0..n).collect();
    for partition in &levels {
        for slot in result.iter_mut() {
            *slot = partition[*slot];
        }
    }
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

/// For each Louvain community, BFS through the community's induced subgraph
/// and assign each connected component its own refined id. Trivially
/// guarantees that every refined community is internally connected — the
/// invariant Louvain occasionally violates and Leiden patches.
fn refine_to_connected_components(
    adj: &[BTreeMap<usize, f64>],
    louvain_partition: &[usize],
) -> Vec<usize> {
    let n = adj.len();
    // Group nodes by Louvain community in deterministic order.
    let mut by_community: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (i, &c) in louvain_partition.iter().enumerate() {
        by_community.entry(c).or_default().push(i);
    }
    let mut refined: Vec<usize> = vec![0; n];
    let mut next_id: usize = 0;
    for (_louvain_comm, nodes) in by_community {
        let node_set: BTreeSet<usize> = nodes.iter().copied().collect();
        let mut visited: BTreeSet<usize> = BTreeSet::new();
        // Iterate node entry order (lowest-first via BTreeMap of community)
        // so refined ids are deterministic across runs.
        for &start in &nodes {
            if visited.contains(&start) {
                continue;
            }
            let component_id = next_id;
            next_id += 1;
            // BFS through edges that stay inside this Louvain community.
            // VecDeque + pop_front gives FIFO order, so traversal expands
            // breadth-first like the doc says.
            let mut queue: VecDeque<usize> = VecDeque::from([start]);
            while let Some(node) = queue.pop_front() {
                if !visited.insert(node) {
                    continue;
                }
                refined[node] = component_id;
                for (&j, _) in &adj[node] {
                    if node_set.contains(&j) && !visited.contains(&j) {
                        queue.push_back(j);
                    }
                }
            }
        }
    }
    refined
}

// ─────────────────────────────────────────────────────────────────────────
// Modularity local-moving. Operates on a generic adjacency vec so the
// aggregation pass can recurse with the same code path.
// ─────────────────────────────────────────────────────────────────────────

/// Local moving phase: each node greedily picks the neighbor community
/// that yields the largest positive Δmodularity. Repeats until no node
/// moves in a full sweep, or `max_moves_per_pass` is exhausted.
fn local_moving(adj: &[BTreeMap<usize, f64>], cfg: &LeidenConfig) -> Vec<usize> {
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
            // The actual modularity change of moving from current to c is
            //   ΔQ_move = ΔQ_join(c) − ΔQ_join(current) = gain(c) − baseline
            // so we track the best `delta` (gain − baseline), not the best
            // absolute `gain`. An earlier version compared `gain > 0`, which
            // silently rejected moves where both `baseline` and `gain` were
            // negative but `delta = gain − baseline > 0` (legitimate
            // improvements at high resolution / dense communities).
            let baseline = {
                let k_in = k_i_in.get(&current_comm).copied().unwrap_or(0.0);
                k_in / m - resolution * sigma_tot[current_comm] * k_i / (2.0 * m * m)
            };
            let mut best_comm = current_comm;
            let mut best_delta = 0.0;
            // BTreeMap iteration is in ascending key order, so when two
            // candidates yield the same delta the lower community id wins —
            // that's the deterministic tiebreak the previous comment
            // referenced.
            for (&c, &k_in) in &k_i_in {
                if c == current_comm {
                    continue;
                }
                let gain = k_in / m - resolution * sigma_tot[c] * k_i / (2.0 * m * m);
                let delta = gain - baseline;
                if delta > best_delta {
                    best_delta = delta;
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
        if is_excluded_community_file(&e.file) {
            continue;
        }
        set.insert(e.file.clone());
    }
    for r in references {
        if is_excluded_community_file(&r.file) {
            continue;
        }
        set.insert(r.file.clone());
    }
    set.into_iter().collect()
}

/// Files we never include as community nodes. Mirrors the dead-code
/// `is_non_source_file` filter — P5.15 external sentinels and sigil's
/// native JSON/YAML/TOML/Markdown entities aren't source code and
/// shouldn't appear as standalone single-file clusters that drown out
/// the real subsystem structure.
fn is_excluded_community_file(file: &str) -> bool {
    if file == "<external>" || file.starts_with("external:") {
        return true;
    }
    let lower = file.to_ascii_lowercase();
    for ext in &[
        ".md", ".markdown", ".rst", ".txt",
        ".yml", ".yaml", ".toml", ".json", ".jsonc",
        ".ini", ".cfg", ".conf",
        ".csv", ".tsv", ".xml",
    ] {
        if lower.ends_with(ext) {
            return true;
        }
    }
    false
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
            struct_hash: "deadbeefcafef00d".to_string(),
            visibility: None,
            rank: None,
            blast_radius: None,
            doc: None,
            heritage: Vec::new(),
            alias: None,        }
    }

    fn refr(file: &str, caller: Option<&str>, name: &str) -> Reference {
        Reference {
            file: file.to_string(),
            caller: caller.map(str::to_string),
            name: name.to_string(),
            ref_kind: "call".to_string(),
            line: 1,
            confidence: None,
            callee_id: None,
        }
    }

    /// For every node in `adj`, confirm that under the partition `comm`,
    /// no neighbor community offers a strictly-positive ΔQ relative to
    /// the node's current community. This is the convergence invariant
    /// `local_moving` is supposed to establish — if any positive delta
    /// exists, the algorithm stopped early.
    fn assert_no_beneficial_move(
        adj: &[BTreeMap<usize, f64>],
        comm: &[usize],
        cfg: &LeidenConfig,
    ) {
        let n = adj.len();
        // Recompute degrees and sigma_tot under the given partition.
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
        let two_m: f64 = degree.iter().sum();
        if two_m <= 0.0 {
            return;
        }
        let m = two_m / 2.0;
        let mut sigma_tot: Vec<f64> = vec![0.0; n];
        for (i, &c) in comm.iter().enumerate() {
            sigma_tot[c] += degree[i];
        }
        let resolution = cfg.resolution.max(0.0);

        for i in 0..n {
            let mut k_i_in: BTreeMap<usize, f64> = BTreeMap::new();
            for (&j, &w) in &adj[i] {
                if j == i {
                    continue;
                }
                *k_i_in.entry(comm[j]).or_insert(0.0) += w;
            }
            let current_comm = comm[i];
            let k_i = degree[i];
            // Tentatively isolate i for evaluation.
            let sigma_current = sigma_tot[current_comm] - k_i;
            let k_in_current = k_i_in.get(&current_comm).copied().unwrap_or(0.0);
            let baseline =
                k_in_current / m - resolution * sigma_current * k_i / (2.0 * m * m);
            for (&c, &k_in) in &k_i_in {
                if c == current_comm {
                    continue;
                }
                let gain = k_in / m - resolution * sigma_tot[c] * k_i / (2.0 * m * m);
                let delta = gain - baseline;
                assert!(
                    delta <= 1e-9,
                    "node {} stuck in comm {} has positive ΔQ to comm {}: \
                     baseline={:.6}, gain={:.6}, delta={:.6}",
                    i,
                    current_comm,
                    c,
                    baseline,
                    gain,
                    delta,
                );
            }
        }
    }

    #[test]
    fn local_moving_leaves_no_beneficial_move_unmade_at_high_resolution() {
        // Convergence contract: after local_moving returns, no node has a
        // strictly-positive ΔQ available to any of its neighbor
        // communities. A floor-at-zero on `best_gain` (the previous
        // implementation) could reject moves where gain(c) and baseline
        // were both negative but gain(c) > baseline; this property test
        // catches that exact failure mode.
        //
        // High resolution (γ = 4.0) magnifies the Σ_tot penalty so
        // baseline drops below zero on dense communities, the regime
        // where the floor bug bites hardest.
        let entities: Vec<Entity> = (0..6)
            .map(|i| ent(&format!("n{i}.rs"), &format!("f{i}")))
            .collect();
        let mut refs = Vec::new();
        // Dense triangle {0,1,2} and dense triangle {3,4,5}, joined by a
        // weak 0↔3 bridge — gives at least one node a sigma_tot-heavy
        // current community at higher resolutions.
        for clique in [[0, 1, 2], [3, 4, 5]] {
            for &a in &clique {
                for &b in &clique {
                    if a != b {
                        refs.push(refr(
                            &format!("n{a}.rs"),
                            Some("c"),
                            &format!("f{b}"),
                        ));
                    }
                }
            }
        }
        refs.push(refr("n0.rs", Some("c"), "f3"));
        refs.push(refr("n3.rs", Some("c"), "f0"));

        let cfg = LeidenConfig {
            resolution: 4.0,
            ..LeidenConfig::default()
        };
        let (nodes, adj) = build_graph(&entities, &refs).unwrap();
        let _ = nodes; // unused; assertion runs over adj indices
        let comm = local_moving(&adj, &cfg);
        assert_no_beneficial_move(&adj, &comm, &cfg);
    }

    #[test]
    fn empty_input_returns_no_clusters() {
        let r = HashMap::new();
        let out = detect_leiden(&[], &[], &r, &LeidenConfig::default());
        assert!(out.is_empty());
    }

    /// BFS-based check: every member of the cluster is reachable from
    /// `members[0]` along edges that connect two cluster members under
    /// the same edge rule the algorithm uses (`build_name_index` +
    /// `build_file_edges`). Used to enforce the Leiden internal-connectivity
    /// contract from `detect_leiden` output.
    fn cluster_is_internally_connected(
        members: &[String],
        entities: &[Entity],
        references: &[Reference],
    ) -> bool {
        if members.len() <= 1 {
            return true;
        }
        let member_set: BTreeSet<&str> = members.iter().map(|s| s.as_str()).collect();
        let name_index = build_name_index(entities);
        let mut adj: HashMap<&str, BTreeSet<&str>> = HashMap::new();
        for r in references {
            if !member_set.contains(r.file.as_str()) {
                continue;
            }
            let Some(targets) = name_index.get(r.name.as_str()) else {
                continue;
            };
            for &t in targets {
                if t == r.file.as_str() {
                    continue;
                }
                if member_set.contains(t) {
                    adj.entry(r.file.as_str()).or_default().insert(t);
                    adj.entry(t).or_default().insert(r.file.as_str());
                }
            }
        }
        let mut visited: BTreeSet<&str> = BTreeSet::new();
        let mut queue: VecDeque<&str> = VecDeque::from([members[0].as_str()]);
        while let Some(node) = queue.pop_front() {
            if !visited.insert(node) {
                continue;
            }
            if let Some(nbrs) = adj.get(node) {
                for &n in nbrs {
                    if !visited.contains(n) {
                        queue.push_back(n);
                    }
                }
            }
        }
        members.iter().all(|m| visited.contains(m.as_str()))
    }

    #[test]
    fn leiden_clusters_are_internally_connected() {
        // Leiden's headline guarantee over Louvain: every output community
        // is internally connected. We assert this on a fixture deliberately
        // shaped to stress Louvain's disconnection failure mode — a "ring
        // of weakly-bridged cliques" where greedy local moves can group
        // non-adjacent cliques into a single community. Even if Louvain
        // happens to satisfy connectivity here, the assertion locks the
        // invariant for any future refactor.
        //
        // Layout:
        //   3 cliques of 3 nodes each: {a1,a2,a3}, {b1,b2,b3}, {c1,c2,c3}.
        //   Single bridge edges: a3↔b1, b3↔c1, c3↔a1.
        let entities: Vec<Entity> = ["a1","a2","a3","b1","b2","b3","c1","c2","c3"]
            .iter()
            .map(|n| ent(&format!("{n}.rs"), &format!("f_{n}")))
            .collect();
        let mut refs = Vec::new();
        // Intra-clique dense refs (each node references the other two in its clique).
        for clique in [["a1","a2","a3"], ["b1","b2","b3"], ["c1","c2","c3"]] {
            for src in &clique {
                for dst in &clique {
                    if src != dst {
                        refs.push(refr(&format!("{src}.rs"), Some("c"), &format!("f_{dst}")));
                    }
                }
            }
        }
        // Bridges: single edge between adjacent cliques.
        refs.push(refr("a3.rs", Some("c"), "f_b1"));
        refs.push(refr("b3.rs", Some("c"), "f_c1"));
        refs.push(refr("c3.rs", Some("c"), "f_a1"));
        let clusters = detect_leiden(&entities, &refs, &HashMap::new(), &LeidenConfig::default());
        assert!(!clusters.is_empty(), "expected at least one cluster");
        for cluster in &clusters {
            assert!(
                cluster_is_internally_connected(&cluster.members, &entities, &refs),
                "Leiden returned an internally disconnected cluster: {:?}",
                cluster.members,
            );
        }
    }

    #[test]
    fn leiden_detect_partitions_two_cliques() {
        // Tracer bullet for the Leiden surface: same two-clique fixture used
        // for Louvain. Confirms `detect_leiden` exists, returns the public
        // `Cluster` shape, and produces the obvious partition. Behavioral
        // correctness across the algorithmic difference (connectivity) is
        // covered in a separate test.
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
            for (j, _) in xs.iter().enumerate() {
                if i != j {
                    refs.push(refr(src, Some("c"), &format!("f{}", j + 1)));
                }
            }
        }
        for (i, src) in ys.iter().enumerate() {
            for (j, _) in ys.iter().enumerate() {
                if i != j {
                    refs.push(refr(src, Some("c"), &format!("g{}", j + 1)));
                }
            }
        }
        let out = detect_leiden(&entities, &refs, &HashMap::new(), &LeidenConfig::default());
        assert_eq!(out.len(), 2, "leiden should detect the two cliques: {:?}", out);
        let sizes: Vec<usize> = out.iter().map(|c| c.size).collect();
        assert_eq!(sizes, vec![4, 4]);
    }

    #[test]
    fn isolated_files_each_become_their_own_cluster() {
        let entities = vec![ent("a.rs", "x"), ent("b.rs", "y"), ent("c.rs", "z")];
        let out = detect_leiden(&entities, &[], &HashMap::new(), &LeidenConfig::default());
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
        let out = detect_leiden(&entities, &refs, &HashMap::new(), &LeidenConfig::default());
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
        let first = detect_leiden(&entities, &refs, &HashMap::new(), &LeidenConfig::default());
        let second = detect_leiden(&entities, &refs, &HashMap::new(), &LeidenConfig::default());
        assert_eq!(first, second);
    }

    #[test]
    fn cluster_ids_are_compact_zero_indexed() {
        let entities = vec![ent("a.rs", "x"), ent("b.rs", "y"), ent("c.rs", "z")];
        let out = detect_leiden(&entities, &[], &HashMap::new(), &LeidenConfig::default());
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
        let out = detect_leiden(&entities, &refs, &rank, &LeidenConfig::default());
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
        let low_res = detect_leiden(
            &entities,
            &refs,
            &HashMap::new(),
            &LeidenConfig {
                resolution: 0.1,
                ..LeidenConfig::default()
            },
        );
        let high_res = detect_leiden(
            &entities,
            &refs,
            &HashMap::new(),
            &LeidenConfig {
                resolution: 5.0,
                ..LeidenConfig::default()
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
