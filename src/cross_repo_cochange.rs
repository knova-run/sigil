//! Cross-repo co-change mining.
//!
//! For each child git repo under a parent directory, mine the recent commit
//! history and find file pairs across repos that change within a short time
//! window of each other (default 24h). Emits one edge per file pair.
//!
//! This is the structural primitive the Knova runner uses to surface hidden
//! coupling between services that share no static link (e.g. a backend route
//! handler and the frontend client that calls it).

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Serialize, PartialEq)]
pub struct CrossRepoEdge {
    pub source_repo: String,
    pub source_file: String,
    pub target_repo: String,
    pub target_file: String,
    pub strength: f64,
    pub frequency: u32,
    pub last_unix: i64,
    /// ISO `yyyy-mm-dd` form of `last_unix` — matches repowise's
    /// `CrossRepoCoChange.last_date` so the schemas interoperate
    /// without conversion.
    pub last_date: String,
}

fn unix_to_iso_date(unix: i64) -> String {
    // Compute year-month-day from epoch seconds, UTC. Avoids pulling in a
    // full chrono dep for one date format.
    // Algorithm: days since 1970-01-01, walk year/month boundaries.
    // The forward-walking loops don't support pre-epoch timestamps; git
    // `%at` never produces negatives for real commits, but clamp defensively
    // so a corrupt input yields the epoch rather than `1970-01-00`.
    let mut days = unix.div_euclid(86_400).max(0);
    let mut year: i64 = 1970;
    loop {
        let leap = is_leap(year);
        let yd = if leap { 366 } else { 365 };
        if days < yd { break; }
        days -= yd;
        year += 1;
    }
    let months_normal = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 1;
    let mut day_of_year_remaining = days;
    for (i, m) in months_normal.iter().enumerate() {
        let len = if i == 1 && is_leap(year) { 29 } else { *m };
        if day_of_year_remaining < len { break; }
        day_of_year_remaining -= len;
        month += 1;
    }
    let day = day_of_year_remaining + 1;
    format!("{year:04}-{month:02}-{day:02}")
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

#[derive(Debug, Clone)]
pub struct CrossRepoConfig {
    /// Maximum seconds between two commits for them to count as
    /// temporally-correlated. 24h matches the repowise default.
    pub window_secs: i64,
    /// How many recent commits per repo to mine.
    pub commits_per_repo: usize,
    /// Minimum strength to surface in output. 0.0 = unfiltered.
    pub min_strength: f64,
}

impl Default for CrossRepoConfig {
    fn default() -> Self {
        Self {
            window_secs: 24 * 3600,
            commits_per_repo: 500,
            // Match repowise's MIN_CROSS_REPO_SCORE = 1.0 — drops one-off
            // co-occurrences that an exp-decay produces sub-unit scores
            // for. Sigil's pre-decay default of 0.0 was leaking thousands
            // of edges into co_changes.jsonl on full-history workspaces.
            min_strength: 1.0,
        }
    }
}

/// Decay constant for time-weighted co-change scoring. Mirrors
/// repowise's `_CO_CHANGE_DECAY_TAU = 180` (days) — a commit pair
/// 180 days ago contributes weight `1/e ≈ 0.368` instead of 1.0.
const CO_CHANGE_DECAY_TAU_DAYS: f64 = 180.0;

/// Hard cap on emitted edges, matching repowise's `_MAX_EDGES = 200`.
/// Prevents pathologically chatty repos from drowning out the signal;
/// the kept edges are the top-strength rows after decay-weighting.
const MAX_EDGES: usize = 200;

#[derive(Debug)]
struct ChangeEvent {
    repo: String,
    file: String,
    unix: i64,
}

/// Mine cross-repo edges across child repos under `parent`.
pub fn mine(parent: &Path, cfg: &CrossRepoConfig) -> Result<Vec<CrossRepoEdge>> {
    let mut events: Vec<ChangeEvent> = Vec::new();
    for repo_dir in discover_child_repos(parent) {
        let repo_name = repo_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| repo_dir.display().to_string());
        events.extend(commit_events(&repo_dir, &repo_name, cfg.commits_per_repo)?);
    }
    Ok(correlate(events, cfg))
}

/// Workspace-aware variant. Takes explicit `(member_name, member_path)`
/// pairs instead of sibling-walking a parent dir. Used by
/// `workspace::workspace_index` so cross-repo co-change folds into the
/// workspace pipeline regardless of where members live on disk.
pub fn mine_members<'a, I>(members: I, cfg: &CrossRepoConfig) -> Result<Vec<CrossRepoEdge>>
where
    I: IntoIterator<Item = (&'a str, &'a Path)>,
{
    let mut events: Vec<ChangeEvent> = Vec::new();
    for (name, path) in members {
        events.extend(commit_events(path, name, cfg.commits_per_repo)?);
    }
    Ok(correlate(events, cfg))
}

fn discover_child_repos(parent: &Path) -> Vec<PathBuf> {
    let mut repos = Vec::new();
    let entries = match std::fs::read_dir(parent) {
        Ok(e) => e,
        Err(_) => return repos,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && path.join(".git").exists() {
            repos.push(path);
        }
    }
    repos
}

fn commit_events(repo: &Path, repo_name: &str, max_commits: usize) -> Result<Vec<ChangeEvent>> {
    let output = Command::new("git")
        .args([
            "log",
            "--no-merges",
            "--name-only",
            "--pretty=format:===|%H|%at",
            &format!("-{}", max_commits),
        ])
        .current_dir(repo)
        .output()
        .with_context(|| format!("git log in {repo_name}"))?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut events = Vec::new();
    let mut current_unix: Option<i64> = None;
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("===|") {
            let mut parts = rest.split('|');
            let _hash = parts.next();
            let unix = parts.next().and_then(|s| s.parse::<i64>().ok());
            current_unix = unix;
            continue;
        }
        if line.is_empty() {
            continue;
        }
        if let Some(unix) = current_unix {
            events.push(ChangeEvent {
                repo: repo_name.to_string(),
                file: line.to_string(),
                unix,
            });
        }
    }
    Ok(events)
}

fn correlate(events: Vec<ChangeEvent>, cfg: &CrossRepoConfig) -> Vec<CrossRepoEdge> {
    use std::collections::BTreeMap;
    let mut events = events;
    events.sort_by_key(|e| e.unix);

    // Pin `now` to the max observed timestamp so the decay weighting is
    // deterministic across runs and indifferent to wall-clock drift —
    // matches the spirit of repowise's `now_ts` (which uses time.time()
    // but indexers tend to run right after a commit, so the answer is
    // close to the max in practice). For a sigil workspace that ran
    // months ago this is more reproducible than wall-clock.
    let now_ts = events.last().map(|e| e.unix).unwrap_or(0);

    // (s_repo, s_file, t_repo, t_file) -> (score, freq, last_unix)
    let mut pairs: BTreeMap<(String, String, String, String), (f64, u32, i64)> =
        BTreeMap::new();
    let n = events.len();
    for i in 0..n {
        let a = &events[i];
        for j in (i + 1)..n {
            let b = &events[j];
            if b.unix - a.unix > cfg.window_secs {
                break;
            }
            if a.repo == b.repo {
                continue;
            }
            // Order the pair canonically.
            let (s_repo, s_file, t_repo, t_file) = if (&a.repo, &a.file) < (&b.repo, &b.file) {
                (a.repo.clone(), a.file.clone(), b.repo.clone(), b.file.clone())
            } else {
                (b.repo.clone(), b.file.clone(), a.repo.clone(), a.file.clone())
            };
            // exp(-age_days / tau). Repowise applies decay to the LATER
            // commit's age (commit_b); mirror that so scores match.
            let age_days = ((now_ts - b.unix).max(0) as f64) / 86_400.0;
            let weight = (-age_days / CO_CHANGE_DECAY_TAU_DAYS).exp();

            let key = (s_repo, s_file, t_repo, t_file);
            let entry = pairs.entry(key).or_insert((0.0, 0, 0));
            entry.0 += weight;
            entry.1 += 1;
            entry.2 = entry.2.max(a.unix).max(b.unix);
        }
    }
    let mut edges: Vec<CrossRepoEdge> = pairs
        .into_iter()
        .map(|((s_repo, s_file, t_repo, t_file), (score, freq, last_unix))| {
            CrossRepoEdge {
                source_repo: s_repo,
                source_file: s_file,
                target_repo: t_repo,
                target_file: t_file,
                strength: (score * 100.0).round() / 100.0, // 2-decimal rounding like repowise
                frequency: freq,
                last_unix,
                last_date: unix_to_iso_date(last_unix),
            }
        })
        .filter(|e| e.strength >= cfg.min_strength)
        .collect();
    edges.sort_by(|a, b| b.strength.total_cmp(&a.strength));
    edges.truncate(MAX_EDGES);
    edges
}
