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

/// Cross-repo resolution result. Issue #30 MVP.
///
/// Each row records that an `external:<modpath>` sentinel in the focus
/// repo has been re-bound to an actual entity definition in a sibling
/// repo's index. Confidence 0.4 reflects the inherent uncertainty of
/// cross-repo binding without a strict package-deps constraint.
#[derive(Debug, Serialize, PartialEq)]
pub struct WorkspaceResolution {
    /// modpath the external sentinel referenced (e.g. `utils.run`).
    pub external_modpath: String,
    /// Sibling repo where a matching definition was found.
    pub provider_repo: String,
    /// File in the provider repo defining the symbol.
    pub provider_file: String,
    /// Symbol's qualified-tail name (the segment after the last `.`/`::`).
    pub provider_symbol: String,
    /// Cross-repo binding confidence — fixed at 0.4 for the MVP.
    pub confidence: f64,
}

/// Cross-repo external-symbol resolution (issue #30 MVP).
///
/// Walks the focus repo's `.sigil/entities.jsonl` for `kind=="external"`
/// sentinels (these have `name = "external:<modpath>"` and
/// `file = "<external>"`). For each, scan every sibling repo's
/// `.sigil/entities.jsonl` for a non-external entity whose `name` (or
/// `qualified_name`) matches the modpath or its leaf segment. Emit a
/// `WorkspaceResolution` row per match.
///
/// MVP scope (per #30 open design questions):
///   * Manifest shape: sibling sigil dirs are auto-discovered via
///     `scan(workspace_root)`. No separate workspace.toml.
///   * Constraint shape: NO package-deps constraint yet — every sibling
///     is a candidate provider. Follow-up can intersect with the
///     `package-deps` edge set.
///   * Confidence floor: fixed at 0.4. Validate against real corpora
///     before promoting / parametrising.
///   * Sentinel handling: emit-alongside (don't mutate the focus index).
pub fn resolve_externals(
    workspace_root: &Path,
    focus_repo: &Path,
) -> Vec<WorkspaceResolution> {
    use serde_json::Value;
    let mut out: Vec<WorkspaceResolution> = Vec::new();

    let focus_entities = focus_repo.join(".sigil/entities.jsonl");
    let Ok(focus_text) = std::fs::read_to_string(&focus_entities) else {
        return out;
    };

    // Collect external modpaths from the focus repo.
    let mut wanted: Vec<String> = Vec::new();
    for line in focus_text.lines() {
        let Ok(e): Result<Value, _> = serde_json::from_str(line) else { continue };
        if e.get("kind").and_then(Value::as_str) != Some("external") {
            continue;
        }
        let Some(name) = e.get("name").and_then(Value::as_str) else { continue };
        if let Some(modpath) = name.strip_prefix("external:") {
            wanted.push(modpath.to_string());
        }
    }
    if wanted.is_empty() {
        return out;
    }

    // Resolve focus_repo's canonical path so we can compare against
    // sibling paths and skip itself.
    let focus_canonical = std::fs::canonicalize(focus_repo)
        .unwrap_or_else(|_| focus_repo.to_path_buf());

    for sibling in scan(workspace_root) {
        let sibling_path = std::path::PathBuf::from(&sibling.path);
        let sibling_canonical = std::fs::canonicalize(&sibling_path)
            .unwrap_or_else(|_| sibling_path.clone());
        if sibling_canonical == focus_canonical {
            continue;
        }
        let sibling_entities = sibling_path.join(".sigil/entities.jsonl");
        let Ok(text) = std::fs::read_to_string(&sibling_entities) else {
            continue;
        };
        for line in text.lines() {
            let Ok(e): Result<Value, _> = serde_json::from_str(line) else { continue };
            if e.get("kind").and_then(Value::as_str) == Some("external") {
                continue;
            }
            let name = e.get("name").and_then(Value::as_str).unwrap_or("");
            let file = e.get("file").and_then(Value::as_str).unwrap_or("");
            let qualified = e.get("qualified_name").and_then(Value::as_str);
            for w in &wanted {
                // Match the modpath against entity name OR qualified_name,
                // and also against the modpath's leaf segment so
                // `external:utils.run` finds entity `run` in utils.py.
                let leaf = w.rsplit(|c: char| c == '.' || c == '/').next().unwrap_or(w);
                if name == w || name == leaf || qualified == Some(w) || qualified == Some(leaf) {
                    out.push(WorkspaceResolution {
                        external_modpath: w.clone(),
                        provider_repo: sibling.repo.clone(),
                        provider_file: file.to_string(),
                        provider_symbol: name.to_string(),
                        confidence: 0.4,
                    });
                }
            }
        }
    }
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
