//! `sigil map` — budget-aware ranked digest of a codebase.
//!
//! The output is the orientation artifact agents read instead of grepping
//! around cold.
//!
//! 1. Score each file by its PageRank from `.sigil/rank.json`.
//! 2. For each entity in that file, score by `blast_radius.direct_files` and
//!    pick the top `--depth` per file.
//! 3. Greedily pack files in rank order until the token budget is exhausted.
//! 4. Render as Markdown (default) or JSON. `--write` tees to
//!    `.sigil/SIGIL_MAP.md` for the hook installers.
//!
//! Token estimation uses bytes/4 as a proxy — accurate enough for budgeting
//! decisions without a tokenizer dependency. If we ever need precision (e.g.
//! to publish benchmark numbers), we swap in `tiktoken-rs` behind a feature
//! flag.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::entity::{Entity, Reference};
use crate::query::index::Index;
use crate::rank::RankManifest;

/// Approximate token count for a byte slice. 1 token ≈ 4 bytes for English +
/// code on modern tokenizers. Off by ~20% in either direction — fine for
/// budget gating.
fn estimate_tokens(s: &str) -> usize {
    // Divide by 4, rounding up so zero-length strings still count as zero
    // but a 1-byte blob counts as 1 token.
    (s.len() + 3) / 4
}

/// Config knobs for a single `sigil map` invocation.
#[derive(Debug, Clone)]
pub struct MapOptions {
    /// Rough upper bound on output tokens. 0 = unlimited.
    pub tokens: usize,
    /// If Some, entities under this path prefix get a score multiplier so the
    /// digest centers on that subtree.
    pub focus: Option<String>,
    /// Max entities shown per file.
    pub depth: usize,
    /// When `focus` is set, multiply rank/blast scores for matching entities
    /// by this factor. 1.0 = no effect.
    pub focus_boost: f64,
    /// Drop entities whose file path matches common test-file conventions
    /// (`tests/`, `*_test.rs`, `*.spec.ts`, etc.). Default off — opt-in.
    pub exclude_tests: bool,
    /// Run community detection and add a "## Subsystems" section grouping
    /// shown files by cluster. Defaults on — cheap to compute and high
    /// orientation value on repos with more than a handful of files.
    pub clusters: bool,
    /// When > 0, attach a `top_entities` list to each subsystem with that
    /// many highest-impact entities (full `code.context`-shaped bundle:
    /// callers, callees, related types). 0 = preserve legacy shape, no
    /// `top_entities` field emitted. The flag exists to collapse the
    /// downstream "list subsystem files → list entities → call code.context
    /// per entity" N+1 pattern into a single map call.
    pub top_entities_per_subsystem: usize,
    /// Run Louvain modularity clustering and tag each shown file with
    /// `cluster_id`. Off by default — the field is opt-in so the default
    /// `sigil map` JSON stays byte-identical for existing consumers. See
    /// `sigil communities` for the standalone CLI.
    pub louvain_clusters: bool,
}

impl Default for MapOptions {
    fn default() -> Self {
        Self {
            tokens: 4000,
            focus: None,
            depth: 5,
            focus_boost: 2.0,
            exclude_tests: false,
            clusters: true,
            top_entities_per_subsystem: 0,
            louvain_clusters: false,
        }
    }
}

/// A single entity as it appears in the map.
#[derive(Debug, Clone, Serialize)]
pub struct MapEntity {
    pub name: String,
    pub kind: String,
    pub line_start: u32,
    pub line_end: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    /// Shortcut for downstream consumers — equal to
    /// `blast_radius.direct_files` when populated, else 0.
    pub impact_files: u32,
    pub direct_callers: u32,
    pub transitive_callers: u32,
}

/// One file block in the map.
#[derive(Debug, Clone, Serialize)]
pub struct MapFile {
    pub path: String,
    pub rank: f64,
    pub lang: Option<String>,
    pub entities: Vec<MapEntity>,
    /// Community id assigned by `community::detect_file_communities`. Omitted
    /// when clustering is disabled (see `MapOptions::clusters`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subsystem: Option<u32>,
    /// Cluster id assigned by `communities::detect` (Louvain modularity
    /// optimization). Additive to `subsystem` (label propagation) — the
    /// two fields can disagree; `cluster_id` is meant to be the canonical
    /// modularity-optimal grouping, surfaced for downstream consumers
    /// that want a stable clustering key without re-running the algorithm.
    /// Populated when `MapOptions::louvain_clusters` is true (default off
    /// to keep `sigil map` output byte-identical for existing callers).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cluster_id: Option<u32>,
}

/// Human-readable summary for one community in the map output.
#[derive(Debug, Clone, Serialize)]
pub struct MapSubsystem {
    pub id: u32,
    pub label: String,
    pub file_count: usize,
    /// Files in this cluster, sorted by rank desc (only those shown in
    /// the map — truncated files don't appear here).
    pub top_files: Vec<String>,
    /// Top-K highest-impact entities in this subsystem (any file, not just
    /// shown ones), with full `code.context`-shaped fields. Populated only
    /// when `MapOptions::top_entities_per_subsystem > 0`. Empty otherwise,
    /// elided from JSON output via `skip_serializing_if`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub top_entities: Vec<TopEntity>,
}

/// Reference edge (caller / callee / type usage) inside a `TopEntity`.
/// Mirrors the shape `code.context` uses so consumers can drop in the
/// same renderer logic.
#[derive(Debug, Clone, Serialize)]
pub struct TopEntityEdge {
    pub file: String,
    pub line: u32,
    pub symbol: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
}

/// One entity surfaced as a "load-bearing thing in this subsystem."
/// Field shape mirrors `code.context`'s output so an N+1 query pattern
/// (`map → context per entity`) collapses into a single `map` call.
#[derive(Debug, Clone, Serialize)]
pub struct TopEntity {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line_start: u32,
    pub line_end: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    /// Author-provided description (Python docstring, Rust ///, godoc,
    /// JSDoc, Javadoc, XML-doc, Doxygen). Mirrors `Entity.doc`. Skipped
    /// from JSON when None — entities without docs stay byte-identical.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    pub callers: Vec<TopEntityEdge>,
    pub callees: Vec<TopEntityEdge>,
    pub related_types: Vec<TopEntityEdge>,
}

/// Full map output.
#[derive(Debug, Clone, Serialize)]
pub struct Map {
    pub meta: MapMeta,
    pub files: Vec<MapFile>,
    pub skipped_file_count: usize,
    /// Subsystems detected via file-graph community detection. Empty when
    /// `MapOptions::clusters` is false.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub subsystems: Vec<MapSubsystem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MapMeta {
    pub sigil_version: String,
    pub total_files: usize,
    pub total_entities: usize,
    pub total_refs: usize,
    pub token_budget: usize,
    pub estimated_tokens: usize,
    pub focus: Option<String>,
}

/// Primary entry point. Given a loaded index + rank manifest, produce a
/// ranked, budget-packed digest. Pure function — no I/O.
pub fn build_map(idx: &Index, rank: &RankManifest, opts: &MapOptions) -> Map {
    // 1. Group entities by file. Sort each bucket by a blast×focus score
    //    so the top-`depth` entries are the load-bearing ones.
    let focus = opts.focus.as_deref();
    let focus_boost = opts.focus_boost.max(1.0);

    let mut by_file: HashMap<&str, Vec<&Entity>> = HashMap::new();
    for e in &idx.entities {
        // Skip imports in the map — they're noise at this view level. Real
        // consumers query `get_file_symbols` directly when they want them.
        if e.kind == "import" {
            continue;
        }
        if opts.exclude_tests && crate::entity::is_test_path(&e.file) {
            continue;
        }
        by_file.entry(e.file.as_str()).or_default().push(e);
    }

    // 2. Build a rank-ordered list of (file, file_rank) pairs. Files with no
    //    rank entry (parsed files without refs reaching out or in) fall to
    //    the bottom but still appear.
    let mut files_ranked: Vec<(&str, f64)> = by_file
        .keys()
        .map(|f| {
            let base = rank.file_rank.get(*f).copied().unwrap_or(0.0);
            let score = if focus.is_some_and(|p| f.starts_with(p)) {
                base * focus_boost
            } else {
                base
            };
            (*f, score)
        })
        .collect();
    files_ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // 3. Greedy packing within the token budget.
    let mut out_files: Vec<MapFile> = Vec::new();
    let mut estimated_tokens: usize = 0;
    // Reserve budget for the header/meta block (~100 tokens).
    let header_budget = 100usize;
    estimated_tokens += header_budget;

    for (file, file_score) in &files_ranked {
        // Never exit before adding at least one file — `--tokens 1` is
        // pathological but should still produce a non-empty map.
        if opts.tokens > 0 && estimated_tokens >= opts.tokens && !out_files.is_empty() {
            break;
        }

        let mut ents: Vec<&Entity> = by_file.get(*file).cloned().unwrap_or_default();
        ents.sort_by(|a, b| entity_score(b, focus, focus_boost).partial_cmp(&entity_score(a, focus, focus_boost)).unwrap_or(std::cmp::Ordering::Equal));
        ents.truncate(opts.depth.max(1));

        let rendered_entities: Vec<MapEntity> = ents
            .iter()
            .map(|e| {
                let br = e.blast_radius.as_ref();
                MapEntity {
                    name: e.name.clone(),
                    kind: e.kind.clone(),
                    line_start: e.line_start,
                    line_end: e.line_end,
                    parent: e.parent.clone(),
                    sig: e.sig.clone(),
                    visibility: e.visibility.clone(),
                    impact_files: br.map(|b| b.direct_files).unwrap_or(0),
                    direct_callers: br.map(|b| b.direct_callers).unwrap_or(0),
                    transitive_callers: br.map(|b| b.transitive_callers).unwrap_or(0),
                }
            })
            .collect();

        let mf = MapFile {
            path: file.to_string(),
            rank: *file_score,
            lang: lang_for(file).map(|s| s.to_string()),
            entities: rendered_entities,
            subsystem: None,
            cluster_id: None,
        };

        // Budget check on the rendered file block.
        let block = render_file_block(&mf);
        let block_tokens = estimate_tokens(&block);
        if opts.tokens > 0 && estimated_tokens + block_tokens > opts.tokens && !out_files.is_empty() {
            // Don't exceed budget; if this is the *first* file, we include it
            // anyway so --tokens 1 still returns something useful.
            break;
        }
        estimated_tokens += block_tokens;
        out_files.push(mf);
    }

    let skipped = files_ranked.len().saturating_sub(out_files.len());
    let (total_entities, total_refs) = idx.len();

    // Community detection — runs over the full index (not just shown
    // files) so cluster membership is stable across --tokens changes.
    // We only surface subsystems whose members appear in the rendered
    // file list, though; subsystems of purely truncated files are not
    // interesting to the agent consumer.
    let subsystems = if opts.clusters {
        attach_subsystems(&mut out_files, idx, opts.top_entities_per_subsystem)
    } else {
        Vec::new()
    };

    // Optional Louvain pass — populates MapFile.cluster_id when requested.
    // Runs over the full index (same as the label-propagation pass above)
    // so cluster assignments don't shift when `--tokens` truncates the
    // shown file list.
    if opts.louvain_clusters {
        let clusters = crate::communities::detect(
            &idx.entities,
            &idx.references,
            &rank.file_rank,
            &crate::communities::LouvainConfig::default(),
        );
        let mut file_to_cluster: std::collections::HashMap<String, u32> =
            std::collections::HashMap::new();
        for c in &clusters {
            for m in &c.members {
                file_to_cluster.insert(m.clone(), c.cluster_id);
            }
        }
        for f in out_files.iter_mut() {
            f.cluster_id = file_to_cluster.get(&f.path).copied();
        }
    }

    Map {
        meta: MapMeta {
            sigil_version: env!("CARGO_PKG_VERSION").to_string(),
            total_files: files_ranked.len(),
            total_entities,
            total_refs,
            token_budget: opts.tokens,
            estimated_tokens,
            focus: opts.focus.clone(),
        },
        files: out_files,
        skipped_file_count: skipped,
        subsystems,
    }
}

/// Run community detection, tag each MapFile with its subsystem id, and
/// return the summary list for rendering. Subsystems that contain only
/// truncated files are elided from the summary but remain discoverable
/// by re-running `sigil map` with a larger `--tokens` budget.
///
/// When `top_entities_n > 0`, each subsystem is also enriched with up to
/// `top_entities_n` highest-impact entities — see `build_top_entities`.
fn attach_subsystems(
    files: &mut [MapFile],
    idx: &Index,
    top_entities_n: usize,
) -> Vec<MapSubsystem> {
    let communities = crate::community::detect_file_communities(&idx.entities, &idx.references);
    // For each shown file, record its community id.
    for f in files.iter_mut() {
        f.subsystem = communities.get(&f.path).copied();
    }
    // Group shown files by community for the summary section.
    let mut by_comm: std::collections::BTreeMap<u32, Vec<&MapFile>> =
        std::collections::BTreeMap::new();
    for f in files.iter() {
        if let Some(id) = f.subsystem {
            by_comm.entry(id).or_default().push(f);
        }
    }
    let mut out: Vec<MapSubsystem> = by_comm
        .into_iter()
        .map(|(id, mut fs)| {
            fs.sort_by(|a, b| {
                b.rank
                    .partial_cmp(&a.rank)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let paths: Vec<&str> = fs.iter().map(|f| f.path.as_str()).collect();
            let label = crate::community::subsystem_label(&paths);
            // For top_entities, scope to ALL files in the community (not just
            // shown ones) — truncating-by-budget shouldn't hide load-bearing
            // entities that just happened to live in a low-rank file.
            let community_files: std::collections::HashSet<&str> = communities
                .iter()
                .filter(|(_, cid)| **cid == id)
                .map(|(p, _)| p.as_str())
                .collect();
            let top_entities = if top_entities_n > 0 {
                build_top_entities(idx, &community_files, top_entities_n)
            } else {
                Vec::new()
            };
            MapSubsystem {
                id,
                label,
                file_count: fs.len(),
                top_files: fs.iter().take(5).map(|f| f.path.clone()).collect(),
                top_entities,
            }
        })
        .collect();
    // Sort subsystems by the highest-ranked file in each — biggest clusters
    // tend to rise to the top, giving an agent the same "what to look at
    // first" signal the file list already uses.
    out.sort_by(|a, b| b.file_count.cmp(&a.file_count).then_with(|| a.label.cmp(&b.label)));
    out
}

/// Pick the top-K entities living in `community_files`, ordered by
/// `transitive_callers` desc, then `direct_callers` desc, then `name` asc.
/// For each, build a `TopEntity` with callers/callees/related_types
/// resolved against the index — same shape `code.context` returns.
///
/// Imports and tests are skipped — neither is the kind of thing a "what
/// is this subsystem about" question wants surfaced.
fn build_top_entities(
    idx: &Index,
    community_files: &std::collections::HashSet<&str>,
    n: usize,
) -> Vec<TopEntity> {
    let mut candidates: Vec<&Entity> = idx
        .entities
        .iter()
        .filter(|e| community_files.contains(e.file.as_str()))
        .filter(|e| e.kind != "import")
        .filter(|e| !crate::entity::is_test_path(&e.file))
        .collect();
    candidates.sort_by(|a, b| {
        let a_br = a.blast_radius.as_ref();
        let b_br = b.blast_radius.as_ref();
        let a_t = a_br.map(|x| x.transitive_callers).unwrap_or(0);
        let b_t = b_br.map(|x| x.transitive_callers).unwrap_or(0);
        let a_d = a_br.map(|x| x.direct_callers).unwrap_or(0);
        let b_d = b_br.map(|x| x.direct_callers).unwrap_or(0);
        b_t.cmp(&a_t)
            .then(b_d.cmp(&a_d))
            .then(a.name.cmp(&b.name))
    });
    candidates
        .into_iter()
        .take(n)
        .map(|e| build_top_entity(idx, e))
        .collect()
}

fn build_top_entity(idx: &Index, e: &Entity) -> TopEntity {
    // Cap caller/callee/related lists to keep payloads tractable. 10 mirrors
    // the default `code.context --depth`. Consumers wanting unbounded data
    // can call `code.context <name>` on the entity directly.
    const EDGE_CAP: usize = 10;

    let mut seen: std::collections::HashSet<(String, u32)> = std::collections::HashSet::new();
    let callers: Vec<TopEntityEdge> = idx
        .refs_to(&e.name)
        .filter(|r| seen.insert((r.file.clone(), r.line)))
        .take(EDGE_CAP)
        .map(top_caller_edge)
        .collect();

    let mut seen: std::collections::HashSet<(String, u32, String)> =
        std::collections::HashSet::new();
    let from_self: Vec<&Reference> = idx
        .refs_from(&e.name)
        .filter(|r| seen.insert((r.file.clone(), r.line, r.name.clone())))
        .collect();
    let (type_refs, call_refs): (Vec<&&Reference>, Vec<&&Reference>) = from_self
        .iter()
        .partition(|r| r.ref_kind == "type_annotation");
    let callees: Vec<TopEntityEdge> = call_refs
        .iter()
        .take(EDGE_CAP)
        .map(|r| top_callee_edge(r))
        .collect();
    let related_types: Vec<TopEntityEdge> = type_refs
        .iter()
        .take(EDGE_CAP)
        .map(|r| top_callee_edge(r))
        .collect();

    TopEntity {
        name: e.name.clone(),
        kind: e.kind.clone(),
        file: e.file.clone(),
        line_start: e.line_start,
        line_end: e.line_end,
        parent: e.parent.clone(),
        sig: e.sig.clone(),
        visibility: e.visibility.clone(),
        doc: e.doc.clone(),
        callers,
        callees,
        related_types,
    }
}

fn top_caller_edge(r: &Reference) -> TopEntityEdge {
    TopEntityEdge {
        file: r.file.clone(),
        line: r.line,
        symbol: r.name.clone(),
        kind: r.ref_kind.clone(),
        caller: r.caller.clone(),
    }
}

fn top_callee_edge(r: &Reference) -> TopEntityEdge {
    TopEntityEdge {
        file: r.file.clone(),
        line: r.line,
        symbol: r.name.clone(),
        kind: r.ref_kind.clone(),
        caller: r.caller.clone(),
    }
}

/// Per-entity sort key: `direct_files` as the primary axis, nudged upward
/// when the entity falls under `--focus` and when it's exported (since
/// callers usually care about the public surface).
fn entity_score(e: &Entity, focus: Option<&str>, focus_boost: f64) -> f64 {
    let base = e
        .blast_radius
        .as_ref()
        .map(|b| (b.direct_files as f64 + 1.0).ln() * 10.0 + b.direct_callers as f64)
        .unwrap_or(0.0);
    let visibility_boost = match e.visibility.as_deref() {
        Some("public") | Some("pub") => 1.5,
        Some("pub(crate)") | Some("crate") => 1.2,
        _ => 1.0,
    };
    let focus_mult = if focus.is_some_and(|p| e.file.starts_with(p)) {
        focus_boost
    } else {
        1.0
    };
    base * visibility_boost * focus_mult
}

/// Render the Markdown form of a Map. Public so tests + callers that want
/// the full string can grab it directly.
pub fn render_markdown(m: &Map) -> String {
    let mut out = String::with_capacity(8 * 1024);
    out.push_str("# Sigil Map\n\n");
    out.push_str(&format!(
        "{} files, {} entities, {} refs · sigil {}\n",
        m.meta.total_files, m.meta.total_entities, m.meta.total_refs, m.meta.sigil_version,
    ));
    if let Some(focus) = &m.meta.focus {
        out.push_str(&format!("focus: `{}`\n", focus));
    }
    out.push_str(&format!(
        "token budget: {} · estimated: {}\n\n",
        if m.meta.token_budget == 0 {
            "unlimited".to_string()
        } else {
            m.meta.token_budget.to_string()
        },
        m.meta.estimated_tokens,
    ));
    if !m.subsystems.is_empty() {
        out.push_str(&format!("## Subsystems ({})\n\n", m.subsystems.len()));
        for s in &m.subsystems {
            out.push_str(&format!(
                "- **{}** (#{}) — {} file(s): {}\n",
                s.label,
                s.id,
                s.file_count,
                s.top_files
                    .iter()
                    .take(3)
                    .map(|p| format!("`{}`", p))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            if !s.top_entities.is_empty() {
                // Tight, no-frills "Top Entities" line per subsystem — name,
                // kind, file:line, sig. Keeps the section scannable; full
                // bundle (callers/callees/related) lives in the JSON form.
                out.push_str("  - top entities:\n");
                for te in &s.top_entities {
                    let sig_part = te
                        .sig
                        .as_deref()
                        .map(|s| format!(" — `{}`", s.trim()))
                        .unwrap_or_default();
                    out.push_str(&format!(
                        "    - {} `{}` ({}:{}){}\n",
                        te.kind, te.name, te.file, te.line_start, sig_part
                    ));
                }
            }
        }
        out.push('\n');
    }

    out.push_str("## Top files by impact\n\n");

    for f in &m.files {
        out.push_str(&render_file_block(f));
    }

    if m.skipped_file_count > 0 {
        out.push_str(&format!(
            "\n_Truncated: {} more file(s) below budget. Increase `--tokens` or scope with `--focus` to see more._\n",
            m.skipped_file_count
        ));
    }

    out
}

fn render_file_block(f: &MapFile) -> String {
    let mut out = String::with_capacity(512);
    let lang_tag = f.lang.as_deref().unwrap_or("?");
    let subsystem_tag = f
        .subsystem
        .map(|id| format!(", subsystem #{}", id))
        .unwrap_or_default();
    out.push_str(&format!(
        "### {} — rank {:.4} ({}{})\n",
        f.path, f.rank, lang_tag, subsystem_tag
    ));
    if f.entities.is_empty() {
        out.push_str("_no symbols surfaced_\n\n");
        return out;
    }
    for e in &f.entities {
        let vis = e
            .visibility
            .as_deref()
            .filter(|v| !v.is_empty())
            .map(|v| format!(" [{}]", v))
            .unwrap_or_default();
        let parent = e
            .parent
            .as_deref()
            .map(|p| format!(" (in {})", p))
            .unwrap_or_default();
        let sig_line = e
            .sig
            .as_deref()
            .map(|s| format!("  `{}`", s.trim()))
            .unwrap_or_default();

        out.push_str(&format!(
            "- {} **{}**{}{} — blast {}f/{}c/{}t\n",
            e.kind, e.name, vis, parent, e.impact_files, e.direct_callers, e.transitive_callers
        ));
        if !sig_line.is_empty() {
            out.push_str(&format!("  {}\n", sig_line.trim_start()));
        }
    }
    out.push('\n');
    out
}

// ──────────────────────────────────────────────────────────────────────────
// I/O helpers: load rank manifest, write the map to disk.
// ──────────────────────────────────────────────────────────────────────────

/// Read `.sigil/rank.json`. Missing file → empty manifest (caller gets
/// uniform-ish scores, which is still useful — the map falls back to
/// listing files in arbitrary order rather than erroring).
pub fn load_rank_manifest(root: &Path) -> Result<RankManifest> {
    let path = root.join(".sigil").join("rank.json");
    if !path.exists() {
        return Ok(RankManifest {
            version: "1".to_string(),
            sigil_version: env!("CARGO_PKG_VERSION").to_string(),
            damping: 0.85,
            iterations_max: 0,
            transitive_depth: 0,
            file_count: 0,
            file_rank: HashMap::new(),
        });
    }
    let content =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&content)
        .with_context(|| format!("parse {} — was it produced by `sigil index`?", path.display()))
}

/// Write the Markdown form of a Map to `.sigil/SIGIL_MAP.md`.
pub fn write_sigil_map(m: &Map, root: &Path) -> Result<()> {
    let dir = root.join(".sigil");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("SIGIL_MAP.md"), render_markdown(m))?;
    Ok(())
}

fn lang_for(file: &str) -> Option<&'static str> {
    let ext = file.rsplit_once('.').map(|(_, e)| e)?;
    if let Some(lang) = crate::parser::languages::detect_language(ext) {
        return Some(lang);
    }
    match ext {
        "json" => Some("json"),
        "yaml" | "yml" => Some("yaml"),
        "toml" => Some("toml"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{BlastRadius, Entity};
    use crate::query::index::Index;

    fn ent(file: &str, name: &str, kind: &str, blast_files: u32, blast_callers: u32) -> Entity {
        Entity {
            file: file.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            line_start: 1,
            line_end: 2,
            parent: None,
            qualified_name: None,
            sig: None,
            meta: None,
            body_hash: None,
            sig_hash: None,
            struct_hash: "deadbeef".to_string(),
            visibility: None,
            rank: None,
            blast_radius: Some(BlastRadius {
                direct_callers: blast_callers,
                direct_files: blast_files,
                transitive_callers: 0,
            }),
            doc: None,
        }
    }

    fn manifest(file_rank: &[(&str, f64)]) -> RankManifest {
        RankManifest {
            version: "1".to_string(),
            sigil_version: "test".to_string(),
            damping: 0.85,
            iterations_max: 50,
            transitive_depth: 3,
            file_count: file_rank.len(),
            file_rank: file_rank
                .iter()
                .map(|(f, r)| (f.to_string(), *r))
                .collect(),
        }
    }

    #[test]
    fn top_entities_per_subsystem_zero_preserves_legacy_shape() {
        // Default (N = 0) must NOT change the wire shape — existing
        // consumers of `subsystems[]` should see no `top_entities` field.
        let idx = Index::build(
            vec![
                ent("a.rs", "foo", "function", 5, 10),
                ent("b.rs", "bar", "function", 5, 10),
            ],
            vec![],
        );
        let r = manifest(&[("a.rs", 0.5), ("b.rs", 0.4)]);
        let m = build_map(&idx, &r, &MapOptions::default());
        let json = serde_json::to_string(&m).unwrap();
        assert!(
            !json.contains("top_entities"),
            "default N=0 must not emit top_entities key"
        );
    }

    #[test]
    fn top_entities_per_subsystem_populates_when_enabled() {
        // When N > 0, every subsystem with member files gets a top_entities
        // list ordered by transitive_callers desc, direct_callers desc, name asc.
        let mut e_huge = ent("a.rs", "huge", "function", 50, 200);
        e_huge.blast_radius = Some(BlastRadius {
            direct_files: 50,
            direct_callers: 200,
            transitive_callers: 500,
        });
        let mut e_med = ent("a.rs", "medium", "function", 10, 30);
        e_med.blast_radius = Some(BlastRadius {
            direct_files: 10,
            direct_callers: 30,
            transitive_callers: 100,
        });
        let mut e_small = ent("a.rs", "small", "function", 1, 1);
        e_small.blast_radius = Some(BlastRadius {
            direct_files: 1,
            direct_callers: 1,
            transitive_callers: 5,
        });

        let idx = Index::build(vec![e_huge, e_med, e_small], vec![]);
        let r = manifest(&[("a.rs", 0.5)]);
        let m = build_map(
            &idx,
            &r,
            &MapOptions {
                tokens: 0,
                top_entities_per_subsystem: 2,
                ..MapOptions::default()
            },
        );
        // One subsystem covering a.rs.
        assert_eq!(m.subsystems.len(), 1);
        let s = &m.subsystems[0];
        assert_eq!(s.top_entities.len(), 2, "should respect N=2 cap");
        assert_eq!(s.top_entities[0].name, "huge", "highest transitive first");
        assert_eq!(s.top_entities[1].name, "medium");
    }

    #[test]
    fn top_entities_carry_context_shaped_fields() {
        let mut e = ent("a.rs", "foo", "function", 5, 10);
        e.sig = Some("fn foo()".to_string());
        e.parent = None;
        e.blast_radius = Some(BlastRadius {
            direct_files: 5,
            direct_callers: 10,
            transitive_callers: 20,
        });
        let idx = Index::build(
            vec![e],
            vec![
                crate::entity::Reference {
                    file: "b.rs".to_string(),
                    caller: Some("main".to_string()),
                    name: "foo".to_string(),
                    ref_kind: "call".to_string(),
                    line: 42,
                },
            ],
        );
        let r = manifest(&[("a.rs", 1.0), ("b.rs", 0.5)]);
        let m = build_map(
            &idx,
            &r,
            &MapOptions {
                tokens: 0,
                top_entities_per_subsystem: 5,
                ..MapOptions::default()
            },
        );
        let te = m
            .subsystems
            .iter()
            .flat_map(|s| s.top_entities.iter())
            .find(|t| t.name == "foo")
            .expect("foo must appear in top_entities");
        assert_eq!(te.kind, "function");
        assert_eq!(te.file, "a.rs");
        assert_eq!(te.sig.as_deref(), Some("fn foo()"));
        // Caller from b.rs is surfaced in `callers`.
        assert!(
            te.callers.iter().any(|c| c.file == "b.rs" && c.line == 42),
            "b.rs:42 caller missing from top_entities[0].callers"
        );
    }

    #[test]
    fn empty_index_produces_empty_map() {
        let idx = Index::default();
        let r = manifest(&[]);
        let m = build_map(&idx, &r, &MapOptions::default());
        assert!(m.files.is_empty());
        assert_eq!(m.meta.total_entities, 0);
    }

    #[test]
    fn files_ordered_by_rank_desc() {
        let idx = Index::build(
            vec![
                ent("a.rs", "foo", "function", 5, 10),
                ent("b.rs", "bar", "function", 5, 10),
                ent("c.rs", "baz", "function", 5, 10),
            ],
            vec![],
        );
        let r = manifest(&[("a.rs", 0.1), ("b.rs", 0.5), ("c.rs", 0.3)]);
        let m = build_map(&idx, &r, &MapOptions { tokens: 0, depth: 5, ..MapOptions::default() });
        let order: Vec<&str> = m.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(order, vec!["b.rs", "c.rs", "a.rs"]);
    }

    #[test]
    fn depth_caps_entities_per_file() {
        let idx = Index::build(
            (0..10)
                .map(|i| ent("a.rs", &format!("sym{i}"), "function", 5, 10))
                .collect(),
            vec![],
        );
        let r = manifest(&[("a.rs", 1.0)]);
        let m = build_map(
            &idx,
            &r,
            &MapOptions {
                tokens: 0,
                depth: 3,
                ..MapOptions::default()
            },
        );
        assert_eq!(m.files.len(), 1);
        assert_eq!(m.files[0].entities.len(), 3);
    }

    #[test]
    fn entities_within_file_sorted_by_blast() {
        let idx = Index::build(
            vec![
                ent("a.rs", "small", "function", 1, 1),
                ent("a.rs", "huge", "function", 50, 200),
                ent("a.rs", "medium", "function", 10, 30),
            ],
            vec![],
        );
        let r = manifest(&[("a.rs", 1.0)]);
        let m = build_map(&idx, &r, &MapOptions { tokens: 0, depth: 5, ..MapOptions::default() });
        let names: Vec<&str> = m.files[0].entities.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["huge", "medium", "small"]);
    }

    #[test]
    fn import_kind_entities_excluded_from_map() {
        let idx = Index::build(
            vec![
                ent("a.rs", "use foo::bar", "import", 5, 10),
                ent("a.rs", "RealStruct", "struct", 5, 10),
            ],
            vec![],
        );
        let r = manifest(&[("a.rs", 1.0)]);
        let m = build_map(&idx, &r, &MapOptions::default());
        let names: Vec<&str> = m.files[0].entities.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["RealStruct"]);
    }

    #[test]
    fn token_budget_truncates_files() {
        let idx = Index::build(
            (0..20)
                .map(|i| ent(&format!("f{i}.rs"), "sym", "function", 5, 10))
                .collect(),
            vec![],
        );
        let r = manifest(
            &(0..20)
                .map(|i| (Box::leak(format!("f{i}.rs").into_boxed_str()) as &'static str, 20.0 - i as f64))
                .collect::<Vec<_>>(),
        );
        let m = build_map(
            &idx,
            &r,
            &MapOptions {
                tokens: 120, // small budget, only a couple of file blocks fit
                depth: 1,
                ..MapOptions::default()
            },
        );
        assert!(m.files.len() < 20);
        assert!(m.skipped_file_count > 0);
        assert!(m.meta.estimated_tokens <= 120 + 200, "budget gate should be roughly honored");
    }

    #[test]
    fn first_file_always_shown_even_on_tiny_budget() {
        // --tokens 1 is pathological but should still produce a non-empty map.
        let idx = Index::build(vec![ent("a.rs", "foo", "function", 5, 10)], vec![]);
        let r = manifest(&[("a.rs", 1.0)]);
        let m = build_map(&idx, &r, &MapOptions { tokens: 1, depth: 1, ..MapOptions::default() });
        assert_eq!(m.files.len(), 1);
    }

    #[test]
    fn focus_prefix_boosts_matching_files() {
        let idx = Index::build(
            vec![
                ent("core/a.rs", "foo", "function", 5, 10),
                ent("tests/t.rs", "bar", "function", 5, 10),
            ],
            vec![],
        );
        let r = manifest(&[("core/a.rs", 0.1), ("tests/t.rs", 0.2)]); // tests ranked higher
        let unfocused = build_map(&idx, &r, &MapOptions { tokens: 0, ..MapOptions::default() });
        assert_eq!(unfocused.files[0].path, "tests/t.rs");

        let focused = build_map(
            &idx,
            &r,
            &MapOptions {
                tokens: 0,
                focus: Some("core/".to_string()),
                focus_boost: 3.0,
                ..MapOptions::default()
            },
        );
        assert_eq!(focused.files[0].path, "core/a.rs");
    }

    #[test]
    fn render_markdown_includes_top_entities_when_enabled() {
        let mut e = ent("a.rs", "load_bearing", "function", 50, 200);
        e.sig = Some("fn load_bearing()".to_string());
        e.blast_radius = Some(BlastRadius {
            direct_files: 50,
            direct_callers: 200,
            transitive_callers: 500,
        });
        let idx = Index::build(vec![e], vec![]);
        let r = manifest(&[("a.rs", 0.5)]);
        let md = render_markdown(&build_map(
            &idx,
            &r,
            &MapOptions {
                tokens: 0,
                top_entities_per_subsystem: 3,
                ..MapOptions::default()
            },
        ));
        assert!(md.contains("top entities:"), "missing top entities line: {md}");
        assert!(md.contains("load_bearing"), "missing entity name: {md}");
        assert!(md.contains("fn load_bearing()"), "missing sig: {md}");
    }

    #[test]
    fn render_markdown_omits_top_entities_when_disabled() {
        let idx = Index::build(vec![ent("a.rs", "foo", "function", 5, 10)], vec![]);
        let r = manifest(&[("a.rs", 0.5)]);
        let md = render_markdown(&build_map(&idx, &r, &MapOptions::default()));
        assert!(
            !md.contains("top entities:"),
            "default N=0 must not render top entities block"
        );
    }

    #[test]
    fn render_markdown_contains_header_and_file_blocks() {
        let idx = Index::build(vec![ent("a.rs", "foo", "function", 5, 10)], vec![]);
        let r = manifest(&[("a.rs", 0.5)]);
        let m = build_map(&idx, &r, &MapOptions::default());
        let md = render_markdown(&m);
        assert!(md.starts_with("# Sigil Map"));
        assert!(md.contains("## Top files by impact"));
        assert!(md.contains("a.rs"));
        assert!(md.contains("foo"));
    }

    #[test]
    fn estimated_tokens_roughly_matches_output_length() {
        let idx = Index::build(
            (0..5)
                .map(|i| ent(&format!("f{i}.rs"), "sym", "function", 5, 10))
                .collect(),
            vec![],
        );
        let r = manifest(
            &(0..5)
                .map(|i| (Box::leak(format!("f{i}.rs").into_boxed_str()) as &'static str, 1.0 - i as f64 * 0.1))
                .collect::<Vec<_>>(),
        );
        let m = build_map(&idx, &r, &MapOptions { tokens: 0, ..MapOptions::default() });
        let md = render_markdown(&m);
        let actual = estimate_tokens(&md);
        // The map's tracked estimate is within a factor of 2 of the actual
        // rendered length — good enough for budget gating.
        let tracked = m.meta.estimated_tokens as i64;
        let diff = (actual as i64 - tracked).abs();
        assert!(diff <= actual as i64 / 2 + 50, "tracked={tracked}, actual={actual}");
    }

    #[test]
    fn load_rank_manifest_missing_returns_empty() {
        let tmp = std::env::temp_dir().join(format!("sigil_map_load_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let m = load_rank_manifest(&tmp).expect("missing rank.json should fall through");
        assert!(m.file_rank.is_empty());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn load_rank_manifest_roundtrips() {
        let tmp = std::env::temp_dir().join(format!("sigil_map_rt_{}", std::process::id()));
        std::fs::create_dir_all(tmp.join(".sigil")).unwrap();
        let m = manifest(&[("a.rs", 0.25), ("b.rs", 0.75)]);
        std::fs::write(
            tmp.join(".sigil/rank.json"),
            serde_json::to_string(&m).unwrap(),
        )
        .unwrap();

        let loaded = load_rank_manifest(&tmp).unwrap();
        assert_eq!(loaded.file_count, 2);
        assert!((loaded.file_rank["b.rs"] - 0.75).abs() < 1e-9);
        std::fs::remove_dir_all(&tmp).ok();
    }
}
