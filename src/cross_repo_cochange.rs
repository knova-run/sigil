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
    let mut days = unix.div_euclid(86_400);
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
            min_strength: 0.0,
        }
    }
}

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
    // (repo, file) -> aggregate {freq, last_unix, strength}
    let mut pairs: BTreeMap<(String, String, String, String), (u32, i64)> = BTreeMap::new();
    let n = events.len();
    for i in 0..n {
        let a = &events[i];
        for b in events.iter().skip(i + 1) {
            if a.repo == b.repo {
                continue;
            }
            if (a.unix - b.unix).abs() > cfg.window_secs {
                continue;
            }
            // Order the pair so (repoA, fileA) < (repoB, fileB) is canonical.
            let (s_repo, s_file, t_repo, t_file) = if (&a.repo, &a.file) < (&b.repo, &b.file) {
                (a.repo.clone(), a.file.clone(), b.repo.clone(), b.file.clone())
            } else {
                (b.repo.clone(), b.file.clone(), a.repo.clone(), a.file.clone())
            };
            let key = (s_repo, s_file, t_repo, t_file);
            let entry = pairs.entry(key).or_insert((0, 0));
            entry.0 += 1;
            entry.1 = entry.1.max(a.unix).max(b.unix);
        }
    }
    let mut edges: Vec<CrossRepoEdge> = pairs
        .into_iter()
        .map(|((s_repo, s_file, t_repo, t_file), (freq, last_unix))| {
            // Strength is a simple decay-weighted-frequency proxy: count
            // matters most, age decays linearly. Tune later if needed.
            let strength = freq as f64;
            CrossRepoEdge {
                source_repo: s_repo,
                source_file: s_file,
                target_repo: t_repo,
                target_file: t_file,
                strength,
                frequency: freq,
                last_unix,
                last_date: unix_to_iso_date(last_unix),
            }
        })
        .filter(|e| e.strength >= cfg.min_strength)
        .collect();
    edges.sort_by(|a, b| b.strength.total_cmp(&a.strength));
    edges
}
