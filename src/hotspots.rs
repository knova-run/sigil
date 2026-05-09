//! Hotspot detection: file churn (commit count) × complexity proxy
//! (current line count).
//!
//! Hotspots are files most likely to harbor bugs — high change frequency
//! amplifies the risk of any individual line. The score is the simplest
//! useful combination: `churn * lines`. Repowise stores this on
//! GitMetadata; sigil emits the JSONL primitive that downstream tooling
//! (Knova's risk pages, etc.) ingests.

use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Serialize, PartialEq)]
pub struct Hotspot {
    pub file: String,
    pub churn: u32,
    pub lines: u32,
    pub hotspot_score: f64,
}

/// Mine hotspots in `repo`. Returns rows sorted by score descending.
pub fn mine(repo: &Path, max_commits: usize) -> Result<Vec<Hotspot>> {
    let churn = file_churn(repo, max_commits)?;
    let mut out: Vec<Hotspot> = churn
        .into_iter()
        .filter_map(|(file, churn_n)| {
            let path = repo.join(&file);
            let lines = match std::fs::read_to_string(&path) {
                Ok(text) => text.lines().count() as u32,
                Err(_) => return None, // file deleted from working tree
            };
            let score = churn_n as f64 * lines as f64;
            Some(Hotspot {
                file,
                churn: churn_n,
                lines,
                hotspot_score: score,
            })
        })
        .collect();
    out.sort_by(|a, b| b.hotspot_score.total_cmp(&a.hotspot_score));
    Ok(out)
}

fn file_churn(repo: &Path, max_commits: usize) -> Result<HashMap<String, u32>> {
    let output = Command::new("git")
        .args([
            "log",
            "--no-merges",
            "--name-only",
            "--pretty=format:",
            &format!("-{}", max_commits),
        ])
        .current_dir(repo)
        .output()?;
    if !output.status.success() {
        return Ok(HashMap::new());
    }
    let mut counts: HashMap<String, u32> = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        *counts.entry(trimmed.to_string()).or_insert(0) += 1;
    }
    Ok(counts)
}
