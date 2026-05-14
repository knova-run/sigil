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

/// One entry in `members.json`. `disabled` and `is_primary` are omitted
/// from JSON when false to keep diffs small (the common case).
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct WorkspaceMember {
    pub name: String,
    pub path: String,
    pub added_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
    /// First-added member is auto-marked primary. `workspace set-default`
    /// flips it. Repowise calls this `is_primary` on its RepoEntry —
    /// matches that schema. Downstream consumers (MCP, doc landing
    /// pages, default-repo query routing) read this to pick a "main"
    /// repo when none is specified.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_primary: bool,
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

    // First-added enabled member becomes the primary. Subsequent adds
    // don't change which member is primary — use `set_primary` for that.
    let is_primary = !disabled
        && !manifest.members.iter().any(|m| m.is_primary);

    let member = WorkspaceMember {
        name: final_name,
        path: canonical_str,
        added_at: now_rfc3339(),
        description: description.map(str::to_string),
        disabled,
        is_primary,
    };
    manifest.members.push(member.clone());
    write_manifest(workspace_root, &manifest)?;
    Ok(member)
}

/// Flip the `is_primary` flag onto exactly the named member; clears it
/// on all others. Returns the canonical name of the new primary.
pub fn set_primary(workspace_root: &Path, name_or_path: &str) -> Result<String> {
    let mut manifest = read_manifest(workspace_root)?;

    let canonical_input = canonicalize_input(Path::new(name_or_path)).ok();
    let canonical_str = canonical_input.as_ref().map(|p| p.to_string_lossy().to_string());

    let idx = manifest.members.iter().position(|m| {
        m.name == name_or_path || canonical_str.as_deref().is_some_and(|c| m.path == c)
    });
    let Some(idx) = idx else {
        return Err(anyhow!(
            "'{}' is not a member of {}",
            name_or_path,
            workspace_root.display()
        ));
    };
    if manifest.members[idx].disabled {
        return Err(anyhow!(
            "'{}' is disabled — enable it before making it the default repo",
            manifest.members[idx].name
        ));
    }
    let new_name = manifest.members[idx].name.clone();
    for m in &mut manifest.members {
        m.is_primary = false;
    }
    manifest.members[idx].is_primary = true;
    write_manifest(workspace_root, &manifest)?;
    Ok(new_name)
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

    // pyproject.toml — both modern `[project] name` (PEP 621) and the
    // legacy `[tool.poetry] name`. Python distribution names allow `-`
    // and `_` interchangeably (PEP 503 normalises both forms), so we
    // register both variants like we do for Cargo crates.
    let pyproj = member_path.join("pyproject.toml");
    if let Ok(text) = std::fs::read_to_string(&pyproj)
        && let Ok(doc) = toml::from_str::<toml::Value>(&text)
    {
        let project_name = doc
            .get("project")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str());
        let poetry_name = doc
            .get("tool")
            .and_then(|t| t.get("poetry"))
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str());
        for name in project_name.into_iter().chain(poetry_name) {
            out.push(name.to_string());
            if name.contains('-') {
                out.push(name.replace('-', "_"));
            } else if name.contains('_') {
                out.push(name.replace('_', "-"));
            }
        }
    }

    // Cargo.toml. Two shapes to handle:
    //   * single-crate repo: `[package] name = "foo"` at the root.
    //   * Cargo workspace: `[workspace] members = ["foo", "foo-macros", …]`
    //     where each member crate has its own `Cargo.toml` with
    //     `[package] name`. Tokio / tracing / axum all use this shape.
    //
    // For workspace members we register every inner crate name (dash
    // variant + Rust-import underscore variant) as a canonical alias.
    // `use tokio_stream::StreamExt` resolves a crate whose Cargo
    // manifest says `name = "tokio-stream"`.
    let cargo = member_path.join("Cargo.toml");
    if let Ok(text) = std::fs::read_to_string(&cargo)
        && let Ok(doc) = toml::from_str::<toml::Value>(&text)
    {
        // Single-package root.
        if let Some(name) = doc
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
        {
            push_crate_aliases(&mut out, name);
        }
        // Cargo workspace root.
        if let Some(members) = doc
            .get("workspace")
            .and_then(|w| w.get("members"))
            .and_then(|m| m.as_array())
        {
            for member in members {
                let Some(rel) = member.as_str() else { continue };
                // MVP: handle literal paths and a single trailing `/*`
                // glob (matches the same convention as `parse_manifest`).
                let candidates: Vec<PathBuf> = if let Some(prefix) = rel.strip_suffix("/*") {
                    let dir = member_path.join(prefix);
                    std::fs::read_dir(&dir)
                        .into_iter()
                        .flatten()
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| p.is_dir())
                        .collect()
                } else if rel.contains('*') {
                    Vec::new() // unsupported elaborate glob — silently skip
                } else {
                    vec![member_path.join(rel)]
                };
                for crate_dir in candidates {
                    let inner = crate_dir.join("Cargo.toml");
                    let Ok(inner_text) = std::fs::read_to_string(&inner) else { continue };
                    let Ok(inner_doc) = toml::from_str::<toml::Value>(&inner_text) else { continue };
                    if let Some(name) = inner_doc
                        .get("package")
                        .and_then(|p| p.get("name"))
                        .and_then(|n| n.as_str())
                    {
                        push_crate_aliases(&mut out, name);
                    }
                }
            }
        }
    }

    out
}

/// Push a Rust crate's name plus its `use`-form alias (dashes → underscores)
/// onto the canonical-name list. `tokio-stream` ⇒ register both
/// `tokio-stream` (for Cargo.toml dep matching) and `tokio_stream`
/// (for `use tokio_stream::...` modpath alignment).
fn push_crate_aliases(out: &mut Vec<String>, name: &str) {
    if name.is_empty() {
        return;
    }
    out.push(name.to_string());
    if name.contains('-') {
        out.push(name.replace('-', "_"));
    }
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
            // Only callables can satisfy a cross-repo binding. Mirrors
            // `is_callable_kind` in src/index.rs — `class`/`struct`/
            // `interface`/`trait` are data shapes, not call targets, so
            // a `external:foo/bar.fake()` shouldn't bind to a `struct
            // Fake {}` somewhere else.
            if !matches!(kind, "function" | "fn" | "method" | "constructor") {
                continue;
            }
            let name = v.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let file = v.get("file").and_then(Value::as_str).unwrap_or("").to_string();
            if name.is_empty() || file.is_empty() || file == "<external>" {
                continue;
            }
            // Skip vendored / build-output paths — they slipped past
            // `.gitignore` and into the index but they're not first-
            // party code, so they shouldn't satisfy cross-repo bindings.
            if file.starts_with("vendor/") || file.contains("/vendor/") || file.starts_with("node_modules/") {
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

            // Module-level cross-repo edge — when the modpath aligns
            // with another member's canonical name, emit a dep edge
            // even if no specific symbol resolves. Captures the common
            // Go pattern (`external:knative.dev/pkg/configmap`) and the
            // JS module-import pattern (`external:body-parser`) where
            // the external represents a package, not a function call.
            for m in &members {
                if m.name == consumer.name {
                    continue;
                }
                let Some(canonicals) = member_canonical.get(&m.name) else { continue };
                if canonicals.iter().any(|c| modpath_aligns_with_canonical(modpath, c)) {
                    let row = serde_json::json!({
                        "file": format!("{}/<external>", consumer.name),
                        "caller": format!("external:{}", modpath),
                        "name": modpath,
                        "kind": "cross_repo_module_dep",
                        "line": 0,
                        "confidence": 0.6,
                        "callee_id": format!("{}/", m.name),
                    });
                    out_lines.push(row.to_string());
                    emitted_count += 1;
                }
            }

            let leaf = leaf_segment(modpath);
            if leaf.is_empty() {
                continue;
            }

            // Candidates: providers other than the consumer itself.
            //
            // The external modpath itself has to align with the
            // provider — otherwise leaf-name match alone produces
            // wild false positives (e.g. `external:k8s.io/foo/fake`
            // binding to a `fake` test helper in knative/pkg just
            // because both have a `fake` leaf). We require the
            // modpath to either equal a provider's canonical name
            // (npm package, Go module path, Cargo crate name) OR
            // start with one of those names followed by `/` or `.`.
            // Bare relative imports (`./util`) are workspace-local
            // and never bind cross-repo.
            let candidates: Vec<&(String, String, String)> = providers
                .get(leaf)
                .map(|v| {
                    v.iter()
                        .filter(|(member, file, _)| {
                            if member == &consumer.name {
                                return false;
                            }
                            // Modpath has to match the provider's
                            // canonical identity. Otherwise this is
                            // most likely a third-party dep that
                            // happens to share a leaf name with a
                            // workspace symbol.
                            let canonical = member_canonical.get(member);
                            let aligned = match canonical {
                                Some(names) if !names.is_empty() => {
                                    names.iter().any(|n| modpath_aligns_with_canonical(modpath, n))
                                }
                                _ => {
                                    // No canonical name declared — accept
                                    // the candidate (legacy single/multi
                                    // match behaviour) so polyglot setups
                                    // that lack a manifest still get
                                    // partial coverage.
                                    true
                                }
                            };
                            if !aligned {
                                return false;
                            }
                            // Production code shouldn't bind to test
                            // fixtures. Cheap heuristic: the provider's
                            // file looks like a test file.
                            if file.ends_with("_test.go")
                                || file.contains("/test/") || file.starts_with("test/")
                                || file.contains("/tests/") || file.starts_with("tests/")
                                || file.contains("/__tests__/")
                                || file.ends_with(".test.js") || file.ends_with(".test.ts")
                                || file.ends_with(".spec.js") || file.ends_with(".spec.ts")
                            {
                                return false;
                            }
                            true
                        })
                        .collect()
                })
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
    // Deterministic output ordering (mirrors CLAUDE.md's "Entity output
    // is sorted deterministically by (file, line_start)" rule applied to
    // the cross-repo-refs JSONL artifact). Lexicographic sort over the
    // serialised JSON is stable and reproducible across runs regardless
    // of HashMap iteration order.
    out_lines.sort();
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

/// Result of a `--from-manifest` parse — a list of absolute paths the
/// manifest declares as workspace members. Glob patterns are already
/// expanded against the manifest's own directory.
#[derive(Debug, PartialEq)]
pub struct BulkAddPlan {
    pub manifest_kind: String,
    pub paths: Vec<PathBuf>,
}

/// Parse one of the supported workspace-manifest formats into a list of
/// member paths. Supported:
///   - `Cargo.toml` — `[workspace] members = [...]` (globs honored)
///   - `pnpm-workspace.yaml` — `packages: [...]` (globs honored)
///   - `package.json` — `workspaces: [...]` (globs honored)
///
/// Returns paths that exist on disk and contain a `.git/`; non-git
/// directories matched by a glob are skipped silently.
pub fn parse_manifest(manifest: &Path) -> Result<BulkAddPlan> {
    let base = manifest
        .parent()
        .ok_or_else(|| anyhow!("manifest has no parent dir: {}", manifest.display()))?;
    let name = manifest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    let (kind, patterns) = match name {
        "Cargo.toml" => ("cargo", patterns_from_cargo_toml(manifest)?),
        "pnpm-workspace.yaml" | "pnpm-workspace.yml" => {
            ("pnpm", patterns_from_pnpm_yaml(manifest)?)
        }
        "package.json" => ("npm", patterns_from_package_json(manifest)?),
        _ => {
            return Err(anyhow!(
                "unsupported manifest: {} — expected Cargo.toml, \
                 pnpm-workspace.yaml, or package.json",
                manifest.display()
            ));
        }
    };

    let mut paths: Vec<PathBuf> = Vec::new();
    for pat in &patterns {
        for resolved in expand_glob(base, pat) {
            if resolved.is_dir() && resolved.join(".git").exists() {
                paths.push(resolved);
            }
        }
    }
    paths.sort();
    paths.dedup();
    Ok(BulkAddPlan {
        manifest_kind: kind.to_string(),
        paths,
    })
}

fn patterns_from_cargo_toml(p: &Path) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(p)
        .with_context(|| format!("read {}", p.display()))?;
    let doc: toml::Value = toml::from_str(&text)
        .with_context(|| format!("parse {}", p.display()))?;
    let members = doc
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array());
    let mut out = Vec::new();
    if let Some(arr) = members {
        for v in arr {
            if let Some(s) = v.as_str() {
                out.push(s.to_string());
            }
        }
    }
    Ok(out)
}

fn patterns_from_pnpm_yaml(p: &Path) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(p)
        .with_context(|| format!("read {}", p.display()))?;
    let doc: serde_yml::Value = serde_yml::from_str(&text)
        .with_context(|| format!("parse {}", p.display()))?;
    let packages = doc.get("packages").and_then(|v| v.as_sequence());
    let mut out = Vec::new();
    if let Some(seq) = packages {
        for v in seq {
            if let Some(s) = v.as_str() {
                out.push(s.to_string());
            }
        }
    }
    Ok(out)
}

fn patterns_from_package_json(p: &Path) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(p)
        .with_context(|| format!("read {}", p.display()))?;
    let doc: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parse {}", p.display()))?;

    let mut out = Vec::new();
    // npm-style: "workspaces": ["pkg/*"]
    if let Some(arr) = doc.get("workspaces").and_then(|v| v.as_array()) {
        for v in arr {
            if let Some(s) = v.as_str() {
                out.push(s.to_string());
            }
        }
    }
    // yarn-style: "workspaces": { "packages": [...] }
    if let Some(arr) = doc
        .get("workspaces")
        .and_then(|w| w.get("packages"))
        .and_then(|v| v.as_array())
    {
        for v in arr {
            if let Some(s) = v.as_str() {
                out.push(s.to_string());
            }
        }
    }
    Ok(out)
}

/// Expand a workspace-manifest pattern relative to `base`. Supports the
/// `<dir>/*` suffix style (most common case across nx/pnpm/Cargo).
/// Non-glob patterns are returned as a single absolute path.
fn expand_glob(base: &Path, pattern: &str) -> Vec<PathBuf> {
    if let Some(prefix) = pattern.strip_suffix("/*") {
        let dir = base.join(prefix);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut out: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        out.sort();
        return out;
    }
    if pattern.contains('*') {
        // More elaborate globs not supported in the MVP; users can use
        // `<dir>/*` or list paths individually.
        eprintln!(
            "workspace: complex glob {:?} not supported — use `<dir>/*` or list explicit paths",
            pattern
        );
        return Vec::new();
    }
    vec![base.join(pattern)]
}

/// Apply a bulk-add plan: register each path as a member. Existing
/// canonical paths are skipped. Returns (added_count, skipped_count).
pub fn apply_bulk_add(workspace_root: &Path, plan: &BulkAddPlan) -> Result<(usize, usize)> {
    let mut added = 0usize;
    let mut skipped = 0usize;
    for path in &plan.paths {
        let canonical = canonicalize_input(path)?;
        let canonical_str = canonical.to_string_lossy().to_string();
        let manifest = read_manifest(workspace_root)?;
        let already = manifest.members.iter().any(|m| m.path == canonical_str);
        if already {
            skipped += 1;
            continue;
        }
        match add(workspace_root, path, None, None, false) {
            Ok(_) => added += 1,
            Err(e) => eprintln!(
                "workspace bulk-add: skipping {} ({})",
                path.display(),
                e
            ),
        }
    }
    Ok((added, skipped))
}

/// Compute the dry-run preview for a bulk-add plan. Returns (would-add,
/// already-member) path lists so the CLI can print a diff.
pub fn preview_bulk_add(
    workspace_root: &Path,
    plan: &BulkAddPlan,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let manifest = read_manifest(workspace_root)?;
    let existing: std::collections::HashSet<String> =
        manifest.members.iter().map(|m| m.path.clone()).collect();
    let mut would_add = Vec::new();
    let mut already = Vec::new();
    for path in &plan.paths {
        let canonical = canonicalize_input(path).unwrap_or_else(|_| path.clone());
        if existing.contains(&canonical.to_string_lossy().to_string()) {
            already.push(canonical);
        } else {
            would_add.push(canonical);
        }
    }
    Ok((would_add, already))
}

/// Marker line written into each member's post-commit hook. Lets
/// `uninstall` find and remove the sigil-managed entry without
/// disturbing user-authored hook code that might also live there.
const HOOK_BEGIN: &str = "# >>> sigil workspace hook (managed) >>>";
const HOOK_END: &str = "# <<< sigil workspace hook (managed) <<<";

/// `sigil workspace install` — drop a post-commit hook into each enabled
/// member's `.git/hooks/post-commit` that re-runs `sigil workspace index
/// --root <ws>` on every commit. Idempotent: re-running upserts the
/// managed block (between `HOOK_BEGIN` and `HOOK_END` markers); the
/// rest of the hook (user code) is left untouched.
pub fn install_hook(workspace_root: &Path) -> Result<usize> {
    let ws_abs = std::path::absolute(workspace_root)
        .with_context(|| format!("absolutising {}", workspace_root.display()))?;
    let members = list(workspace_root)?;
    let enabled: Vec<_> = members.into_iter().filter(|m| !m.disabled).collect();
    if enabled.is_empty() {
        return Err(anyhow!(
            "no enabled members in {} — `workspace install` has nothing to wire",
            workspace_root.display()
        ));
    }

    let body = format!(
        "{HOOK_BEGIN}\nsigil workspace index --root '{}' >/dev/null 2>&1 || true\n{HOOK_END}",
        ws_abs.display().to_string().replace('\'', "'\\''")
    );

    let mut installed = 0usize;
    for m in &enabled {
        let hook = std::path::Path::new(&m.path).join(".git/hooks/post-commit");
        let hooks_dir = match hook.parent() {
            Some(d) => d,
            None => continue,
        };
        std::fs::create_dir_all(hooks_dir)
            .with_context(|| format!("creating {}", hooks_dir.display()))?;

        let existing = std::fs::read_to_string(&hook).unwrap_or_default();
        let stripped = strip_managed_block(&existing);
        let new_content = if stripped.is_empty() {
            format!("#!/bin/sh\n{body}\n")
        } else if stripped.starts_with("#!") {
            format!("{}\n{body}\n", stripped.trim_end())
        } else {
            format!("#!/bin/sh\n{}\n{body}\n", stripped.trim_end())
        };

        std::fs::write(&hook, new_content)
            .with_context(|| format!("writing {}", hook.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&hook)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&hook, perms)
                .with_context(|| format!("chmod {}", hook.display()))?;
        }

        installed += 1;
    }
    Ok(installed)
}

/// `sigil workspace uninstall` — strip the sigil-managed block from
/// every enabled member's post-commit hook. Leaves user-authored hook
/// code alone. Idempotent.
pub fn uninstall_hook(workspace_root: &Path) -> Result<usize> {
    let members = list(workspace_root)?;
    let mut removed = 0usize;
    for m in &members {
        let hook = std::path::Path::new(&m.path).join(".git/hooks/post-commit");
        if !hook.exists() {
            continue;
        }
        let existing = std::fs::read_to_string(&hook).unwrap_or_default();
        if !existing.contains(HOOK_BEGIN) {
            continue;
        }
        let stripped = strip_managed_block(&existing);
        if stripped.trim().is_empty() || stripped.trim() == "#!/bin/sh" {
            // Nothing left — drop the file entirely so re-install
            // starts clean.
            std::fs::remove_file(&hook).ok();
        } else {
            std::fs::write(&hook, stripped.trim_end().to_string() + "\n")
                .with_context(|| format!("writing {}", hook.display()))?;
        }
        removed += 1;
    }
    Ok(removed)
}

/// Strip the sigil-managed block (everything between HOOK_BEGIN and
/// HOOK_END inclusive, plus a leading blank line if present) from a
/// hook file's contents. Used by install (to upsert) and uninstall.
fn strip_managed_block(text: &str) -> String {
    let mut out = String::new();
    let mut in_block = false;
    for line in text.lines() {
        if line.contains(HOOK_BEGIN) {
            in_block = true;
            // Drop trailing blank line we added before the block
            while out.ends_with("\n\n") {
                out.pop();
            }
            continue;
        }
        if in_block {
            if line.contains(HOOK_END) {
                in_block = false;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// True when an external sentinel's modpath plausibly points at a
/// provider with `canonical_name`. Covers three patterns:
///
///   * `modpath == canonical`           — e.g. `external:body-parser`
///     against an npm package named `body-parser`.
///   * `modpath starts with canonical + '/'` — e.g.
///     `external:knative.dev/pkg/logging` against a Go module whose
///     `go.mod` declares `module knative.dev/pkg`.
///   * `modpath starts with canonical + '.'` or `canonical::` — e.g.
///     `external:pydantic_core.PydanticUndefined` against a Python
///     package named `pydantic_core`.
fn modpath_aligns_with_canonical(modpath: &str, canonical: &str) -> bool {
    if modpath == canonical {
        return true;
    }
    if let Some(rest) = modpath.strip_prefix(canonical) {
        return rest.starts_with('/') || rest.starts_with('.') || rest.starts_with("::");
    }
    false
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
    let removed_was_primary = manifest
        .members
        .iter()
        .any(|m| {
            (m.name == name_or_path
                || canonical_str.as_deref().is_some_and(|c| m.path == c))
                && m.is_primary
        });
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

    // If we just removed the primary, promote the next enabled member
    // so the workspace always has a primary (when at least one member
    // remains enabled).
    if removed_was_primary
        && !manifest.members.iter().any(|m| m.is_primary)
        && let Some(next) = manifest.members.iter_mut().find(|m| !m.disabled)
    {
        next.is_primary = true;
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
    /// `git rev-parse HEAD` at index time. Lets downstream consumers
    /// know whether the member's source moved since the last workspace
    /// index, independent of whether its `.sigil/` files were rewritten
    /// (e.g. a commit that touched only test fixtures sigil ignores).
    /// Mirrors repowise's `RepoEntry.last_commit_at_index`. Absent when
    /// the member isn't a git repo or `git rev-parse` fails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_commit_sha: Option<String>,
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

fn co_changes_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".sigil-workspace").join("co_changes.jsonl")
}

fn contract_links_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".sigil-workspace").join("contract_links.jsonl")
}

fn contracts_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".sigil-workspace").join("contracts.jsonl")
}

/// One reverse-proxy rewrite rule: `<consumer_prefix>` is the path
/// shape consumers send (e.g. `/api/`); `<provider_prefix>` is what
/// arrives at the provider after the rewrite (e.g. `/`).
#[derive(Debug, Clone)]
pub struct ProxyRewrite {
    pub consumer_prefix: String,
    pub provider_prefix: String,
}

/// Discover reverse-proxy URL rewrites declared anywhere in the
/// workspace. Returns one rule per (consumer-prefix → provider-prefix)
/// mapping. Sources covered:
///
///   * nginx: `location /api/ { proxy_pass http://upstream/; }` — the
///     trailing slash on proxy_pass strips the location prefix.
///   * Caddy: `reverse_proxy /api/* upstream` — Caddyfile handler.
///   * Vercel: `"rewrites": [{ "source": "/api/:path*", "destination":
///     "http://backend/:path*" }]` in vercel.json / next.config.js.
///   * k8s Ingress: nginx.ingress.kubernetes.io/rewrite-target.
fn discover_proxy_rewrites(workspace_root: &Path) -> Vec<ProxyRewrite> {
    let mut out = Vec::new();
    let members = list(workspace_root).unwrap_or_default();
    for m in members.into_iter().filter(|m| !m.disabled) {
        let path = std::path::PathBuf::from(&m.path);
        walk_for_rewrites(&path, &mut out);
    }
    out
}

fn walk_for_rewrites(dir: &Path, out: &mut Vec<ProxyRewrite>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        if path.is_dir() {
            if matches!(name, ".git" | "node_modules" | "vendor" | "target" | "dist" | "build" | ".sigil" | ".sigil-workspace") {
                continue;
            }
            walk_for_rewrites(&path, out);
            continue;
        }
        // Stop reading huge files (binaries, lockfiles, etc.).
        if std::fs::metadata(&path).map(|m| m.len() > 1_000_000).unwrap_or(true) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        match name {
            "nginx.conf" | "default.conf" => extract_nginx_rewrites(&text, out),
            "Caddyfile" => extract_caddyfile_rewrites(&text, out),
            "vercel.json" | "now.json" => extract_vercel_rewrites(&text, out),
            _ if name.ends_with(".conf") && text.contains("proxy_pass") =>
                extract_nginx_rewrites(&text, out),
            _ => {}
        }
    }
}

fn extract_nginx_rewrites(text: &str, out: &mut Vec<ProxyRewrite>) {
    // Look for `location <prefix> { … proxy_pass <upstream>; … }` blocks.
    // When `proxy_pass` ends in `/` the location prefix is STRIPPED;
    // when it doesn't, the prefix is PRESERVED.
    // We do a simple block scan (brace-balanced) rather than a full
    // nginx grammar; covers the common single-line block shape too.
    static LOC_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let loc_re = LOC_RE.get_or_init(|| {
        regex::Regex::new(r"location\s+([^\s{]+)\s*\{").unwrap()
    });
    static PASS_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pass_re = PASS_RE.get_or_init(|| {
        regex::Regex::new(r"proxy_pass\s+([^\s;]+)\s*;").unwrap()
    });
    for loc_caps in loc_re.captures_iter(text) {
        let prefix = loc_caps[1].to_string();
        let block_start = loc_caps.get(0).unwrap().end();
        // Find the matching `}` by counting brace depth.
        let bytes = text.as_bytes();
        let mut depth = 1i32;
        let mut end = block_start;
        for i in block_start..bytes.len() {
            match bytes[i] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 { end = i; break; }
                }
                _ => {}
            }
        }
        let block = &text[block_start..end];
        let Some(pass_caps) = pass_re.captures(block) else { continue };
        let upstream = pass_caps[1].to_string();
        // Provider prefix = the path component of upstream after the
        // scheme+host. nginx semantics: when `proxy_pass` URI ends
        // with `/`, the location prefix gets stripped before
        // forwarding (i.e. provider_prefix is whatever appears in the
        // upstream URI minus the trailing slash); otherwise the full
        // location prefix is preserved verbatim.
        let upstream_path = upstream
            .strip_prefix("http://").or_else(|| upstream.strip_prefix("https://"))
            .and_then(|rest| rest.split_once('/'))
            .map(|(_host, p)| format!("/{p}"))
            .unwrap_or_default();
        let provider_prefix = if upstream.ends_with('/') {
            // Strip-mode: replace location prefix with the upstream path.
            if upstream_path.is_empty() { "/".to_string() } else { upstream_path }
        } else {
            // Preserve-mode: location prefix is forwarded verbatim.
            prefix.clone()
        };
        // Normalise trailing slashes so the rewrite-apply step matches.
        let cp = if prefix.ends_with('/') { prefix } else { format!("{prefix}/") };
        let pp = if provider_prefix.ends_with('/') { provider_prefix } else { format!("{provider_prefix}/") };
        out.push(ProxyRewrite { consumer_prefix: cp, provider_prefix: pp });
    }
}

fn extract_caddyfile_rewrites(text: &str, out: &mut Vec<ProxyRewrite>) {
    // Caddyfile: `reverse_proxy /api/* upstream:8080` — Caddy strips
    // nothing by default (the matcher path is forwarded as-is). For
    // sigil's rewrite mapping we treat it as a passthrough — record
    // the matcher prefix and leave provider_prefix identical so
    // consumers and providers both see the same path.
    //
    // The actual "rewrite" form in Caddy is `handle_path /api/* { reverse_proxy upstream }`
    // — `handle_path` strips the matcher path before forwarding.
    static HANDLE_PATH_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = HANDLE_PATH_RE.get_or_init(|| {
        regex::Regex::new(r"handle_path\s+(/[^\s{]+)").unwrap()
    });
    for caps in re.captures_iter(text) {
        let pat = caps[1].to_string();
        // Strip trailing `*` glob.
        let prefix = pat.trim_end_matches('*').to_string();
        let cp = if prefix.ends_with('/') { prefix } else { format!("{prefix}/") };
        // handle_path strips → provider_prefix = "/"
        out.push(ProxyRewrite { consumer_prefix: cp, provider_prefix: "/".to_string() });
    }
}

fn extract_vercel_rewrites(text: &str, out: &mut Vec<ProxyRewrite>) {
    // vercel.json: `"rewrites": [{ "source": "/api/:path*", "destination":
    // "http://backend/:path*" }]`. The `:path*` slug is shared, so the
    // effective mapping is `/api/` → `/`.
    let Ok(v): Result<serde_json::Value, _> = serde_json::from_str(text) else { return };
    let Some(rewrites) = v.get("rewrites").and_then(|r| r.as_array()) else { return };
    for r in rewrites {
        let Some(source) = r.get("source").and_then(|s| s.as_str()) else { continue };
        let Some(dest) = r.get("destination").and_then(|s| s.as_str()) else { continue };
        // Strip `:path*` / `:slug*` suffix from both sides; what's left
        // is the static prefix.
        let trim = |s: &str| -> String {
            static SUFFIX_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
            let re = SUFFIX_RE.get_or_init(|| regex::Regex::new(r":[A-Za-z_][A-Za-z0-9_]*\*?$").unwrap());
            re.replace(s, "").into_owned()
        };
        let src_prefix = trim(source);
        // dest may be a full URL — strip scheme+host.
        let dest_path = if dest.starts_with("http://") || dest.starts_with("https://") {
            dest.split_once("://")
                .and_then(|(_, rest)| rest.split_once('/'))
                .map(|(_, p)| format!("/{p}"))
                .unwrap_or_else(|| "/".to_string())
        } else {
            dest.to_string()
        };
        let dest_prefix = trim(&dest_path);
        let cp = if src_prefix.ends_with('/') { src_prefix } else { format!("{src_prefix}/") };
        let pp = if dest_prefix.ends_with('/') { dest_prefix.clone() } else { format!("{dest_prefix}/") };
        if !cp.is_empty() && !pp.is_empty() {
            out.push(ProxyRewrite { consumer_prefix: cp, provider_prefix: pp });
        }
    }
}

/// Apply a single rewrite to a consumer contract_id. Returns the
/// rewritten contract_id when the consumer_prefix is a prefix of the
/// path component; None otherwise.
fn apply_rewrite(contract_id: &str, rule: &ProxyRewrite) -> Option<String> {
    // Decompose `http::METHOD::/path...`.
    let parts: Vec<&str> = contract_id.splitn(3, "::").collect();
    if parts.len() != 3 || parts[0] != "http" { return None; }
    let method = parts[1];
    let path = parts[2];
    let cp = &rule.consumer_prefix;
    let pp = &rule.provider_prefix;
    if path.starts_with(cp) {
        let suffix = &path[cp.len()..];
        let new_path = if pp.ends_with('/') && !suffix.is_empty() {
            format!("{pp}{suffix}")
        } else if !pp.ends_with('/') && !suffix.is_empty() {
            format!("{pp}/{suffix}")
        } else {
            pp.trim_end_matches('/').to_string()
        };
        Some(format!("http::{method}::{new_path}"))
    } else {
        None
    }
}

/// Output row for cross-repo contract matching. Joins a provider in
/// one member with one or more consumers in others by normalized
/// `contract_id`. Mirrors repowise's `ContractLink` shape plus a
/// `confidence` field reflecting how the join was resolved.
#[derive(Debug, Serialize)]
pub struct ContractLink {
    pub contract_id: String,
    pub contract_type: String, // "http" | "grpc" | "topic"
    pub provider_repo: String,
    pub provider_file: String,
    pub provider_line: u32,
    pub provider_framework: String,
    pub consumer_repo: String,
    pub consumer_file: String,
    pub consumer_line: u32,
    pub consumer_framework: String,
    /// Tiers (exact values; consumers can filter on `confidence >= X`):
    ///   1.0  literal == literal in both repos.
    ///   0.9  both use `$ENV.X` AND both `.env` files agree on the value.
    ///   0.8  one side literal, other side `$ENV.X` resolves to that
    ///        literal via `.env` (mixed strategy).
    ///   0.6  both use `$ENV.X` but no `.env` file resolved the variable
    ///        on at least one side — name-only match.
    ///   0.4  both use `$ENV.X` and `.env` files resolved the variable
    ///        on BOTH sides but to DIFFERENT values — `notes` carries
    ///        the diff for the reviewer.
    pub confidence: f64,
    /// Which join strategy produced this link. One of:
    ///   `literal`    — both sides string-literal topics matched verbatim.
    ///   `env_value`  — both sides `$ENV.X`, env tables agree on value.
    ///   `env_name`   — both sides `$ENV.X`, env tables disagree or one
    ///                  is unresolved (see `notes`).
    ///   `mixed`      — one side literal, other side env-resolved.
    pub match_strategy: String,
    /// Optional human-readable note explaining a low-confidence link
    /// (e.g. "env values differ" / "no .env file found").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// Read `.env` / `.env.example` / `docker-compose.yml` / `values.yaml`
/// for a workspace member, returning a flat `VARNAME → value` map.
/// First definition wins (so the most-specific source — `.env`, which
/// is per-machine — overrides `.env.example`, which is checked in).
pub fn load_member_env_table(member_path: &Path) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    // Lowest-priority sources first; we won't overwrite once a key is set
    // by a higher-priority source.
    let candidates = [
        ".env.example", ".env.sample", ".env.template",
        ".env.local", ".env.development", ".env.production", ".env",
    ];
    for name in candidates {
        let p = member_path.join(name);
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') { continue; }
            let Some((k, v)) = trimmed.split_once('=') else { continue };
            let k = k.trim().to_string();
            let v = v.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
            // Strip optional `export` prefix.
            let k = k.strip_prefix("export ").map(|s| s.trim().to_string()).unwrap_or(k);
            // First-wins ordering already implied by candidate list ordering.
            out.entry(k).or_insert(v);
        }
    }
    // docker-compose.yml — top-level `services.<name>.environment` entries.
    for compose in ["docker-compose.yml", "docker-compose.yaml", "compose.yml", "compose.yaml"] {
        let p = member_path.join(compose);
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        let Ok(doc): Result<serde_yml::Value, _> = serde_yml::from_str(&text) else { continue };
        let Some(services) = doc.get("services").and_then(|s| s.as_mapping()) else { continue };
        for (_svc, svc_val) in services {
            let Some(env) = svc_val.get("environment") else { continue };
            // `environment` can be a list of `KEY=value` strings or a mapping.
            if let Some(map) = env.as_mapping() {
                for (k, v) in map {
                    let Some(key) = k.as_str() else { continue };
                    let val = v.as_str().unwrap_or("").to_string();
                    out.entry(key.to_string()).or_insert(val);
                }
            } else if let Some(seq) = env.as_sequence() {
                for item in seq {
                    let Some(s) = item.as_str() else { continue };
                    let Some((k, v)) = s.split_once('=') else { continue };
                    out.entry(k.trim().to_string()).or_insert(v.trim().to_string());
                }
            }
        }
    }
    out
}

/// Match providers in one workspace member against consumers in
/// another, joined by normalized `contract_id` (`http::METHOD::PATH`,
/// `grpc::Service/Method`, `topic::name`). One link per
/// (contract_id, provider_repo, consumer_repo) — the highest-confidence
/// pair when multiple sites exist.
pub fn resolve_workspace_contract_links(workspace_root: &Path) -> Result<usize> {
    use crate::contracts::ContractRow;
    let members: Vec<WorkspaceMember> = list(workspace_root)?
        .into_iter()
        .filter(|m| !m.disabled)
        .collect();

    let path = contract_links_path(workspace_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    if members.len() < 2 {
        std::fs::write(&path, "")?;
        return Ok(0);
    }

    // Per-contract: providers list + consumers list grouped by member.
    type Site = (String, ContractRow);
    let mut providers: std::collections::HashMap<String, Vec<Site>> = std::collections::HashMap::new();
    let mut consumers: std::collections::HashMap<String, Vec<Site>> = std::collections::HashMap::new();

    // Repowise-parity: also write every extracted contract row to
    // `.sigil-workspace/contracts.jsonl` so downstream consumers can
    // browse the full set even when no provider↔consumer match exists.
    let mut all_contracts: Vec<String> = Vec::new();

    // Per-member env table built from `.env*` / docker-compose.yml /
    // values.yaml. Used to resolve `topic::$ENV.X` contracts to their
    // literal values for stronger cross-repo matching.
    let mut env_tables: std::collections::HashMap<String, std::collections::HashMap<String, String>> =
        std::collections::HashMap::new();
    for m in &members {
        let mp = std::path::Path::new(&m.path);
        env_tables.insert(m.name.clone(), load_member_env_table(mp));
    }

    for m in &members {
        let mp = std::path::Path::new(&m.path);
        for c in crate::contracts::extract_from_root(mp) {
            // Persist with the member name prefixed so consumers can
            // address rows globally.
            let mut row = serde_json::to_value(&c)?;
            if let Some(obj) = row.as_object_mut() {
                obj.insert("repo".to_string(), serde_json::Value::String(m.name.clone()));
            }
            all_contracts.push(row.to_string());

            let id = c.contract_id.clone();
            match c.role.as_str() {
                // DB schema declaration acts as the join's "provider" — it
                // owns the table; consumer rows are the readers/writers.
                "provider" | "publisher" | "owner" => {
                    providers.entry(id).or_default().push((m.name.clone(), c));
                }
                // `reader` covers MongoDB collection reads emitted by
                // `mongo_collection_re` in contracts.rs; `writer` is
                // reserved for future direct INSERT/UPDATE detectors.
                // All three role values feed the consumer-side join.
                "consumer" | "subscriber" | "reader" | "writer" => {
                    consumers.entry(id).or_default().push((m.name.clone(), c));
                }
                _ => {}
            }
        }
    }

    // Write the full contracts dump (overwrite every run so removals
    // surface as deletions rather than stale rows). Sort for
    // deterministic row order across runs.
    let contracts_jsonl = contracts_path(workspace_root);
    all_contracts.sort();
    let body = if all_contracts.is_empty() {
        String::new()
    } else {
        let mut s = all_contracts.join("\n");
        s.push('\n');
        s
    };
    std::fs::write(&contracts_jsonl, body)
        .with_context(|| format!("writing {}", contracts_jsonl.display()))?;

    let mut out_lines: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String, String)> = std::collections::HashSet::new();

    // Helper: a provider's `http::*::/path` should match every consumer
    // verb at `/path`. Many frameworks (Django, Rails, gRPC `Handle`)
    // don't bind a method at the URL declaration site — the dispatch
    // happens inside the view. Without this fan-out, a Django backend
    // exposing `/users/login` (method=`*`) would never link to the
    // RealWorld frontend's `POST /users/login`.
    fn join_keys(id: &str) -> Vec<String> {
        // contract_id shape: `<kind>::<method>::<path>` (or `grpc::<svc>`
        // / `topic::<name>` which we don't fan out).
        let parts: Vec<&str> = id.splitn(3, "::").collect();
        if parts.len() != 3 || parts[0] != "http" {
            return vec![id.to_string()];
        }
        if parts[1] != "*" {
            return vec![id.to_string()];
        }
        // Provider is method-agnostic — emit a key for every HTTP verb so
        // the matcher catches any consumer that calls this path.
        ["GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS", "HEAD"]
            .iter()
            .map(|m| format!("{}::{m}::{}", parts[0], parts[2]))
            .collect()
    }

    // Pre-expand the providers map: for `http::*::/p` keys, also
    // register every verb-specific key pointing at the same site list.
    // This lets the inner consumer lookup find them with one HashMap
    // hit per contract_id.
    let mut expanded_providers: std::collections::HashMap<String, Vec<&Site>> =
        std::collections::HashMap::new();
    for (id, sites) in &providers {
        for join_id in join_keys(id) {
            expanded_providers
                .entry(join_id)
                .or_default()
                .extend(sites.iter());
        }
    }

    // Reverse-proxy rewrites. If any nginx / Caddy / Vercel config in
    // the workspace declares a prefix rewrite, fold each consumer
    // contract_id through every applicable rule and look it up under
    // the rewritten form too. Lets a frontend `/api/users` consumer
    // join a backend `/users` provider when nginx strips `/api/`.
    let rewrites = discover_proxy_rewrites(workspace_root);

    // Helpers for the env-aware match below ------------------------------
    fn env_name_from_id(id: &str) -> Option<String> {
        // `topic::$ENV.ORDERS_TOPIC` → `ORDERS_TOPIC`.
        id.split_once("::$ENV.").map(|(_, n)| n.to_string())
    }
    fn resolve_env(
        id: &str,
        member: &str,
        env_tables: &std::collections::HashMap<String, std::collections::HashMap<String, String>>,
    ) -> Option<String> {
        let env_name = env_name_from_id(id)?;
        env_tables.get(member)?.get(&env_name).cloned()
    }
    // For an env-keyed contract_id, return its literal-valued form by
    // looking up the env name in the member's env table.
    fn resolved_id(
        id: &str,
        member: &str,
        env_tables: &std::collections::HashMap<String, std::collections::HashMap<String, String>>,
    ) -> Option<String> {
        let val = resolve_env(id, member, env_tables)?;
        let (kind, _) = id.split_once("::").unwrap_or(("topic", ""));
        Some(format!("{kind}::{val}"))
    }

    // Pre-build a map of (literal contract_id) → Vec<(member, env_name)>
    // so we can match `topic::orders` (literal in repo A) against
    // `topic::$ENV.ORDERS_TOPIC` (env in repo B) when B's `.env`
    // resolves to `orders`.
    let mut env_providers_by_resolved_id: std::collections::HashMap<String, Vec<&Site>> =
        std::collections::HashMap::new();
    for (id, sites) in &providers {
        if env_name_from_id(id).is_some() {
            // Index each site's resolved id (from its own member's env table).
            for site in sites.iter() {
                if let Some(rid) = resolved_id(id, &site.0, &env_tables) {
                    for join_id in join_keys(&rid) {
                        env_providers_by_resolved_id.entry(join_id)
                            .or_default()
                            .push(site);
                    }
                }
            }
        }
    }

    for (id, cons_sites) in &consumers {
        // Build the lookup-IDs list with provenance so we can derive
        // the right confidence + match_strategy for each provider hit.
        // Each tuple is (lookup_id, base_confidence, strategy).
        let mut lookup_ids: Vec<(String, f64, &'static str)> = vec![
            (id.clone(), 1.0, "literal"),
        ];
        // Proxy rewrites — still literal-tier on both sides.
        for rule in &rewrites {
            if let Some(rewritten) = apply_rewrite(id, rule) {
                lookup_ids.push((rewritten, 1.0, "literal"));
            }
        }

        // Env-keyed consumer? Try matching against literal providers
        // via this consumer's env-table resolution (mixed strategy).
        let cons_env_name = env_name_from_id(id);
        if cons_env_name.is_some() {
            // For each consumer-site, try resolving its env var and
            // looking up under the literal form.
            for (cm, _) in cons_sites {
                if let Some(rid) = resolved_id(id, cm, &env_tables) {
                    lookup_ids.push((rid, 0.8, "mixed"));
                }
            }
        } else {
            // Literal consumer — also try matching against env-keyed
            // providers whose env value resolves to this same literal.
            if let Some(sites) = env_providers_by_resolved_id.get(id) {
                for s in sites {
                    let key = (id.clone(), s.0.clone(), "ENV_LITERAL_MATCH".to_string());
                    if seen.contains(&key) { continue; }
                    // Defer actual emission to the main loop by injecting
                    // a synthetic lookup via the env-resolved map below.
                    let _ = s; // placeholder; handled via combined merge.
                }
            }
        }

        let mut combined: Vec<(&Site, f64, &'static str)> = Vec::new();
        for (lookup_id, base_conf, strategy) in &lookup_ids {
            if let Some(sites) = expanded_providers.get(lookup_id) {
                combined.extend(sites.iter().map(|s| (*s, *base_conf, *strategy)));
            }
        }
        // Env-keyed literal merge: for literal consumer, pull in
        // env-keyed providers whose resolved value matches.
        if cons_env_name.is_none()
            && let Some(sites) = env_providers_by_resolved_id.get(id)
        {
            combined.extend(sites.iter().map(|s| (*s, 0.8, "mixed")));
        }

        if combined.is_empty() { continue; }

        // Highest confidence first, so when the same (provider, consumer)
        // pair is reachable via multiple lookup strategies (e.g. literal
        // match AND env-resolved mixed match), the literal-tier row wins
        // the `seen` dedup race rather than being silently demoted.
        combined.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        for (p, base_conf, strategy) in &combined {
            let (p_member, p_row) = (&p.0, &p.1);
            for (c_member, c_row) in cons_sites {
                if p_member == c_member {
                    continue;
                }

                // env-name == env-name path: if both sides used the
                // same `$ENV.X`, see what the .env files say.
                let mut confidence = *base_conf;
                let mut strategy = (*strategy).to_string();
                let mut notes: Option<String> = None;

                if let Some(env_name) = env_name_from_id(id) {
                    // Consumer is env-keyed. Provider's contract_id is
                    // the same string (same env name on both sides), so
                    // we're in the env_name / env_value tier.
                    let p_val = env_tables.get(p_member).and_then(|t| t.get(&env_name));
                    let c_val = env_tables.get(c_member).and_then(|t| t.get(&env_name));
                    match (p_val, c_val) {
                        (Some(pv), Some(cv)) if pv == cv => {
                            confidence = 0.9; strategy = "env_value".to_string();
                        }
                        (Some(pv), Some(cv)) => {
                            confidence = 0.4; strategy = "env_name".to_string();
                            notes = Some(format!(
                                "env values differ: {p_member}={pv} vs {c_member}={cv}"
                            ));
                        }
                        _ => {
                            confidence = 0.6; strategy = "env_name".to_string();
                            notes = Some("no .env file resolved this variable".to_string());
                        }
                    }
                }

                let key = (id.clone(), p_member.clone(), c_member.clone());
                if !seen.insert(key) {
                    continue;
                }
                let link = ContractLink {
                    contract_id: id.clone(),
                    contract_type: p_row.kind.clone(),
                    provider_repo: p_member.clone(),
                    provider_file: p_row.file.clone(),
                    provider_line: p_row.line,
                    provider_framework: p_row.framework.clone(),
                    consumer_repo: c_member.clone(),
                    consumer_file: c_row.file.clone(),
                    consumer_line: c_row.line,
                    consumer_framework: c_row.framework.clone(),
                    confidence,
                    match_strategy: strategy,
                    notes,
                };
                out_lines.push(serde_json::to_string(&link)?);
            }
        }
    }

    // Deterministic output ordering for contract_links.jsonl —
    // consumer iteration is HashMap-keyed, so without an explicit
    // sort the row order varies per run.
    out_lines.sort();
    let body = if out_lines.is_empty() {
        String::new()
    } else {
        let mut s = out_lines.join("\n");
        s.push('\n');
        s
    };
    std::fs::write(&path, body)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(out_lines.len())
}

/// Run cross-repo co-change mining across every enabled member and
/// write `co_changes.jsonl`. Reuses the existing `cross_repo_cochange`
/// engine — shape matches repowise's `CrossRepoCoChange` so downstream
/// consumers don't need adapter code.
pub fn resolve_workspace_co_changes(workspace_root: &Path) -> Result<usize> {
    let members: Vec<WorkspaceMember> = list(workspace_root)?
        .into_iter()
        .filter(|m| !m.disabled)
        .collect();
    let path = co_changes_path(workspace_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if members.len() < 2 {
        std::fs::write(&path, "")?;
        return Ok(0);
    }
    let cfg = crate::cross_repo_cochange::CrossRepoConfig::default();
    let pairs: Vec<(String, PathBuf)> = members
        .iter()
        .map(|m| (m.name.clone(), PathBuf::from(&m.path)))
        .collect();
    let iter = pairs
        .iter()
        .map(|(n, p)| (n.as_str(), p.as_path()));
    let edges = crate::cross_repo_cochange::mine_members(iter, &cfg)?;
    let mut body = String::new();
    for e in &edges {
        body.push_str(&serde_json::to_string(e)?);
        body.push('\n');
    }
    std::fs::write(&path, body)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(edges.len())
}

/// `git rev-parse HEAD` at `repo_path`. Returns `None` when the path
/// isn't a git repo, has no commits yet, or git is unavailable. Cheap
/// — runs once per member per `workspace index`.
fn git_head_sha(repo_path: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(s)
    } else {
        None
    }
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
    workspace_index_with_options(workspace_root, false)
}

/// `--full` wakes every member's auto-build path and forces stamp +
/// cross-repo + rank writes even when nothing changed. The default
/// (`full=false`) short-circuits when the new stamp set matches the
/// previous run (so `workspace index` twice in a row is a no-op).
pub fn workspace_index_with_options(workspace_root: &Path, full: bool) -> Result<()> {
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
            // Mirror the persistence pass `Cli::Index` does after
            // `build_index`: write entities + refs to JSONL, then run a
            // rank + blast-radius pass and persist rank.json. Without
            // this the `.sigil/` directory never lands on disk and the
            // subsequent stamp + cross-repo resolver runs against
            // nothing.
            let mut result = crate::index::build_index(
                &path, None, /* full */ true, /* include_refs */ true,
                /* tier3 */ true, /* verbose */ false,
            );
            if !result.refs.is_empty() {
                let cfg = crate::rank::RankConfig::default();
                let ranked = crate::rank::rank_with_config(&result.entities, &result.refs, &cfg);
                crate::rank::apply_blast_radius(&mut result.entities, &ranked);
                let rank_manifest = crate::rank::RankManifest::from_ranked(&ranked, &cfg);
                let _ = crate::writer::write_rank_json(&rank_manifest, &path, /* pretty */ false);
            }
            if let Err(e) = crate::writer::write_to_files(
                &result.entities, &result.refs, &path, /* pretty */ false,
            ) {
                eprintln!(
                    "workspace index: failed to write .sigil/ for '{}': {}",
                    member.name, e
                );
            }
        }

        let entities = sigil_dir.join("entities.jsonl");
        let refs = sigil_dir.join("refs.jsonl");

        let (e_len, e_mtime) = stamp_file(&entities).unwrap_or((0, 0));
        let (r_len, r_mtime) = if refs.exists() {
            stamp_file(&refs).unwrap_or((0, 0))
        } else {
            (0, 0)
        };
        let last_commit_sha = git_head_sha(&path);

        stamps.members.insert(
            member.name.clone(),
            MemberStamp {
                entities_len: e_len,
                entities_mtime_ms: e_mtime,
                refs_len: r_len,
                refs_mtime_ms: r_mtime,
                last_commit_sha,
            },
        );
    }

    if enabled_count == 0 {
        return Err(anyhow!(
            "no enabled members in {} — every member is disabled",
            workspace_root.display()
        ));
    }

    let stamp_path = stamp_manifest_path(workspace_root);

    // Phase 4: skip rewrites when the new stamp set matches the prior
    // run. `--full` overrides. Membership changes show up as a stamp-set
    // diff (different keys) and trigger a re-run; per-member content
    // changes show up as size/mtime drift on the existing keys.
    if !full && stamp_path.exists() {
        if let Ok(prior_text) = std::fs::read_to_string(&stamp_path)
            && let Ok(prior) = serde_json::from_str::<StampManifest>(&prior_text)
            && prior.version == stamps.version
            && prior.members == stamps.members
        {
            eprintln!(
                "workspace index: no changes since last run — {} member(s) up to date",
                stamps.members.len()
            );
            return Ok(());
        }
    }

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

    // Repowise-parity: mine cross-repo co-change edges from each
    // member's git log and write `co_changes.jsonl`. Best-effort — a
    // git failure on one member doesn't abort the whole index.
    let co_change_count = match resolve_workspace_co_changes(workspace_root) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("workspace index: co-change mining failed: {} (skipped)", e);
            0
        }
    };

    // Repowise-parity: match HTTP / gRPC / topic contracts across
    // members by normalized contract_id, write `contract_links.jsonl`.
    let contract_link_count = match resolve_workspace_contract_links(workspace_root) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("workspace index: contract matching failed: {} (skipped)", e);
            0
        }
    };

    // Workspace-level PageRank over the union-loaded graph (cross-repo
    // refs included). File-rank keys use the `<member.name>/<rel>`
    // naming that `Index::load_workspace` emits, so consumers like
    // `map::load_workspace_rank_manifest` and MCP's `get_overview`
    // resolve correctly. Best-effort — a parse failure on one member
    // doesn't abort the whole index.
    let rank_files = match write_workspace_rank(workspace_root) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("workspace index: rank computation failed: {} (skipped)", e);
            0
        }
    };

    eprintln!(
        "workspace index: stamped {} member(s){}, {} cross-repo ref(s), {} co-change edge(s), {} contract link(s), {} ranked file(s)",
        stamps.members.len(),
        if warnings > 0 { format!(" ({} skipped)", warnings) } else { String::new() },
        cross_emitted,
        co_change_count,
        contract_link_count,
        rank_files,
    );
    Ok(())
}

/// Load the rank manifest for a workspace. Prefers
/// `<workspace>/.sigil-workspace/rank.json` (written by `sigil workspace
/// index` with globally-correct PageRank over the union graph). Falls
/// back to merging each enabled member's `.sigil/rank.json` with
/// `<member.name>/` prefixes — still useful when workspace index hasn't
/// been run yet but per-repo ranks exist.
pub fn load_rank_manifest(workspace_root: &Path) -> crate::rank::RankManifest {
    use crate::rank::RankManifest;

    let ws_rank = workspace_root.join(".sigil-workspace").join("rank.json");
    if ws_rank.exists()
        && let Ok(content) = std::fs::read_to_string(&ws_rank)
        && let Ok(manifest) = serde_json::from_str::<RankManifest>(&content)
    {
        return manifest;
    }

    let manifest = match list(workspace_root) {
        Ok(m) => m,
        Err(_) => return RankManifest::default(),
    };

    let mut file_rank: std::collections::BTreeMap<String, f64> =
        std::collections::BTreeMap::new();
    for member in manifest {
        if member.disabled {
            continue;
        }
        let member_path = std::path::PathBuf::from(&member.path);
        let rank_path = member_path.join(".sigil").join("rank.json");
        if !rank_path.exists() {
            continue;
        }
        let content = match std::fs::read_to_string(&rank_path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let m: RankManifest = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let prefix = format!("{}/", member.name);
        for (file, rank) in m.file_rank {
            file_rank.insert(format!("{prefix}{file}"), rank);
        }
    }

    // Fallback-path metadata mirrors the parameters PageRank would have
    // used had it run (RankConfig::default()). Hard-coding zeros made
    // the manifest header lie about how the file_rank values were
    // produced; using the real defaults keeps it honest even though
    // these per-member rank.json files were computed independently
    // rather than over the union graph.
    let cfg = crate::rank::RankConfig::default();
    RankManifest {
        version: "1".to_string(),
        sigil_version: env!("CARGO_PKG_VERSION").to_string(),
        damping: cfg.damping,
        iterations_max: cfg.max_iterations,
        transitive_depth: cfg.transitive_depth,
        file_count: file_rank.len(),
        file_rank,
    }
}

/// Compute PageRank over the union-loaded workspace graph and write
/// `<workspace>/.sigil-workspace/rank.json`. File-rank keys carry the
/// `<member.name>/<rel>` prefix so downstream consumers (map's overview,
/// MCP's get_overview) hit the same names that `Index::load_workspace`
/// emits. Returns the number of ranked files. Empty graph → empty
/// manifest, written so consumers know the file is current.
fn write_workspace_rank(workspace_root: &Path) -> Result<usize> {
    let idx = crate::query::index::Index::load_workspace(workspace_root)
        .context("union-load workspace for rank pass")?;
    let cfg = crate::rank::RankConfig::default();
    let ranked = crate::rank::rank_with_config(&idx.entities, &idx.references, &cfg);
    let manifest = crate::rank::RankManifest::from_ranked(&ranked, &cfg);
    let rank_path = workspace_root.join(".sigil-workspace").join("rank.json");
    if let Some(parent) = rank_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let body = serde_json::to_string(&manifest)?;
    std::fs::write(&rank_path, body + "\n")
        .with_context(|| format!("writing {}", rank_path.display()))?;
    Ok(manifest.file_count)
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

/// True when a modpath is a standard-library import that shouldn't
/// participate in cross-repo resolution. Cross-repo binding only makes
/// sense for first-party code; stdlib bindings produce noise matched
/// against unrelated entities with colliding leaf names (issue #47.4).
///
/// Heuristics by modpath shape:
///   * Contains `::` → Rust style; stdlib iff first segment is
///     `std` / `core` / `alloc`.
///   * Contains `/` → Go style; stdlib iff the first segment has no `.`
///     (third-party modules always have a TLD like `github.com/...`).
///   * Otherwise → single-segment or dotted name; check against curated
///     Python + Node built-in sets.
fn is_stdlib_modpath(modpath: &str) -> bool {
    if modpath.is_empty() {
        return false;
    }

    if modpath.contains("::") {
        let first = modpath.split("::").next().unwrap_or("");
        return matches!(first, "std" | "core" | "alloc");
    }

    if modpath.contains('/') {
        let first = modpath.split('/').next().unwrap_or("");
        return !first.is_empty() && !first.contains('.');
    }

    let first_dot_seg = modpath.split('.').next().unwrap_or(modpath);
    PYTHON_STDLIB.binary_search(&first_dot_seg).is_ok()
        || NODE_BUILTINS.binary_search(&first_dot_seg).is_ok()
        || GO_STDLIB_TOPLEVEL.binary_search(&first_dot_seg).is_ok()
}

/// Go stdlib top-level package names (single-segment imports like
/// `context`, `errors`, `fmt`). Multi-segment Go stdlib like `net/url`
/// is caught by the slash-path branch above. Kept sorted for
/// `binary_search`.
const GO_STDLIB_TOPLEVEL: &[&str] = &[
    "bufio", "bytes", "cmp", "context", "errors", "expvar", "flag", "fmt",
    "hash", "io", "iter", "log", "maps", "math", "mime", "os", "path",
    "plugin", "reflect", "regexp", "runtime", "slices", "sort", "strconv",
    "strings", "structs", "sync", "syscall", "testing", "time", "unicode",
    "unique", "unsafe", "weak",
];

/// CPython 3.x stdlib top-level module names. Kept sorted for
/// `binary_search`. Sourced from <https://docs.python.org/3/library/>;
/// covers the modules typical imports reach for.
const PYTHON_STDLIB: &[&str] = &[
    "__future__", "abc", "argparse", "array", "ast", "asynchat", "asyncio",
    "asyncore", "atexit", "audioop", "base64", "bdb", "binascii", "bisect",
    "builtins", "bz2",
    // `cProfile` sorts BEFORE `calendar`: uppercase 'P' (0x50) < 'a'
    // (0x61). Keep this comment as a tripwire so future alphabetizing
    // by eye doesn't "fix" the apparent disorder and break binary_search.
    "cProfile",
    "calendar", "cgi", "cgitb", "chunk", "cmath", "cmd",
    "code", "codecs", "codeop", "collections", "colorsys", "compileall",
    "concurrent", "configparser", "contextlib", "contextvars", "copy",
    "copyreg", "crypt", "csv", "ctypes", "curses", "dataclasses",
    "datetime", "dbm", "decimal", "difflib", "dis", "distutils", "doctest",
    "email", "encodings", "ensurepip", "enum", "errno", "faulthandler",
    "fcntl", "filecmp", "fileinput", "fnmatch", "fractions", "ftplib",
    "functools", "gc", "genericpath", "getopt", "getpass", "gettext", "glob",
    "graphlib", "grp", "gzip", "hashlib", "heapq", "hmac", "html", "http",
    "idlelib", "imaplib", "imghdr", "imp", "importlib", "inspect", "io",
    "ipaddress", "itertools", "json", "keyword", "lib2to3", "linecache",
    "locale", "logging", "lzma", "mailbox", "mailcap", "marshal", "math",
    "mimetypes", "mmap", "modulefinder", "msilib", "msvcrt", "multiprocessing",
    "netrc", "nis", "nntplib", "ntpath", "numbers", "opcode", "operator",
    "optparse", "os", "ossaudiodev", "parser", "pathlib", "pdb", "pickle",
    "pickletools", "pipes", "pkgutil", "platform", "plistlib", "poplib",
    "posix", "posixpath", "pprint", "profile", "pstats", "pty", "pwd",
    "py_compile", "pyclbr", "pydoc", "pyexpat", "queue", "quopri", "random",
    "re", "readline", "reprlib", "resource", "rlcompleter", "runpy", "sched",
    "secrets", "select", "selectors", "shelve", "shlex", "shutil", "signal",
    "site", "smtpd", "smtplib", "sndhdr", "socket", "socketserver", "spwd",
    "sqlite3", "ssl", "stat", "statistics", "string", "stringprep", "struct",
    "subprocess", "sunau", "symbol", "symtable", "sys", "sysconfig", "syslog",
    "tabnanny", "tarfile", "telnetlib", "tempfile", "termios", "test",
    "textwrap", "threading", "time", "timeit", "tkinter", "token", "tokenize",
    "tomllib", "trace", "traceback", "tracemalloc", "tty", "turtle",
    "turtledemo", "types", "typing", "unicodedata", "unittest", "urllib",
    "uu", "uuid", "venv", "warnings", "wave", "weakref", "webbrowser",
    "winreg", "winsound", "wsgiref", "xdrlib", "xml", "xmlrpc", "zipapp",
    "zipfile", "zipimport", "zlib", "zoneinfo",
];

/// Node.js built-in modules. Kept sorted for `binary_search`. Sourced
/// from `node --version` 22 docs. Excludes `node:` prefix — sigil
/// strips that before emission.
const NODE_BUILTINS: &[&str] = &[
    "assert", "async_hooks", "buffer", "child_process", "cluster",
    "console", "constants", "crypto", "dgram", "diagnostics_channel",
    "dns", "domain", "events", "fs", "http", "http2", "https",
    "inspector", "module", "net", "os", "path", "perf_hooks", "process",
    "punycode", "querystring", "readline", "repl", "stream",
    "string_decoder", "sys", "test", "timers", "tls", "trace_events",
    "tty", "url", "util", "v8", "vm", "wasi", "worker_threads", "zlib",
];

/// Cross-repo external-symbol resolution (issue #30 MVP, polished in
/// issue #47).
///
/// Walks the focus repo's `.sigil/entities.jsonl` for `kind=="external"`
/// sentinels (these have `name = "external:<modpath>"` and
/// `file = "<external>"`). For each, scans every enabled workspace
/// member's `.sigil/entities.jsonl` for a non-external entity whose
/// `name` (or `qualified_name`) matches the modpath or its leaf
/// segment. Emit a `WorkspaceResolution` row per match.
///
/// Scope:
///   * Membership: scoped to enabled members in
///     `.sigil-workspace/members.json` (was `scan(workspace_root)`
///     until issue #47.3 — that broad scan produced bogus matches
///     against unrelated sibling repos).
///   * Stdlib filter: imports the language's standard library
///     (`is_stdlib_modpath`) are dropped before matching — those aren't
///     real cross-repo bindings.
///   * Constraint shape: NO package-deps constraint yet — every member
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

    // Collect external modpaths from the focus repo. Stdlib imports
    // (Go `context` / `net/url`, Python `os`, Node `fs`, Rust `std::*`)
    // are filtered here — they're not real cross-repo bindings, and
    // matching them against unrelated entities with colliding leaf
    // names produces noise (issue #47.4).
    let mut wanted: Vec<String> = Vec::new();
    for line in focus_text.lines() {
        let Ok(e): Result<Value, _> = serde_json::from_str(line) else { continue };
        if e.get("kind").and_then(Value::as_str) != Some("external") {
            continue;
        }
        let Some(name) = e.get("name").and_then(Value::as_str) else { continue };
        if let Some(modpath) = name.strip_prefix("external:") {
            if is_stdlib_modpath(modpath) {
                continue;
            }
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

    // Scope to enabled members (issue #47.3). Previously `scan()` walked
    // every `.git/`-bearing sibling, producing bogus matches against
    // unrelated repos in the workspace's parent dir. Resolution now
    // respects the explicit membership in `.sigil-workspace/members.json`.
    let members: Vec<WorkspaceMember> = match list(workspace_root) {
        Ok(m) => m.into_iter().filter(|x| !x.disabled).collect(),
        Err(_) => return out,
    };

    for sibling in members {
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
                        provider_repo: sibling.name.clone(),
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

/// Persist a batch of symbol-level resolutions tagged with their
/// consumer repo into the workspace's `cross_repo_refs.jsonl` (issue
/// #47.5). Module-level rows already in the file (`kind != "cross_repo_symbol"`)
/// are preserved; existing `cross_repo_symbol` rows are replaced
/// wholesale so `workspace resolve` is idempotent across runs.
///
/// Rows are written in the overloaded `Reference` shape decided during
/// grilling: `caller=external_modpath, name=provider_symbol,
/// kind=cross_repo_symbol, callee_id=<provider_repo>/<file>::<symbol>`.
/// The final on-disk file is fully lexicographically sorted (preserved
/// module-level rows merged with new symbol rows, then a single sort
/// pass) — matches CLAUDE.md's deterministic-diff rule.
pub fn persist_resolutions_grouped(
    workspace_root: &Path,
    tagged: &[(String, WorkspaceResolution)],
) -> Result<()> {
    use std::collections::BTreeMap;

    let path = workspace_root.join(".sigil-workspace").join("cross_repo_refs.jsonl");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let mut keep: Vec<String> = Vec::new();
    if path.exists()
        && let Ok(existing) = std::fs::read_to_string(&path)
    {
        for line in existing.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let kind = serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| v.get("kind").and_then(|s| s.as_str()).map(str::to_string));
            if kind.as_deref() != Some("cross_repo_symbol") {
                keep.push(line.to_string());
            }
        }
    }

    // BTreeMap keyed by (consumer_repo, caller, callee_id) for
    // deterministic output without an explicit sort.
    let mut new_rows: BTreeMap<(String, String, String), serde_json::Value> = BTreeMap::new();
    for (consumer_repo, r) in tagged {
        let callee_id =
            format!("{}/{}::{}", r.provider_repo, r.provider_file, r.provider_symbol);
        let row = serde_json::json!({
            "file": format!("{consumer_repo}/<external>"),
            "caller": r.external_modpath,
            "name": r.provider_symbol,
            "kind": "cross_repo_symbol",
            "line": 0,
            "confidence": r.confidence,
            "callee_id": callee_id,
        });
        new_rows.insert((consumer_repo.clone(), r.external_modpath.clone(), callee_id), row);
    }

    // Merge preserved module-level rows + new symbol rows, sort the
    // entire output as one sequence. CLAUDE.md: "Output JSONL artifacts
    // are lexicographically sorted for deterministic diffs."
    let mut all_lines: Vec<String> = keep;
    all_lines.extend(new_rows.into_values().map(|row| row.to_string()));
    all_lines.sort();
    let body = if all_lines.is_empty() {
        String::new()
    } else {
        let mut s = all_lines.join("\n");
        s.push('\n');
        s
    };
    std::fs::write(&path, body)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Helper for callers (e.g. multi-repo build scripts) that want each repo
/// path resolved to an absolute PathBuf.
pub fn paths(parent: &Path) -> Vec<PathBuf> {
    scan(parent)
        .into_iter()
        .map(|r| PathBuf::from(r.path))
        .collect()
}

#[cfg(test)]
mod rank_loader_tests {
    use super::*;
    use crate::rank::RankManifest;
    use std::collections::BTreeMap;

    fn write_member_rank(member_root: &Path, files: &[(&str, f64)]) {
        std::fs::create_dir_all(member_root.join(".sigil")).unwrap();
        let mut file_rank = BTreeMap::new();
        for (f, r) in files {
            file_rank.insert(f.to_string(), *r);
        }
        let m = RankManifest {
            version: "1".to_string(),
            sigil_version: "test".to_string(),
            damping: 0.85,
            iterations_max: 50,
            transitive_depth: 3,
            file_count: file_rank.len(),
            file_rank,
        };
        std::fs::write(
            member_root.join(".sigil/rank.json"),
            serde_json::to_string(&m).unwrap(),
        )
        .unwrap();
    }

    fn write_members_json_at(workspace_root: &Path, members: &[(&str, &Path)]) {
        let ws_dir = workspace_root.join(".sigil-workspace");
        std::fs::create_dir_all(&ws_dir).unwrap();
        let arr: Vec<_> = members
            .iter()
            .map(|(name, path)| {
                serde_json::json!({
                    "name": name,
                    "path": path.to_string_lossy(),
                    "added_at": "2026-05-14T00:00:00Z",
                })
            })
            .collect();
        let body = serde_json::json!({"version": 1, "members": arr});
        std::fs::write(ws_dir.join("members.json"), body.to_string()).unwrap();
    }

    #[test]
    fn load_rank_manifest_prefers_workspace_level_rank_json() {
        // When `.sigil-workspace/rank.json` exists, the loader reads it
        // directly (globally-correct PageRank). Per-member rank.json is
        // ignored — the union-graph scores include cross-repo edges
        // that per-member ranking can't see.
        let tmp = std::env::temp_dir().join(format!("sigil_ws_rank_prefer_ut_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let ws_dir = tmp.join(".sigil-workspace");
        std::fs::create_dir_all(&ws_dir).unwrap();
        let mut file_rank = BTreeMap::new();
        file_rank.insert("repo-a/src/lib.rs".to_string(), 0.7);
        file_rank.insert("repo-b/src/main.rs".to_string(), 0.3);
        let m = RankManifest {
            version: "1".to_string(),
            sigil_version: "test".to_string(),
            damping: 0.85,
            iterations_max: 50,
            transitive_depth: 3,
            file_count: 2,
            file_rank,
        };
        std::fs::write(
            ws_dir.join("rank.json"),
            serde_json::to_string(&m).unwrap(),
        )
        .unwrap();

        let loaded = load_rank_manifest(&tmp);
        assert!(
            (loaded.file_rank["repo-a/src/lib.rs"] - 0.7).abs() < 1e-9,
            "workspace-level rank.json must be preferred",
        );
        assert_eq!(loaded.file_rank.len(), 2);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn load_rank_manifest_falls_back_to_per_member_merge_with_prefixes() {
        // No workspace-level rank.json — loader merges each member's
        // .sigil/rank.json, prefixing file keys with the member name to
        // match `Index::load_workspace`'s union-load naming.
        let tmp = std::env::temp_dir().join(format!("sigil_ws_rank_merge_ut_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let alpha = tmp.join("alpha");
        let beta = tmp.join("beta");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        write_member_rank(&alpha, &[("src/lib.rs", 0.6)]);
        write_member_rank(&beta, &[("src/lib.rs", 0.4)]);
        write_members_json_at(&tmp, &[("alpha", &alpha), ("beta", &beta)]);

        let merged = load_rank_manifest(&tmp);
        assert!(
            (merged.file_rank["alpha/src/lib.rs"] - 0.6).abs() < 1e-9,
            "alpha rank entry must be prefixed",
        );
        assert!(
            (merged.file_rank["beta/src/lib.rs"] - 0.4).abs() < 1e-9,
            "beta rank entry must be prefixed",
        );
        std::fs::remove_dir_all(&tmp).ok();
    }
}
