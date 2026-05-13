//! `sigil workspace` — coordinator over multiple git repos under a parent
//! directory. Discovers child repos and exposes them as a uniform substrate
//! to the Knova runner's workspace-mode features.
//!
//! Membership is explicit: users register repos with `workspace add` and
//! deregister with `workspace remove`. `workspace scan` is retained as a
//! discovery helper for bulk-add but never writes `members.json` itself.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Authoritative workspace membership list. Lives at
/// `<workspace-root>/.sigil-workspace/members.json`. Schema version 1.
#[derive(Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct WorkspaceManifest {
    pub version: u32,
    #[serde(default)]
    pub members: Vec<WorkspaceMember>,
}

/// One entry in `members.json`. `disabled` is omitted from JSON when
/// false to keep diffs small (the common case).
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct WorkspaceMember {
    pub name: String,
    pub path: String,
    pub added_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
}

fn is_false(b: &bool) -> bool { !*b }

/// Path to the workspace manifest under a given workspace root.
pub fn manifest_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".sigil-workspace").join("members.json")
}

/// Bootstrap a workspace: create `<root>/.sigil-workspace/members.json`
/// with an empty membership list at schema version 1.
///
/// Errors if `.sigil-workspace/` already exists unless `force=true`. With
/// `force`, an existing non-empty `members.json` is preserved (never
/// silently destructive); only a missing file is created.
pub fn init(workspace_root: &Path, force: bool) -> Result<()> {
    let dir = workspace_root.join(".sigil-workspace");
    let members = manifest_path(workspace_root);
    if dir.exists() && !force {
        return Err(anyhow!(
            "workspace already initialized at {} (use --force to re-init)",
            dir.display()
        ));
    }
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    if !members.exists() {
        let manifest = WorkspaceManifest { version: 1, members: Vec::new() };
        write_manifest(workspace_root, &manifest)?;
    }
    Ok(())
}

fn read_manifest(workspace_root: &Path) -> Result<WorkspaceManifest> {
    let p = manifest_path(workspace_root);
    if !p.exists() {
        return Err(anyhow!(
            "workspace not initialized at {} — run `sigil workspace init` first",
            workspace_root.display()
        ));
    }
    let text = std::fs::read_to_string(&p)
        .with_context(|| format!("reading {}", p.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("parsing {}", p.display()))
}

fn write_manifest(workspace_root: &Path, manifest: &WorkspaceManifest) -> Result<()> {
    let p = manifest_path(workspace_root);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(manifest)?;
    std::fs::write(&p, json + "\n")
        .with_context(|| format!("writing {}", p.display()))
}

/// Expand `~/...` against $HOME, absolutise the path without resolving
/// through symlinks. Lexically removes `.` segments (keeps `..` so we
/// don't traverse symlinks behind the user's back).
fn canonicalize_input(input: &Path) -> Result<PathBuf> {
    let expanded = if let Some(rest) = input
        .to_str()
        .and_then(|s| s.strip_prefix("~/"))
    {
        let home = std::env::var_os("HOME")
            .ok_or_else(|| anyhow!("$HOME not set; cannot expand ~"))?;
        PathBuf::from(home).join(rest)
    } else if input == Path::new("~") {
        let home = std::env::var_os("HOME")
            .ok_or_else(|| anyhow!("$HOME not set; cannot expand ~"))?;
        PathBuf::from(home)
    } else {
        input.to_path_buf()
    };

    let abs = std::path::absolute(&expanded)
        .with_context(|| format!("absolutising {}", expanded.display()))?;

    // Strip `.` components; preserve `..` (we don't resolve through
    // symlinks, so collapsing `..` lexically could mask the user's
    // intent).
    let mut out = PathBuf::new();
    for comp in abs.components() {
        match comp {
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    Ok(out)
}

/// Register a repo as a workspace member. See `add` semantics in
/// WORKSPACE_INDEXING_PLAN.md.
///
/// - Path is expanded (`~`, `.`) and absolutised; symlinks preserved.
/// - The repo must exist on disk and contain a `.git/` (file or dir, to
///   support submodules).
/// - Default name is the path's basename; `name_override` (--as) wins.
///   Collisions get a numeric suffix and emit a warning to stderr.
/// - Re-adding the same canonical path is a no-op (existing entry
///   returned; description / added_at / disabled NOT overwritten).
pub fn add(
    workspace_root: &Path,
    input_path: &Path,
    name_override: Option<&str>,
    description: Option<&str>,
    disabled: bool,
) -> Result<WorkspaceMember> {
    let canonical = canonicalize_input(input_path)?;
    if !canonical.is_dir() {
        return Err(anyhow!(
            "{} is not a directory (cannot add as workspace member)",
            canonical.display()
        ));
    }
    let git = canonical.join(".git");
    if !git.exists() {
        return Err(anyhow!(
            "{} has no .git/ (not a git repo)",
            canonical.display()
        ));
    }

    let mut manifest = read_manifest(workspace_root)?;
    let canonical_str = canonical.to_string_lossy().to_string();

    // Idempotent on canonical path
    if let Some(existing) = manifest
        .members
        .iter()
        .find(|m| m.path == canonical_str)
        .cloned()
    {
        return Ok(existing);
    }

    let basename = canonical
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .ok_or_else(|| anyhow!("path has no basename: {}", canonical.display()))?;

    let desired_name = name_override
        .map(str::to_string)
        .unwrap_or(basename);

    // Collision suffix
    let final_name = if manifest.members.iter().any(|m| m.name == desired_name) {
        let mut n = 2;
        loop {
            let candidate = format!("{}-{}", desired_name, n);
            if !manifest.members.iter().any(|m| m.name == candidate) {
                eprintln!(
                    "workspace add: name '{}' already taken, using '{}' instead (override with --as)",
                    desired_name, candidate
                );
                break candidate;
            }
            n += 1;
        }
    } else {
        desired_name
    };

    let member = WorkspaceMember {
        name: final_name,
        path: canonical_str,
        added_at: now_rfc3339(),
        description: description.map(str::to_string),
        disabled,
    };
    manifest.members.push(member.clone());
    write_manifest(workspace_root, &manifest)?;
    Ok(member)
}

/// Maximum cross-repo emissions per external sentinel. Prevents
/// pathological blow-up on common names (`run`, `init`, `main`).
const CROSS_REPO_CAP_PER_SENTINEL: usize = 10;

/// Read the canonical package/module name a workspace member advertises
/// via its top-level manifest. Used to detect direct `package-deps`
/// edges (consumer → provider) for the 0.6 confidence tier.
///
/// Returns the values across multiple manifests if a repo publishes more
/// than one (e.g. a multi-module Go repo). Order is unspecified.
fn member_canonical_names(member_path: &Path) -> Vec<String> {
    let mut out = Vec::new();

    // npm package.json
    let pkg = member_path.join("package.json");
    if let Ok(text) = std::fs::read_to_string(&pkg)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
        && let Some(name) = v.get("name").and_then(|n| n.as_str())
        && !name.is_empty()
    {
        out.push(name.to_string());
    }

    // go.mod (top-level only — multi-module repos are out of MVP scope)
    let gomod = member_path.join("go.mod");
    if let Ok(text) = std::fs::read_to_string(&gomod) {
        for line in text.lines() {
            if let Some(rest) = line.trim().strip_prefix("module ")
                && let Some(m) = rest.split_whitespace().next()
            {
                out.push(m.to_string());
                break;
            }
        }
    }

    // Cargo.toml — [package] name (workspace roots without [package] yield none)
    let cargo = member_path.join("Cargo.toml");
    if let Ok(text) = std::fs::read_to_string(&cargo)
        && let Ok(doc) = toml::from_str::<toml::Value>(&text)
        && let Some(name) = doc
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
    {
        out.push(name.to_string());
    }

    out
}

/// Read the set of dependency names this member declares in its
/// top-level manifests. Used by the 0.6 evidence check on the consumer
/// side of a cross-repo binding.
fn member_declared_deps(member_path: &Path) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();

    // package.json — dependencies + devDependencies + peerDependencies
    let pkg = member_path.join("package.json");
    if let Ok(text) = std::fs::read_to_string(&pkg)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
    {
        for section in &["dependencies", "devDependencies", "peerDependencies"] {
            if let Some(map) = v.get(*section).and_then(|x| x.as_object()) {
                for key in map.keys() {
                    out.insert(key.clone());
                }
            }
        }
    }

    // go.mod — every `require <dep> <ver>` line
    let gomod = member_path.join("go.mod");
    if let Ok(text) = std::fs::read_to_string(&gomod) {
        for edge in crate::package_deps::parse_go_mod("", "go.mod", &text) {
            out.insert(edge.dependency);
        }
    }

    // Cargo.toml — [dependencies] keys
    let cargo = member_path.join("Cargo.toml");
    if let Ok(text) = std::fs::read_to_string(&cargo)
        && let Ok(doc) = toml::from_str::<toml::Value>(&text)
        && let Some(deps) = doc.get("dependencies").and_then(|d| d.as_table())
    {
        for key in deps.keys() {
            out.insert(key.clone());
        }
    }

    out
}

/// Phase 3 cross-repo resolver. Walks every enabled member's
/// `external:<modpath>` sentinels and matches them against callable
/// definitions in other members. Writes Reference rows to
/// `<root>/.sigil-workspace/cross_repo_refs.jsonl` per the permissive
/// emission policy:
///
/// - Single match (one provider, one file): 0.6 if direct
///   `package-deps` edge consumer→provider, else 0.4
/// - Multiple matches: 0.3 each (one tier below the corresponding
///   single-match confidence)
/// - Cap: `CROSS_REPO_CAP_PER_SENTINEL` per sentinel; excess dropped
///   deterministically by `(provider_repo, provider_file)`
///
/// The emitted Reference rows already carry the workspace `<member>/`
/// prefix on `file` and `callee_id` so the Phase 2 union-load can
/// stitch them in without an extra rewrite pass.
pub fn resolve_workspace_cross_repo(workspace_root: &Path) -> Result<usize> {
    use serde_json::Value;

    let members: Vec<WorkspaceMember> = list(workspace_root)?
        .into_iter()
        .filter(|m| !m.disabled)
        .collect();

    if members.len() < 2 {
        // Nothing to cross-resolve. Always (re)write an empty file so
        // downstream consumers see a consistent placeholder and any
        // pre-existing rows from a larger membership get cleared.
        let cross = cross_repo_refs_path(workspace_root);
        if let Some(parent) = cross.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&cross, "")
            .with_context(|| format!("writing {}", cross.display()))?;
        return Ok(0);
    }

    // Provider index: leaf-name → Vec<(member_name, file, full_name)>.
    // Built once across every enabled member's entities.
    let mut providers: std::collections::HashMap<
        String,
        Vec<(String, String, String)>,
    > = std::collections::HashMap::new();

    // Per-member: canonical names this member advertises (npm package
    // name, Go module path, Cargo crate name). Used downstream to map a
    // consumer's declared deps back to provider members.
    let mut member_canonical: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    // Per-member: package names this member declares as dependencies.
    let mut member_deps: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();

    for m in &members {
        let mp = std::path::Path::new(&m.path);
        member_canonical.insert(m.name.clone(), member_canonical_names(mp));
        member_deps.insert(m.name.clone(), member_declared_deps(mp));

        let p = mp.join(".sigil/entities.jsonl");
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        for line in text.lines() {
            let Ok(v): Result<Value, _> = serde_json::from_str(line) else { continue };
            let kind = v.get("kind").and_then(Value::as_str).unwrap_or("");
            // Only callables can satisfy a cross-repo binding
            if !matches!(kind, "function" | "method" | "class" | "struct" | "interface" | "trait" | "fn") {
                continue;
            }
            let name = v.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let file = v.get("file").and_then(Value::as_str).unwrap_or("").to_string();
            if name.is_empty() || file.is_empty() || file == "<external>" {
                continue;
            }
            let leaf = leaf_segment(&name).to_string();
            providers
                .entry(leaf)
                .or_default()
                .push((m.name.clone(), file, name));
        }
    }

    let mut out_lines: Vec<String> = Vec::new();
    let mut emitted_count = 0usize;

    for consumer in &members {
        let consumer_ent_path = std::path::Path::new(&consumer.path).join(".sigil/entities.jsonl");
        let Ok(text) = std::fs::read_to_string(&consumer_ent_path) else { continue };
        for line in text.lines() {
            let Ok(v): Result<Value, _> = serde_json::from_str(line) else { continue };
            if v.get("kind").and_then(Value::as_str) != Some("external") {
                continue;
            }
            let raw_name = v.get("name").and_then(Value::as_str).unwrap_or("");
            let Some(modpath) = raw_name.strip_prefix("external:") else { continue };
            let leaf = leaf_segment(modpath);
            if leaf.is_empty() {
                continue;
            }

            // Candidates: providers other than the consumer itself
            let candidates: Vec<&(String, String, String)> = providers
                .get(leaf)
                .map(|v| v.iter().filter(|(member, _, _)| member != &consumer.name).collect())
                .unwrap_or_default();
            if candidates.is_empty() {
                continue;
            }

            // Stable order for deterministic cap behaviour
            let mut candidates = candidates;
            candidates.sort_by(|a, b| (a.0.as_str(), a.1.as_str()).cmp(&(b.0.as_str(), b.1.as_str())));

            let cap = CROSS_REPO_CAP_PER_SENTINEL.min(candidates.len());
            let is_single = candidates.len() == 1;

            for (provider_name, provider_file, provider_symbol) in candidates.iter().take(cap) {
                // Direct package-deps edge: consumer declares any of the
                // provider's canonical names as a dependency.
                let direct_dep_edge = member_canonical
                    .get(provider_name)
                    .map(|names| {
                        let deps = member_deps.get(&consumer.name);
                        match deps {
                            Some(s) => names.iter().any(|n| s.contains(n)),
                            None => false,
                        }
                    })
                    .unwrap_or(false);

                let confidence: f64 = if is_single {
                    if direct_dep_edge { 0.6 } else { 0.4 }
                } else {
                    // Ambiguous match: one tier below. Direct dep edge
                    // doesn't apply since the binding itself isn't unique.
                    0.3
                };

                let prefixed_provider_file = format!("{}/{}", provider_name, provider_file);
                let consumer_synthetic_file = format!("{}/<external>", consumer.name);
                let callee_id = format!("{}::{}", prefixed_provider_file, provider_symbol);

                let row = serde_json::json!({
                    "file": consumer_synthetic_file,
                    "caller": format!("external:{}", modpath),
                    "name": leaf,
                    "kind": "cross_repo_call",
                    "line": 0,
                    "confidence": confidence,
                    "callee_id": callee_id,
                });
                out_lines.push(row.to_string());
                emitted_count += 1;
            }
        }
    }

    let cross = cross_repo_refs_path(workspace_root);
    if let Some(parent) = cross.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let body = if out_lines.is_empty() {
        String::new()
    } else {
        let mut s = out_lines.join("\n");
        s.push('\n');
        s
    };
    std::fs::write(&cross, body)
        .with_context(|| format!("writing {}", cross.display()))?;

    Ok(emitted_count)
}

fn leaf_segment(s: &str) -> &str {
    s.rsplit(|c: char| c == '.' || c == '/' || c == ':')
        .next()
        .unwrap_or(s)
}

/// Drop a member from `members.json`. Lookup is by `name` first, then
/// by canonical path. If no member matches, a warning is printed to
/// stderr and the call succeeds (idempotent — see plan §1).
///
/// Returns `Ok(true)` if a member was dropped, `Ok(false)` if absent.
/// Per-repo `.sigil/` at the dropped repo is left untouched.
pub fn remove(workspace_root: &Path, name_or_path: &str) -> Result<bool> {
    let mut manifest = read_manifest(workspace_root)?;

    let canonical_input = canonicalize_input(Path::new(name_or_path)).ok();
    let canonical_str = canonical_input.as_ref().map(|p| p.to_string_lossy().to_string());

    let before = manifest.members.len();
    manifest.members.retain(|m| {
        let name_matches = m.name == name_or_path;
        let path_matches = canonical_str.as_deref().is_some_and(|c| m.path == c);
        !(name_matches || path_matches)
    });
    let dropped = manifest.members.len() < before;

    if !dropped {
        eprintln!(
            "workspace remove: '{}' is not a member of {} (nothing to do)",
            name_or_path,
            workspace_root.display()
        );
        return Ok(false);
    }

    write_manifest(workspace_root, &manifest)?;
    Ok(true)
}

/// Per-member fingerprint stored in `.sigil-workspace/manifest.json`.
/// Used by Phase 4 incremental refresh to skip members whose `.sigil/`
/// hasn't changed.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, Default)]
pub struct MemberStamp {
    pub entities_len: u64,
    pub entities_mtime_ms: i128,
    pub refs_len: u64,
    pub refs_mtime_ms: i128,
}

/// Top-level workspace stamp manifest. Keyed by member name. Lives at
/// `.sigil-workspace/manifest.json`.
#[derive(Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct StampManifest {
    pub version: u32,
    pub members: std::collections::BTreeMap<String, MemberStamp>,
}

fn stamp_manifest_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".sigil-workspace").join("manifest.json")
}

fn cross_repo_refs_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".sigil-workspace").join("cross_repo_refs.jsonl")
}

fn stamp_file(p: &Path) -> Result<(u64, i128)> {
    let meta = std::fs::metadata(p)
        .with_context(|| format!("stat {}", p.display()))?;
    let len = meta.len();
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i128)
        .unwrap_or(0);
    Ok((len, mtime_ms))
}

/// `sigil workspace index` — refresh stamps for every enabled member.
///
/// Phase 1 behaviour:
/// - Errors if no members exist (init not run, or empty manifest).
/// - Skips disabled members (kept in `members.json` but ignored here).
/// - Warns and skips members whose `path` is gone (e.g. unmounted). Does
///   NOT mutate `members.json` — the user is the sole authority on
///   membership.
/// - Auto-builds a member's `.sigil/` via `build_index` if it's missing.
/// - Stamps each member's `entities.jsonl` + `refs.jsonl` size/mtime
///   into `.sigil-workspace/manifest.json`.
/// - Writes an empty `cross_repo_refs.jsonl` placeholder (Phase 3 fills).
pub fn workspace_index(workspace_root: &Path) -> Result<()> {
    let manifest = read_manifest(workspace_root)?;
    let total = manifest.members.len();
    if total == 0 {
        return Err(anyhow!(
            "no members registered in {} — run `sigil workspace add <repo>` first",
            workspace_root.display()
        ));
    }

    let mut stamps = StampManifest { version: 1, members: Default::default() };
    let mut enabled_count = 0usize;
    let mut warnings = 0usize;

    for member in &manifest.members {
        if member.disabled {
            continue;
        }
        enabled_count += 1;

        let path = std::path::PathBuf::from(&member.path);
        if !path.exists() {
            eprintln!(
                "workspace index: member '{}' path {} no longer exists — skipping (members.json unchanged)",
                member.name, member.path
            );
            warnings += 1;
            continue;
        }

        let sigil_dir = path.join(".sigil");
        if !sigil_dir.exists() {
            eprintln!(
                "workspace index: auto-building .sigil/ for '{}' ({})",
                member.name, member.path
            );
            // Best-effort; the per-repo build pipeline writes its own
            // .sigil/ atomically. We don't need to inspect the result —
            // we re-stat below.
            let _ = crate::index::build_index(&path, None, false, true, true, false);
        }

        let entities = sigil_dir.join("entities.jsonl");
        let refs = sigil_dir.join("refs.jsonl");

        let (e_len, e_mtime) = stamp_file(&entities).unwrap_or((0, 0));
        let (r_len, r_mtime) = if refs.exists() {
            stamp_file(&refs).unwrap_or((0, 0))
        } else {
            (0, 0)
        };

        stamps.members.insert(
            member.name.clone(),
            MemberStamp {
                entities_len: e_len,
                entities_mtime_ms: e_mtime,
                refs_len: r_len,
                refs_mtime_ms: r_mtime,
            },
        );
    }

    if enabled_count == 0 {
        return Err(anyhow!(
            "no enabled members in {} — every member is disabled",
            workspace_root.display()
        ));
    }

    // Write stamps
    let stamp_path = stamp_manifest_path(workspace_root);
    if let Some(parent) = stamp_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let json = serde_json::to_string_pretty(&stamps)?;
    std::fs::write(&stamp_path, json + "\n")
        .with_context(|| format!("writing {}", stamp_path.display()))?;

    // Phase 3: walk external sentinels in every enabled member and write
    // resolved cross-repo bindings to `cross_repo_refs.jsonl`. Phase 2's
    // `Index::load_workspace` already appends this file on every query.
    let cross_emitted = resolve_workspace_cross_repo(workspace_root)?;

    eprintln!(
        "workspace index: stamped {} member(s){}, {} cross-repo ref(s)",
        stamps.members.len(),
        if warnings > 0 { format!(" ({} skipped)", warnings) } else { String::new() },
        cross_emitted
    );
    Ok(())
}

/// Read the current membership list. Used by `workspace list` and by
/// every downstream command (index, union-load, cross-repo resolution).
pub fn list(workspace_root: &Path) -> Result<Vec<WorkspaceMember>> {
    Ok(read_manifest(workspace_root)?.members)
}

/// Flip a member's `disabled` flag. Idempotent. Returns whether the
/// flag actually changed.
pub fn set_disabled(workspace_root: &Path, name_or_path: &str, disabled: bool) -> Result<bool> {
    let mut manifest = read_manifest(workspace_root)?;

    let canonical_input = canonicalize_input(Path::new(name_or_path)).ok();
    let canonical_str = canonical_input.as_ref().map(|p| p.to_string_lossy().to_string());

    let member = manifest.members.iter_mut().find(|m| {
        m.name == name_or_path || canonical_str.as_deref().is_some_and(|c| m.path == c)
    });
    let Some(member) = member else {
        return Err(anyhow!(
            "'{}' is not a member of {}",
            name_or_path,
            workspace_root.display()
        ));
    };

    let changed = member.disabled != disabled;
    member.disabled = disabled;
    if changed {
        write_manifest(workspace_root, &manifest)?;
    }
    Ok(changed)
}

/// RFC 3339 / ISO 8601 UTC timestamp without an external dep. Format:
/// `YYYY-MM-DDTHH:MM:SSZ`. Good enough for an audit trail.
fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    civil_datetime(secs)
}

/// Convert a Unix epoch second to `YYYY-MM-DDTHH:MM:SSZ`. Public for
/// tests that need to assert against a frozen clock.
fn civil_datetime(epoch_secs: i64) -> String {
    // Days since 1970-01-01 (proleptic Gregorian). Algorithm: Howard
    // Hinnant's date library — public domain.
    let secs_per_day: i64 = 86_400;
    let days = epoch_secs.div_euclid(secs_per_day);
    let secs_of_day = epoch_secs.rem_euclid(secs_per_day);

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    let h = secs_of_day / 3600;
    let mi = (secs_of_day / 60) % 60;
    let s = secs_of_day % 60;

    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, h, mi, s)
}

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
