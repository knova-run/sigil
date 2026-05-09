//! `sigil workspace` — coordinator over multiple git repos under a parent
//! directory. Discovers child repos and exposes them as a uniform substrate
//! to the Knova runner's workspace-mode features.
//!
//! The structural-primitive subcommands (decisions, contracts, package-deps,
//! cochange --workspace) operate per-repo — they don't need a workspace
//! orchestrator. `sigil workspace scan` answers the prerequisite question:
//! "which child repos are in this workspace?" so callers can iterate.

use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, PartialEq)]
pub struct WorkspaceRepo {
    pub repo: String,
    pub path: String,
}

/// Discover child git repos under `parent`. A child is a directory with a
/// `.git/` (or .git file for submodules) directly inside it.
pub fn scan(parent: &Path) -> Vec<WorkspaceRepo> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(parent) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let git_meta = path.join(".git");
        if !git_meta.exists() {
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        out.push(WorkspaceRepo {
            repo: name,
            path: path.display().to_string(),
        });
    }
    out.sort_by(|a, b| a.repo.cmp(&b.repo));
    out
}

/// Helper for callers (e.g. multi-repo build scripts) that want each repo
/// path resolved to an absolute PathBuf.
pub fn paths(parent: &Path) -> Vec<PathBuf> {
    scan(parent)
        .into_iter()
        .map(|r| PathBuf::from(r.path))
        .collect()
}
