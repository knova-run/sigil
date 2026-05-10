//! Bus-factor risk per file — derived from the same git log walk as
//! `sigil ownership`, but with a richer per-file payload.
//!
//! For each file: emit the primary owner's share, the second-place owner's
//! share, and a coarse risk band. A high primary share means knowledge is
//! concentrated in one person — losing them is expensive. Repowise / Knova
//! risk dashboards consume the rows as a single-engineer-failure signal.
//!
//! ## Algorithm
//!
//! 1. Walk `git log --no-merges --name-only --pretty=format:===|%ae -N`.
//! 2. Bucket commits by `(file, author_email)`.
//! 3. Per file, compute primary_share = top_author_count / total_commits;
//!    second_share = second_author_count / total_commits (0.0 if only one
//!    author).
//! 4. Map to `risk`:
//!        primary_share >= threshold  → high
//!        primary_share >= 0.6        → medium
//!        otherwise                   → low
//!    (threshold defaults to 0.8 — matches the issue spec.)

use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Serialize, PartialEq)]
pub struct BusFactor {
    pub path: String,
    pub primary_owner: String,
    pub primary_share: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub second_owner: Option<String>,
    pub second_share: f64,
    pub risk: &'static str,
}

/// Mine bus-factor signals in `repo`. Returns rows sorted by primary_share
/// descending, then by path ascending as a deterministic tiebreaker.
pub fn mine(repo: &Path, max_commits: usize, threshold: f64) -> Result<Vec<BusFactor>> {
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

    let mut out: Vec<BusFactor> = by_file
        .into_iter()
        .map(|(file, authors)| {
            // Sort authors by count desc, email asc for determinism.
            let mut ranked: Vec<(String, u32)> = authors.into_iter().collect();
            ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let total: u32 = ranked.iter().map(|(_, c)| *c).sum();
            let (primary_owner, primary_count) = ranked
                .first()
                .cloned()
                .unwrap_or_else(|| (String::new(), 0));
            let (second_owner, second_count) = match ranked.get(1) {
                Some((e, c)) => (Some(e.clone()), *c),
                None => (None, 0),
            };
            let primary_share = if total == 0 {
                0.0
            } else {
                primary_count as f64 / total as f64
            };
            let second_share = if total == 0 {
                0.0
            } else {
                second_count as f64 / total as f64
            };
            let risk = classify(primary_share, threshold);
            BusFactor {
                path: file,
                primary_owner,
                primary_share,
                second_owner,
                second_share,
                risk,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.primary_share
            .total_cmp(&a.primary_share)
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(out)
}

fn classify(primary_share: f64, threshold: f64) -> &'static str {
    if primary_share >= threshold {
        "high"
    } else if primary_share >= 0.6 {
        "medium"
    } else {
        "low"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_high_at_threshold() {
        assert_eq!(classify(0.8, 0.8), "high");
        assert_eq!(classify(0.95, 0.8), "high");
    }

    #[test]
    fn classify_medium_band() {
        assert_eq!(classify(0.6, 0.8), "medium");
        assert_eq!(classify(0.79, 0.8), "medium");
    }

    #[test]
    fn classify_low_below_60() {
        assert_eq!(classify(0.59, 0.8), "low");
        assert_eq!(classify(0.0, 0.8), "low");
    }

    #[test]
    fn classify_respects_custom_threshold() {
        // raise threshold — 0.8 share is now only medium
        assert_eq!(classify(0.8, 0.9), "medium");
        // lower threshold — 0.65 share is high
        assert_eq!(classify(0.65, 0.6), "high");
    }
}
