//! Per-file ownership extraction from git history.
//!
//! Aggregates commit counts by author email per file. Emits the primary
//! owner (highest-commit author) and their share of total commits to
//! that file. Repowise stores these as `GitMetadata.ownership` and uses
//! them on file-page rendering ("owner: @alice (71%)").

use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Serialize, PartialEq)]
pub struct FileOwnership {
    pub file: String,
    pub primary_owner: String,
    pub ownership_pct: f64,
    pub author_count: u32,
    pub commit_count: u32,
}

/// Mine per-file ownership in `repo`. Walks the most recent `max_commits`
/// commits via git log; aggregates author email per touched file.
pub fn mine(repo: &Path, max_commits: usize) -> Result<Vec<FileOwnership>> {
    let output = Command::new("git")
        .args([
            "log",
            "--no-merges",
            "--name-only",
            "--pretty=format:===|%ae",
            &format!("-{}", max_commits),
        ])
        .current_dir(repo)
        .output()?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    // file -> author -> count
    let mut by_file: HashMap<String, HashMap<String, u32>> = HashMap::new();
    let mut current_email: Option<String> = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(rest) = line.strip_prefix("===|") {
            current_email = Some(rest.to_string());
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(email) = current_email.as_deref() {
            *by_file
                .entry(trimmed.to_string())
                .or_default()
                .entry(email.to_string())
                .or_insert(0) += 1;
        }
    }

    let mut out: Vec<FileOwnership> = by_file
        .into_iter()
        .map(|(file, authors)| {
            let total: u32 = authors.values().sum();
            let (primary, count) = authors
                .iter()
                .max_by_key(|(_, c)| **c)
                .map(|(e, c)| (e.clone(), *c))
                .unwrap_or_default();
            let pct = if total == 0 { 0.0 } else { (count as f64) * 100.0 / (total as f64) };
            FileOwnership {
                file,
                primary_owner: primary,
                ownership_pct: pct,
                author_count: authors.len() as u32,
                commit_count: total,
            }
        })
        .collect();
    out.sort_by(|a, b| b.commit_count.cmp(&a.commit_count));
    Ok(out)
}
