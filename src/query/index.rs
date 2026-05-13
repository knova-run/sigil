//! In-house query index for sigil.
//!
//! The index is built from `.sigil/entities.jsonl` + `.sigil/refs.jsonl` (sigil's
//! on-disk source of truth). It lives in memory and exposes the lookups that
//! `sigil callers / callees / symbols / children / search / explore` need.
//!
//! Fine up to ~500k entities. Above the `SIGIL_AUTO_ENGAGE_THRESHOLD_MB` size
//! the DuckDB backend slots in behind the same public API.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};

use crate::entity::{Entity, Reference};

/// What a `search()` invocation should look at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Everything the index knows about (symbols + files). Text blocks
    /// are omitted — sigil doesn't index docstrings/comments today.
    All,
    /// Match only against entity names.
    Symbols,
    /// Match only against file paths.
    Files,
}

impl Scope {
    /// Parse codeix-compatible scope strings so main.rs's `--scope` flag
    /// keeps working across the day-6 swap. Deliberately infallible —
    /// unknown values fall back to `Scope::All` rather than erroring, matching
    /// codeix's behavior. Named `parse` (not `from_str`) to avoid the
    /// `std::str::FromStr` trait collision clippy flags.
    pub fn parse(s: &str) -> Self {
        match s {
            "symbols" | "symbol" => Scope::Symbols,
            "files" | "file" => Scope::Files,
            _ => Scope::All,
        }
    }
}

/// A single search hit. References into the index — lifetime-bound.
#[derive(Debug)]
pub enum SearchHit<'a> {
    Symbol(&'a Entity),
    File(FileHit),
}

/// A file match from `search()` or `explore_*()`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FileHit {
    pub path: String,
    pub lang: Option<String>,
    pub entity_count: usize,
}

/// One row of `explore_dir_overview()`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DirSummary {
    pub path: String,
    pub file_count: usize,
    pub langs: Vec<String>,
}

/// Directory portion of `file`, or "" for the root.
fn parent_dir(file: &str) -> &str {
    match file.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

/// Language name for a file, if tree-sitter (or sigil's custom parsers) handle it.
fn lang_for(file: &str) -> Option<&'static str> {
    let ext = file.rsplit_once('.').map(|(_, e)| e)?;
    if let Some(lang) = crate::parser::languages::detect_language(ext) {
        return Some(lang);
    }
    // Sigil's custom parsers beyond codeix's coverage.
    match ext {
        "json" => Some("json"),
        "yaml" | "yml" => Some("yaml"),
        "toml" => Some("toml"),
        _ => None,
    }
}

/// The tail segment of a `::`-qualified name, or `None` if the name has
/// no `::` (i.e., is already bare). Used so qualified ref names are
/// looked up under both the full path and the unqualified tail.
/// Enumerate each meaningful class prefix of a caller string, for use
/// in `refs_by_caller` indexing. For `Sinatra::Base.foo` returns
/// [`Sinatra::Base`, `Base`, `Sinatra`] — so `callees Base` reaches
/// the method's outgoing calls even though the caller is stored as
/// `Sinatra::Base.foo`. At each prefix step we ALSO emit the
/// qualified tail of that prefix, mirroring the qualified-tail
/// indexing on the callee side. Skips the full caller (already
/// indexed) and empty strings.
fn caller_prefixes(caller: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = caller.to_string();
    loop {
        // Pick the split that yields the LONGER head — corresponds to
        // the latest separator in the string. Handles mixed
        // `Sinatra::Base.foo` cases correctly.
        let by_dot = cur.rsplit_once('.').map(|(h, _)| h.to_string());
        let by_cc = cur.rsplit_once("::").map(|(h, _)| h.to_string());
        let next = match (by_dot, by_cc) {
            (Some(d), Some(c)) => if d.len() > c.len() { d } else { c },
            (Some(d), None) => d,
            (None, Some(c)) => c,
            _ => break,
        };
        if next.is_empty() || next == cur {
            break;
        }
        out.push(next.clone());
        // Also emit the bare leaf segment of this prefix so a lookup
        // by the inner class name (e.g. `Base`) reaches refs whose
        // caller has an outer namespace (e.g. `Sinatra::Base.foo` OR
        // the dot-normalized `Rack.Protection.Base.call`). The leaf
        // is whichever separator (`.` or `::`) comes latest in the
        // string.
        if let Some(leaf) = bare_leaf(&next) {
            if leaf != next {
                out.push(leaf.to_string());
            }
        }
        cur = next;
    }
    out
}

fn qualified_tail(name: &str) -> Option<&str> {
    let tail = name.rsplit("::").next().unwrap_or(name);
    if tail.len() == name.len() {
        None
    } else {
        Some(tail)
    }
}

/// Immediate `::` head of a Rust-style FQN ref name, plus the bare
/// leaf of that head when the leaf looks like a type name (starts with
/// an uppercase letter). Used so `callers Regex` reaches refs stored
/// as `Regex::new`, and `callers Bar` reaches `Foo::Bar::baz`.
///
/// We restrict to ONE step back (not the full chain) and gate the leaf
/// emission on case so we don't pollute lookups for module-root names:
///  * `Regex::new`                → `["Regex"]`
///  * `Foo::Bar::baz`             → `["Foo::Bar", "Bar"]`
///  * `crate::parser::parse_file` → `["crate::parser"]` (no `parser` —
///    lowercase module names would collide with legitimate workspace
///    symbols of the same name; tested by `refs_to_does_not_pollute_*`)
///  * `std::collections::HashMap` → `["std::collections"]`
///
/// We restrict to `::` (not `.`) because dot-qualified refs like
/// `requests.Session` should NOT surface under `callers requests` —
/// that's module-namespace bookkeeping, not a meaningful symbol use.
fn cc_head_prefixes(name: &str) -> Vec<String> {
    head_prefixes_with_sep(name, "::")
}

/// Generalised head-prefix emitter for either `::` or `.` separator.
/// For `<head><sep><tail>` emits the immediate head plus its bare_leaf
/// (gated on uppercase first-char). Multi-segment heads are always
/// emitted; single-segment heads only when type-name-shaped. This is
/// what lets `callers Regex` reach `Regex::new` AND `callers Connection`
/// reach `Faraday::Connection.new` without surfacing module-namespace
/// noise like `callers crate` / `callers obj`.
pub(crate) fn head_prefixes_with_sep(name: &str, sep: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Some((head, _)) = name.rsplit_once(sep) else {
        return out;
    };
    if head.is_empty() {
        return out;
    }
    let head_has_multi = head.contains("::") || head.contains('.');
    let head_is_typey = head.starts_with(|c: char| c.is_ascii_uppercase());
    if head_has_multi || head_is_typey {
        out.push(head.to_string());
    }
    if let Some(leaf) = bare_leaf(head) {
        if leaf != head && leaf.starts_with(|c: char| c.is_ascii_uppercase()) {
            out.push(leaf.to_string());
        }
    }
    out
}

/// Bare-leaf segment after the LATEST `.` or `::` separator. Handles
/// both Ruby-style `Rack.Protection.Base` (dot-normalized namespaces)
/// and Rust/Ruby `Sinatra::Base`. Returns None when `name` has no
/// separator (i.e., is already bare).
pub(crate) fn bare_leaf(name: &str) -> Option<&str> {
    let dot_pos = name.rfind('.');
    let cc_pos = name.rfind("::");
    let cut = match (dot_pos, cc_pos) {
        (Some(d), Some(c)) => Some(if d > c + 1 { d + 1 } else { c + 2 }),
        (Some(d), None) => Some(d + 1),
        (None, Some(c)) => Some(c + 2),
        _ => None,
    }?;
    Some(&name[cut..])
}

/// In-memory index over sigil's entities and references.
///
/// Lookup complexity: O(1) for exact-name/exact-file lookups via the maps;
/// O(n) for substring search over entity names (still fast at <1M entities).
#[derive(Debug, Default)]
pub struct Index {
    pub entities: Vec<Entity>,
    pub references: Vec<Reference>,

    // Precomputed maps built during `build()`. Indices point into the vecs
    // above. Using `Vec<usize>` rather than `SmallVec` for now — easy to swap
    // later if a profile shows it matters.
    entities_by_name: HashMap<String, Vec<usize>>,
    entities_by_file: HashMap<String, Vec<usize>>,
    refs_by_name: HashMap<String, Vec<usize>>,     // target name → ref idxs (callers)
    refs_by_caller: HashMap<String, Vec<usize>>,   // caller → ref idxs (callees)
    refs_by_file: HashMap<String, Vec<usize>>,
}

impl Index {
    /// Build an index from already-in-memory entities + references. Takes
    /// ownership so we can move the vecs in rather than copying ~100 MB of
    /// data at large scale.
    pub fn build(entities: Vec<Entity>, references: Vec<Reference>) -> Self {
        let mut entities_by_name: HashMap<String, Vec<usize>> = HashMap::new();
        let mut entities_by_file: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, e) in entities.iter().enumerate() {
            entities_by_name.entry(e.name.clone()).or_default().push(i);
            // Also index entities under the trailing segment of their
            // qualified name so a lookup like `context Base` matches an
            // entity stored as `Sinatra.Base` (Ruby/Kotlin/Scala emit the
            // qualified form). Uses `bare_leaf` so both `::` and `.`
            // separators are recognised — Python emits `requests.Session`,
            // Rust emits `crate::sessions::Session`. Mirrors
            // `refs_by_name`'s leaf indexing for callee lookups.
            if let Some(leaf) = bare_leaf(&e.name) {
                if leaf != e.name {
                    entities_by_name.entry(leaf.to_string()).or_default().push(i);
                }
            }
            entities_by_file.entry(e.file.clone()).or_default().push(i);
        }

        let mut refs_by_name: HashMap<String, Vec<usize>> = HashMap::new();
        let mut refs_by_caller: HashMap<String, Vec<usize>> = HashMap::new();
        let mut refs_by_file: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, r) in references.iter().enumerate() {
            refs_by_name.entry(r.name.clone()).or_default().push(i);
            // Also index qualified names under their trailing segment so
            // that `callers parse_file` matches refs whose stored name is
            // `crate::parser::treesitter::parse_file` (Rust FQN), AND
            // `callers Session` matches `requests.Session` (Python
            // dot-qualified member-call refs from the tier-2 alias
            // resolver). `bare_leaf` cuts on whichever separator (`.`
            // or `::`) is latest in the name.
            if let Some(leaf) = bare_leaf(&r.name) {
                refs_by_name.entry(leaf.to_string()).or_default().push(i);
            }
            // Also index qualified refs under their immediate head prefix
            // (with uppercase gating to avoid pollution). Two passes:
            //  * `::` head — lets `callers Regex` reach `Regex::new`,
            //    `callers Foo::Bar` reach `Foo::Bar::baz`.
            //  * `.` head — lets `callers Connection` reach Ruby refs
            //    of form `Faraday::Connection.new` (where bare_leaf is
            //    `new` and the cc-head is just `Faraday`). Uppercase
            //    gating means `requests.Session` does NOT surface under
            //    `callers requests`.
            if r.name.contains("::") {
                for head in head_prefixes_with_sep(&r.name, "::") {
                    refs_by_name.entry(head).or_default().push(i);
                }
            }
            if r.name.contains('.') {
                for head in head_prefixes_with_sep(&r.name, ".") {
                    refs_by_name.entry(head).or_default().push(i);
                }
            }
            if let Some(caller) = &r.caller {
                refs_by_caller.entry(caller.clone()).or_default().push(i);
                // Also index under each progressive class prefix so
                // `callees Moshi` returns refs whose caller is
                // `Moshi.foo`, AND `callees Base` returns refs whose
                // caller is `Sinatra::Base.foo`. Two-level fan-out is
                // needed because mixed `::` + `.` separators (Ruby
                // emits `Sinatra::Base.method`) carry two split points.
                // Symmetric with `refs_by_name`'s qualified-tail
                // indexing on the callee side.
                for head in caller_prefixes(caller) {
                    refs_by_caller.entry(head).or_default().push(i);
                }
            }
            refs_by_file.entry(r.file.clone()).or_default().push(i);
        }

        Index {
            entities,
            references,
            entities_by_name,
            entities_by_file,
            refs_by_name,
            refs_by_caller,
            refs_by_file,
        }
    }

    /// Load from `.sigil/entities.jsonl` + `.sigil/refs.jsonl` under the given
    /// project root. Missing files are treated as empty.
    pub fn load(root: &Path) -> Result<Self> {
        let sigil_dir = root.join(".sigil");
        let entities = read_jsonl::<Entity>(&sigil_dir.join("entities.jsonl"))
            .context("failed to load entities.jsonl")?;
        let references = read_jsonl::<Reference>(&sigil_dir.join("refs.jsonl"))
            .context("failed to load refs.jsonl")?;
        Ok(Self::build(entities, references))
    }

    /// Total counts — useful for stats output and for the Phase 0.5 DuckDB
    /// auto-upgrade heuristic.
    pub fn len(&self) -> (usize, usize) {
        (self.entities.len(), self.references.len())
    }

    pub fn is_empty(&self) -> bool {
        self.entities.is_empty() && self.references.is_empty()
    }

    /// Entities defined with this exact name. Multiple hits for ambiguous
    /// symbols (e.g., two modules each defining `Config`).
    pub fn entities_by_name(&self, name: &str) -> impl Iterator<Item = &Entity> {
        self.entities_by_name
            .get(name)
            .map(|idxs| idxs.as_slice())
            .unwrap_or(&[])
            .iter()
            .map(move |&i| &self.entities[i])
    }

    /// All entities in a file.
    pub fn entities_by_file(&self, file: &str) -> impl Iterator<Item = &Entity> {
        self.entities_by_file
            .get(file)
            .map(|idxs| idxs.as_slice())
            .unwrap_or(&[])
            .iter()
            .map(move |&i| &self.entities[i])
    }

    /// References whose *target* is `name` — i.e., callers of `name`.
    pub fn refs_to(&self, name: &str) -> impl Iterator<Item = &Reference> {
        self.refs_by_name
            .get(name)
            .map(|idxs| idxs.as_slice())
            .unwrap_or(&[])
            .iter()
            .map(move |&i| &self.references[i])
    }

    /// References whose *caller* is `caller` — i.e., what `caller` calls.
    pub fn refs_from(&self, caller: &str) -> impl Iterator<Item = &Reference> {
        self.refs_by_caller
            .get(caller)
            .map(|idxs| idxs.as_slice())
            .unwrap_or(&[])
            .iter()
            .map(move |&i| &self.references[i])
    }

    /// References defined in a file.
    pub fn refs_in_file(&self, file: &str) -> impl Iterator<Item = &Reference> {
        self.refs_by_file
            .get(file)
            .map(|idxs| idxs.as_slice())
            .unwrap_or(&[])
            .iter()
            .map(move |&i| &self.references[i])
    }

    // ──────────────────────────────────────────────────────────────────────
    // Public query API — mirrors codeix::SearchDb methods used by main.rs.
    // Return sigil's own `Entity` / `Reference` types; the day-6 switch in
    // main.rs swaps these in and deletes the codeix-backed functions in
    // src/query/mod.rs.
    //
    // `kind_filter`: exact-match filter on ref_kind (for refs) or kind (for
    // entities). None = no filter. Matches codeix's behavior.
    //
    // `limit`: 0 = unlimited. Positive = take at most `limit` results.
    // Results are returned in insertion order (which, for sigil's index, is
    // sorted by (file, line_start) per the project convention — so this
    // ordering is stable across runs).
    // ──────────────────────────────────────────────────────────────────────

    /// All references targeting `name`, optionally filtered by ref kind.
    pub fn get_callers(&self, name: &str, kind_filter: Option<&str>, limit: usize) -> Vec<&Reference> {
        let iter = self.refs_to(name).filter(|r| match kind_filter {
            Some(k) => r.ref_kind == k,
            None => true,
        });
        apply_limit(iter, limit)
    }

    /// All references made by `caller`, optionally filtered by ref kind.
    pub fn get_callees(&self, caller: &str, kind_filter: Option<&str>, limit: usize) -> Vec<&Reference> {
        let iter = self.refs_from(caller).filter(|r| match kind_filter {
            Some(k) => r.ref_kind == k,
            None => true,
        });
        apply_limit(iter, limit)
    }

    /// All entities defined in `file`, optionally filtered by entity kind.
    pub fn get_file_symbols(&self, file: &str, kind_filter: Option<&str>, limit: usize) -> Vec<&Entity> {
        let iter = self.entities_by_file(file).filter(|e| match kind_filter {
            Some(k) => e.kind == k,
            None => true,
        });
        apply_limit(iter, limit)
    }

    /// Unique file paths covered by this index, sorted.
    pub fn files(&self) -> Vec<String> {
        let mut files: Vec<String> = self.entities_by_file.keys().cloned().collect();
        files.sort();
        files
    }

    /// Directory overview: for each directory containing indexed files,
    /// return (file count, unique languages). Used by `sigil explore`.
    ///
    /// `path_prefix`: if Some, restrict to files under this prefix (matches
    /// the prefix semantics codeix's `explore_dir_overview` uses).
    pub fn explore_dir_overview(&self, path_prefix: Option<&str>) -> Vec<DirSummary> {
        let mut by_dir: std::collections::BTreeMap<String, (usize, std::collections::BTreeSet<String>)> =
            std::collections::BTreeMap::new();

        for file in self.entities_by_file.keys() {
            if let Some(prefix) = path_prefix
                && !file.starts_with(prefix)
            {
                continue;
            }
            let dir = parent_dir(file).to_string();
            let lang = lang_for(file);
            let entry = by_dir.entry(dir).or_default();
            entry.0 += 1;
            if let Some(l) = lang {
                entry.1.insert(l.to_string());
            }
        }

        by_dir
            .into_iter()
            .map(|(path, (file_count, langs))| DirSummary {
                path,
                file_count,
                langs: langs.into_iter().collect(),
            })
            .collect()
    }

    /// Flat file listing with directory + language, capped per directory.
    /// Matches the shape of codeix's `explore_files_capped`.
    pub fn explore_files_capped(
        &self,
        path_prefix: Option<&str>,
        cap_per_dir: usize,
    ) -> Vec<(String, String, Option<String>)> {
        let mut by_dir: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();

        let mut files: Vec<&String> = self.entities_by_file.keys().collect();
        files.sort();

        for file in files {
            if let Some(prefix) = path_prefix
                && !file.starts_with(prefix)
            {
                continue;
            }
            let dir = parent_dir(file).to_string();
            by_dir.entry(dir).or_default().push(file.clone());
        }

        let mut out = Vec::new();
        for (dir, mut entries) in by_dir {
            entries.sort();
            if cap_per_dir > 0 && entries.len() > cap_per_dir {
                entries.truncate(cap_per_dir);
            }
            for f in entries {
                let lang = lang_for(&f).map(|s| s.to_string());
                out.push((dir.clone(), f, lang));
            }
        }
        out
    }

    /// Search across symbols and/or files by substring (case-insensitive).
    /// Mirrors the shape codeix's `search()` returns, minus text-block hits
    /// (sigil doesn't index doc/comment text today).
    pub fn search(
        &self,
        query: &str,
        scope: Scope,
        kind_filter: Option<&str>,
        path_prefix: Option<&str>,
        limit: usize,
    ) -> Vec<SearchHit<'_>> {
        if query.is_empty() {
            return Vec::new();
        }
        let q = query.to_lowercase();
        let mut hits: Vec<SearchHit<'_>> = Vec::new();

        let want_symbols = matches!(scope, Scope::All | Scope::Symbols);
        let want_files = matches!(scope, Scope::All | Scope::Files);

        if want_symbols {
            for e in &self.entities {
                if let Some(prefix) = path_prefix
                    && !e.file.starts_with(prefix)
                {
                    continue;
                }
                if let Some(k) = kind_filter
                    && e.kind != k
                {
                    continue;
                }
                if e.name.to_lowercase().contains(&q) {
                    hits.push(SearchHit::Symbol(e));
                    if limit > 0 && hits.len() >= limit {
                        return hits;
                    }
                }
            }
        }

        if want_files {
            let mut files: Vec<&String> = self.entities_by_file.keys().collect();
            files.sort();
            for f in files {
                if let Some(prefix) = path_prefix
                    && !f.starts_with(prefix)
                {
                    continue;
                }
                if f.to_lowercase().contains(&q) {
                    hits.push(SearchHit::File(FileHit {
                        path: f.clone(),
                        lang: lang_for(f).map(|s| s.to_string()),
                        entity_count: self.entities_by_file.get(f).map(|v| v.len()).unwrap_or(0),
                    }));
                    if limit > 0 && hits.len() >= limit {
                        return hits;
                    }
                }
            }
        }

        hits
    }

    /// Single-project index — codeix's multi-project MountTable is gone.
    /// Returning a single empty-string entry matches codeix's convention
    /// ("" = the root project), which the existing `format_*` helpers
    /// already handle.
    pub fn list_projects(&self) -> Vec<String> {
        vec![String::new()]
    }

    /// All entities in `file` whose `parent` matches `parent`.
    pub fn get_children(
        &self,
        file: &str,
        parent: &str,
        kind_filter: Option<&str>,
        limit: usize,
    ) -> Vec<&Entity> {
        let iter = self.entities_by_file(file).filter(|e| {
            e.parent.as_deref() == Some(parent)
                && match kind_filter {
                    Some(k) => e.kind == k,
                    None => true,
                }
        });
        apply_limit(iter, limit)
    }
}

fn apply_limit<'a, T, I>(iter: I, limit: usize) -> Vec<&'a T>
where
    I: Iterator<Item = &'a T>,
{
    if limit == 0 {
        iter.collect()
    } else {
        iter.take(limit).collect()
    }
}

fn read_jsonl<T: for<'de> serde::Deserialize<'de>>(path: &Path) -> Result<Vec<T>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    let mut out = Vec::new();
    for (lineno, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let item: T = serde_json::from_str(line)
            .with_context(|| format!("{}:{}: parse JSON", path.display(), lineno + 1))?;
        out.push(item);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{Entity, Reference};

    fn ent(file: &str, name: &str, kind: &str) -> Entity {
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
            blast_radius: None,
            doc: None,
            heritage: Vec::new(),
            alias: None,        }
    }

    fn refr(file: &str, caller: Option<&str>, name: &str, kind: &str) -> Reference {
        Reference {
            file: file.to_string(),
            caller: caller.map(|c| c.to_string()),
            name: name.to_string(),
            ref_kind: kind.to_string(),
            line: 1,
            confidence: None,
            callee_id: None,
        }
    }

    #[test]
    fn empty_index_has_zero_counts() {
        let idx = Index::build(vec![], vec![]);
        assert_eq!(idx.len(), (0, 0));
        assert!(idx.is_empty());
    }

    #[test]
    fn entities_by_name_finds_all_matches() {
        let idx = Index::build(
            vec![
                ent("a.rs", "foo", "function"),
                ent("b.rs", "foo", "function"), // ambiguous — two files define foo
                ent("c.rs", "bar", "function"),
            ],
            vec![],
        );
        let foos: Vec<_> = idx.entities_by_name("foo").collect();
        assert_eq!(foos.len(), 2);
        let bars: Vec<_> = idx.entities_by_name("bar").collect();
        assert_eq!(bars.len(), 1);
        let missing: Vec<_> = idx.entities_by_name("nope").collect();
        assert_eq!(missing.len(), 0);
    }

    #[test]
    fn entities_by_file_groups_correctly() {
        let idx = Index::build(
            vec![
                ent("a.rs", "foo", "function"),
                ent("a.rs", "bar", "function"),
                ent("b.rs", "baz", "function"),
            ],
            vec![],
        );
        let in_a: Vec<_> = idx.entities_by_file("a.rs").collect();
        assert_eq!(in_a.len(), 2);
        let in_b: Vec<_> = idx.entities_by_file("b.rs").collect();
        assert_eq!(in_b.len(), 1);
    }

    #[test]
    fn refs_to_returns_callers() {
        let idx = Index::build(
            vec![ent("a.rs", "foo", "function")],
            vec![
                refr("b.rs", Some("main"), "foo", "call"),
                refr("c.rs", Some("helper"), "foo", "call"),
                refr("d.rs", Some("main"), "other", "call"), // should not match
            ],
        );
        let callers: Vec<_> = idx.refs_to("foo").collect();
        assert_eq!(callers.len(), 2);
        let callers_other: Vec<_> = idx.refs_to("other").collect();
        assert_eq!(callers_other.len(), 1);
    }

    #[test]
    fn refs_to_matches_qualified_tail() {
        // Refs stored under `crate::a::b::foo` must surface when the caller
        // searches for the bare name `foo`. Regression for the sigil-self
        // finding where `parse_file(...)` called as `crate::parser::
        // treesitter::parse_file(...)` from src/index.rs was missed.
        let idx = Index::build(
            vec![ent("a.rs", "foo", "function")],
            vec![
                refr("b.rs", Some("main"), "foo", "call"),
                refr("c.rs", Some("caller"), "crate::a::b::foo", "call"),
                refr("d.rs", Some("caller"), "Foo::foo", "call"),
                refr("e.rs", Some("caller"), "bar", "call"),    // must not match
                refr("f.rs", Some("caller"), "foobar", "call"), // must not match (no `::` boundary)
            ],
        );
        let bare: Vec<_> = idx.refs_to("foo").collect();
        assert_eq!(bare.len(), 3, "bare `foo` matches plain + both qualified refs");
        // Full qualified lookup still works as exact match.
        let qualified: Vec<_> = idx.refs_to("crate::a::b::foo").collect();
        assert_eq!(qualified.len(), 1);
        // Bare-name miss stays miss.
        let miss: Vec<_> = idx.refs_to("baz").collect();
        assert!(miss.is_empty());
    }

    #[test]
    fn refs_from_returns_callees() {
        let idx = Index::build(
            vec![],
            vec![
                refr("a.rs", Some("main"), "foo", "call"),
                refr("a.rs", Some("main"), "bar", "call"),
                refr("a.rs", Some("helper"), "foo", "call"),
            ],
        );
        let from_main: Vec<_> = idx.refs_from("main").collect();
        assert_eq!(from_main.len(), 2);
        let from_helper: Vec<_> = idx.refs_from("helper").collect();
        assert_eq!(from_helper.len(), 1);
    }

    #[test]
    fn refs_to_matches_dot_head_for_mixed_separator_names() {
        // Ruby refs like `Faraday::Connection.new` are stored with
        // mixed `::` + `.`. bare_leaf gives `new` (latest separator
        // wins); cc_head_prefixes walks `::` and emits `Faraday`.
        // But the meaningful symbol — `Connection` — is hidden inside
        // the part before the latest `.`. Add a `.`-head pass that
        // mirrors cc_head_prefixes: emit the head before the latest
        // `.` (when multi-segment), plus its bare_leaf when uppercase.
        // Regression: bug-fixes branch audit, faraday's `callers
        // Connection` returned 0.
        let idx = Index::build(
            vec![],
            vec![
                refr("a.rb", Some("c1"), "Faraday::Connection.new", "call"),
                refr("b.rb", Some("c2"), "Faraday::Connection.new", "call"),
                // Lowercase-leaf dot-head should NOT match.
                refr("c.rb", Some("c3"), "obj.helper", "call"),
                refr("d.rb", Some("c4"), "Foo.bar.baz", "call"),
            ],
        );
        assert_eq!(
            idx.refs_to("Connection").count(),
            2,
            "callers of `Connection` should reach `Faraday::Connection.new` refs"
        );
        assert_eq!(
            idx.refs_to("Foo.bar").count(),
            1,
            "multi-segment dot head emitted as-is"
        );
        assert_eq!(
            idx.refs_to("bar").count(),
            0,
            "lowercase leaf of dot head not emitted"
        );
        assert_eq!(
            idx.refs_to("obj").count(),
            0,
            "single-segment lowercase dot head not emitted"
        );
    }

    #[test]
    fn refs_to_does_not_pollute_under_module_prefix() {
        // `cc_head_prefixes` should reach `Regex` from `Regex::new` (a
        // type), but NOT pollute `callers parser` / `callers std` /
        // `callers crate` with every Rust scoped path. The heuristic:
        // only walk one `::` head back, and only emit its bare leaf if
        // the leaf starts with uppercase (i.e. looks like a type, not
        // a module). Regression from PR #23 review.
        let idx = Index::build(
            vec![],
            vec![
                refr("a.rs", Some("f"), "std::collections::HashMap", "import"),
                refr("b.rs", Some("g"), "crate::parser::parse_file", "call"),
                refr("c.rs", Some("h"), "Foo::Bar::baz", "call"),  // nested type
                refr("d.rs", Some("i"), "Regex::new", "call"),     // 2-segment type::method
            ],
        );
        // Lowercase module-root names: MUST NOT match.
        assert_eq!(
            idx.refs_to("std").count(),
            0,
            "module root `std` should not surface refs under `callers std`"
        );
        assert_eq!(
            idx.refs_to("crate").count(),
            0,
            "Rust path keyword `crate` should not surface refs"
        );
        assert_eq!(
            idx.refs_to("parser").count(),
            0,
            "lowercase module name `parser` should not surface refs (would pollute when a `parser` module exists)"
        );
        assert_eq!(
            idx.refs_to("collections").count(),
            0,
            "lowercase module segment `collections` should not surface refs"
        );
        // Also test single-segment lowercase head: `crate::Foo` should
        // NOT make `callers crate` return refs.
        let idx2 = Index::build(
            vec![],
            vec![refr("e.rs", Some("j"), "crate::Foo", "call")],
        );
        assert_eq!(
            idx2.refs_to("crate").count(),
            0,
            "`crate::Foo`'s head `crate` is single-segment lowercase — skip"
        );
        assert_eq!(
            idx2.refs_to("Foo").count(),
            1,
            "`crate::Foo`'s head `Foo` (uppercase) — should match"
        );
        // Type-name heads still work.
        assert_eq!(
            idx.refs_to("Regex").count(),
            1,
            "type head `Regex` from `Regex::new` should surface"
        );
        assert_eq!(
            idx.refs_to("Bar").count(),
            1,
            "inner type leaf `Bar` from `Foo::Bar::baz` should surface"
        );
    }

    #[test]
    fn refs_to_matches_cc_qualified_head_for_associated_calls() {
        // Rust `Regex::new()` constructor calls are stored as refs with
        // name=`Regex::new`. `sigil callers Regex` should reach those
        // refs — calling the constructor IS a use of the `Regex`
        // type. Leaf-only indexing (which buys us `callers new`) is
        // not enough.
        let idx = Index::build(
            vec![ent("regex.rs", "Regex", "struct")],
            vec![
                refr("a.rs", Some("f1"), "Regex::new", "call"),
                refr("b.rs", Some("f2"), "Regex::new", "call"),
                refr("c.rs", Some("f3"), "Regex::is_match", "call"),
                refr("d.rs", Some("f4"), "OtherType::new", "call"), // must NOT match Regex
            ],
        );
        let to_regex: Vec<_> = idx.refs_to("Regex").collect();
        assert_eq!(
            to_regex.len(),
            3,
            "callers of `Regex` should reach `Regex::new` + `Regex::is_match` refs; got {:?}",
            to_regex.iter().map(|r| (&r.file, &r.name)).collect::<Vec<_>>(),
        );
        // Leaf indexing still works for the method name itself.
        let to_new: Vec<_> = idx.refs_to("new").collect();
        assert_eq!(to_new.len(), 3, "leaf `new` matches both Regex::new entries plus OtherType::new");
    }

    #[test]
    fn refs_to_matches_dot_qualified_callee_leaf() {
        // Python tier-2 alias resolution stores member calls like
        // `requests.Session(...)` as a ref with name=`requests.Session`.
        // `sigil callers Session` should reach those refs — the leaf
        // segment after the `.` IS the symbol the user is asking
        // about. Currently `qualified_tail` only splits `::`, so this
        // is a real gap surfaced by the requests-repo audit.
        let idx = Index::build(
            vec![ent("a.py", "Session", "class")],
            vec![
                refr("b.py", Some("main"), "Session", "call"),
                refr("c.py", Some("api"), "requests.Session", "call"),
                refr("d.py", Some("helper"), "sessions.Session", "call"),
                refr("e.py", Some("test"), "SessionRedirectMixin", "call"), // must NOT match
            ],
        );
        let to_session: Vec<_> = idx.refs_to("Session").collect();
        assert_eq!(
            to_session.len(),
            3,
            "callers of `Session` should reach bare + both dot-qualified refs; got {:?}",
            to_session.iter().map(|r| (&r.file, &r.name)).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn refs_from_matches_method_scoped_caller_via_mixed_separator_prefixes() {
        // Ruby emits caller=`Sinatra::Base.foo` (mixed `::` + `.`).
        // `callees Base` should reach those refs because `Base` is the
        // qualified tail of `Sinatra::Base`, which is itself the class
        // prefix of `Sinatra::Base.foo`.
        let idx = Index::build(
            vec![],
            vec![
                refr("a.rb", Some("Sinatra::Base.foo"), "log", "call"),
                refr("a.rb", Some("Sinatra::Base.bar"), "puts", "call"),
            ],
        );
        let from_base: Vec<_> = idx.refs_from("Base").collect();
        assert_eq!(
            from_base.len(),
            2,
            "callees of class `Base` should reach refs whose caller is `Sinatra::Base.<method>`; got {:?}",
            from_base.iter().map(|r| (&r.caller, &r.name)).collect::<Vec<_>>(),
        );
        let from_qualified: Vec<_> = idx.refs_from("Sinatra::Base").collect();
        assert_eq!(from_qualified.len(), 2, "intermediate prefix also indexed");
    }

    #[test]
    fn refs_from_matches_nested_dot_only_caller_via_inner_class_leaf() {
        // Ruby parser normalizes `::` namespace separators to `.`, so a
        // class body method like `Rack::Protection::Base#call` is stored
        // with caller `Rack.Protection.Base.call`. Looking up `callees
        // Base` (the bare class name) MUST reach this ref — the inner
        // class is what the user wants to navigate by, even though
        // there is no `::` separator anywhere in the caller string.
        let idx = Index::build(
            vec![],
            vec![
                refr("a.rb", Some("Rack.Protection.Base.call"), "puts", "call"),
                refr("a.rb", Some("Rack.Protection.Base.debug"), "log", "call"),
            ],
        );
        let from_base: Vec<_> = idx.refs_from("Base").collect();
        assert_eq!(
            from_base.len(),
            2,
            "callees of class `Base` should reach refs whose caller is `Rack.Protection.Base.<method>`; got {:?}",
            from_base.iter().map(|r| (&r.caller, &r.name)).collect::<Vec<_>>(),
        );
        let from_qualified: Vec<_> = idx.refs_from("Rack.Protection.Base").collect();
        assert_eq!(from_qualified.len(), 2, "intermediate prefix also indexed");
    }

    #[test]
    fn refs_from_matches_method_scoped_caller_via_class_prefix() {
        // A class with a method that calls something — looking up
        // callees of the class itself should find the method's calls.
        // Mirrors what `sigil callees Moshi` should return for refs
        // whose caller is "Moshi.foo" or "Moshi::foo".
        let idx = Index::build(
            vec![],
            vec![
                refr("a.kt", Some("Moshi.foo"), "println", "call"),
                refr("a.kt", Some("Moshi.bar"), "log", "call"),
                refr("a.kt", Some("Other.baz"), "noise", "call"),
            ],
        );
        let from_class: Vec<_> = idx.refs_from("Moshi").collect();
        assert_eq!(
            from_class.len(),
            2,
            "callees of class `Moshi` should include refs from its methods; got {:?}",
            from_class.iter().map(|r| (&r.caller, &r.name)).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn refs_with_no_caller_skipped_in_refs_from() {
        // Top-level refs (no enclosing caller) must not appear in refs_from.
        let idx = Index::build(
            vec![],
            vec![
                refr("a.rs", None, "foo", "import"),
                refr("a.rs", Some("main"), "foo", "call"),
            ],
        );
        let from_main: Vec<_> = idx.refs_from("main").collect();
        assert_eq!(from_main.len(), 1);
        // Top-level ref is still findable via refs_to
        let to_foo: Vec<_> = idx.refs_to("foo").collect();
        assert_eq!(to_foo.len(), 2);
    }

    #[test]
    fn refs_in_file_groups_by_file() {
        let idx = Index::build(
            vec![],
            vec![
                refr("a.rs", Some("m"), "x", "call"),
                refr("a.rs", Some("m"), "y", "call"),
                refr("b.rs", Some("m"), "z", "call"),
            ],
        );
        let in_a: Vec<_> = idx.refs_in_file("a.rs").collect();
        assert_eq!(in_a.len(), 2);
    }

    #[test]
    fn load_missing_dir_returns_empty_index() {
        let tmp = std::env::temp_dir().join(format!("sigil_query_empty_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let idx = Index::load(&tmp).expect("missing jsonl should load as empty");
        assert!(idx.is_empty());
        std::fs::remove_dir_all(&tmp).ok();
    }

    // ──────────────────────────────────────────────────────────────────
    // Day-4 public API: get_callers / get_callees / get_file_symbols /
    // get_children — kind filter + limit semantics.
    // ──────────────────────────────────────────────────────────────────

    #[test]
    fn get_callers_filters_by_kind() {
        let idx = Index::build(
            vec![],
            vec![
                refr("a.rs", Some("m"), "foo", "call"),
                refr("b.rs", Some("m"), "foo", "import"),
                refr("c.rs", Some("m"), "foo", "call"),
            ],
        );
        assert_eq!(idx.get_callers("foo", None, 0).len(), 3);
        assert_eq!(idx.get_callers("foo", Some("call"), 0).len(), 2);
        assert_eq!(idx.get_callers("foo", Some("import"), 0).len(), 1);
        assert_eq!(idx.get_callers("foo", Some("nope"), 0).len(), 0);
    }

    #[test]
    fn get_callers_respects_limit() {
        let idx = Index::build(
            vec![],
            (0..10)
                .map(|i| refr(&format!("f{i}.rs"), Some("m"), "foo", "call"))
                .collect(),
        );
        assert_eq!(idx.get_callers("foo", None, 0).len(), 10, "limit 0 = unlimited");
        assert_eq!(idx.get_callers("foo", None, 3).len(), 3);
        assert_eq!(idx.get_callers("foo", None, 100).len(), 10, "limit > total returns all");
    }

    #[test]
    fn get_callees_filters_and_limits() {
        let idx = Index::build(
            vec![],
            vec![
                refr("a.rs", Some("main"), "foo", "call"),
                refr("a.rs", Some("main"), "Bar", "instantiation"),
                refr("a.rs", Some("main"), "baz", "call"),
                refr("a.rs", Some("helper"), "foo", "call"),
            ],
        );
        assert_eq!(idx.get_callees("main", None, 0).len(), 3);
        assert_eq!(idx.get_callees("main", Some("call"), 0).len(), 2);
        assert_eq!(idx.get_callees("main", None, 1).len(), 1);
        assert_eq!(idx.get_callees("unknown", None, 0).len(), 0);
    }

    #[test]
    fn get_file_symbols_filters_by_kind() {
        let idx = Index::build(
            vec![
                ent("a.rs", "Foo", "struct"),
                ent("a.rs", "foo", "function"),
                ent("a.rs", "bar", "function"),
                ent("b.rs", "Baz", "struct"),
            ],
            vec![],
        );
        assert_eq!(idx.get_file_symbols("a.rs", None, 0).len(), 3);
        assert_eq!(idx.get_file_symbols("a.rs", Some("function"), 0).len(), 2);
        assert_eq!(idx.get_file_symbols("a.rs", Some("struct"), 0).len(), 1);
        assert_eq!(idx.get_file_symbols("missing.rs", None, 0).len(), 0);
    }

    #[test]
    fn get_children_filters_by_parent() {
        let mut parent_foo = ent("a.rs", "method1", "method");
        parent_foo.parent = Some("Foo".to_string());
        let mut parent_foo_2 = ent("a.rs", "method2", "method");
        parent_foo_2.parent = Some("Foo".to_string());
        let mut parent_bar = ent("a.rs", "other", "method");
        parent_bar.parent = Some("Bar".to_string());

        let idx = Index::build(
            vec![
                ent("a.rs", "Foo", "struct"),
                parent_foo,
                parent_foo_2,
                parent_bar,
            ],
            vec![],
        );
        assert_eq!(idx.get_children("a.rs", "Foo", None, 0).len(), 2);
        assert_eq!(idx.get_children("a.rs", "Bar", None, 0).len(), 1);
        assert_eq!(idx.get_children("a.rs", "Nobody", None, 0).len(), 0);
        // Top-level entities (parent: None) are not children of anything.
        assert_eq!(idx.get_children("a.rs", "", None, 0).len(), 0);
    }

    #[test]
    fn get_children_respects_kind_filter_and_limit() {
        let mk = |name: &str, kind: &str, parent: &str| {
            let mut e = ent("a.rs", name, kind);
            e.parent = Some(parent.to_string());
            e
        };
        let idx = Index::build(
            vec![
                mk("m1", "method", "C"),
                mk("m2", "method", "C"),
                mk("f", "field", "C"),
                mk("m3", "method", "C"),
            ],
            vec![],
        );
        assert_eq!(idx.get_children("a.rs", "C", None, 0).len(), 4);
        assert_eq!(idx.get_children("a.rs", "C", Some("method"), 0).len(), 3);
        assert_eq!(idx.get_children("a.rs", "C", Some("method"), 2).len(), 2);
    }

    #[test]
    fn get_returns_results_in_insertion_order() {
        // Callers listed in the order they appear in refs.jsonl — sigil writes
        // refs sorted by (file, line) so this matters for stable CLI output.
        let idx = Index::build(
            vec![],
            vec![
                refr("a.rs", Some("m"), "foo", "call"),
                refr("b.rs", Some("m"), "foo", "call"),
                refr("c.rs", Some("m"), "foo", "call"),
            ],
        );
        let callers: Vec<&str> = idx.get_callers("foo", None, 0).iter().map(|r| r.file.as_str()).collect();
        assert_eq!(callers, vec!["a.rs", "b.rs", "c.rs"]);
    }

    // ──────────────────────────────────────────────────────────────────
    // Day-5 API: search / explore_dir_overview / explore_files_capped /
    // list_projects.
    // ──────────────────────────────────────────────────────────────────

    #[test]
    fn search_matches_symbol_name_substring_case_insensitive() {
        let idx = Index::build(
            vec![
                ent("src/a.rs", "ParseError", "struct"),
                ent("src/a.rs", "parse", "function"),
                ent("src/b.rs", "helper", "function"),
            ],
            vec![],
        );
        let hits = idx.search("parse", Scope::Symbols, None, None, 0);
        let names: Vec<&str> = hits
            .iter()
            .filter_map(|h| match h {
                SearchHit::Symbol(e) => Some(e.name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["ParseError", "parse"]);

        // case-insensitive
        let hits2 = idx.search("PARSE", Scope::Symbols, None, None, 0);
        assert_eq!(hits2.len(), 2);
    }

    #[test]
    fn search_respects_kind_and_path_prefix_filters() {
        let idx = Index::build(
            vec![
                ent("src/a.rs", "ParseError", "struct"),
                ent("src/a.rs", "parse", "function"),
                ent("tests/parse_test.rs", "parse", "function"),
            ],
            vec![],
        );
        let hits = idx.search("parse", Scope::Symbols, Some("function"), None, 0);
        assert_eq!(hits.len(), 2);

        let hits_src = idx.search("parse", Scope::Symbols, None, Some("src/"), 0);
        assert_eq!(hits_src.len(), 2);

        let hits_tests = idx.search("parse", Scope::Symbols, None, Some("tests/"), 0);
        assert_eq!(hits_tests.len(), 1);
    }

    #[test]
    fn search_scope_controls_symbols_vs_files() {
        let idx = Index::build(
            vec![
                ent("src/parse.rs", "helper", "function"),
                ent("src/b.rs", "parse_input", "function"),
            ],
            vec![],
        );
        let all = idx.search("parse", Scope::All, None, None, 0);
        let sym = idx.search("parse", Scope::Symbols, None, None, 0);
        let fil = idx.search("parse", Scope::Files, None, None, 0);
        assert_eq!(all.len(), sym.len() + fil.len());
        assert!(sym.iter().all(|h| matches!(h, SearchHit::Symbol(_))));
        assert!(fil.iter().all(|h| matches!(h, SearchHit::File(_))));
    }

    #[test]
    fn search_empty_query_returns_nothing() {
        let idx = Index::build(vec![ent("a.rs", "foo", "function")], vec![]);
        assert!(idx.search("", Scope::All, None, None, 0).is_empty());
    }

    #[test]
    fn search_limit_stops_early() {
        let idx = Index::build(
            (0..20).map(|i| ent(&format!("a{i}.rs"), "foo", "function")).collect(),
            vec![],
        );
        assert_eq!(idx.search("foo", Scope::Symbols, None, None, 5).len(), 5);
        assert_eq!(idx.search("foo", Scope::Symbols, None, None, 0).len(), 20);
    }

    #[test]
    fn explore_dir_overview_groups_files_by_dir() {
        let idx = Index::build(
            vec![
                ent("src/a.rs", "foo", "function"),
                ent("src/a.rs", "bar", "function"),
                ent("src/b.rs", "baz", "function"),
                ent("tests/t.rs", "t", "function"),
                ent("README.md", "hi", "section"),
            ],
            vec![],
        );
        let dirs = idx.explore_dir_overview(None);
        let by_path: std::collections::HashMap<&str, &DirSummary> =
            dirs.iter().map(|d| (d.path.as_str(), d)).collect();
        assert_eq!(by_path.get("src").unwrap().file_count, 2);
        assert_eq!(by_path.get("tests").unwrap().file_count, 1);
        assert!(by_path.get("").is_some(), "root-level files land in the empty-string dir");
    }

    #[test]
    fn explore_dir_overview_respects_prefix() {
        let idx = Index::build(
            vec![
                ent("src/a.rs", "foo", "function"),
                ent("tests/t.rs", "t", "function"),
            ],
            vec![],
        );
        let src_only = idx.explore_dir_overview(Some("src/"));
        assert_eq!(src_only.len(), 1);
        assert_eq!(src_only[0].path, "src");
    }

    #[test]
    fn explore_files_capped_caps_per_dir() {
        let idx = Index::build(
            (0..10)
                .map(|i| ent(&format!("src/f{i}.rs"), "x", "function"))
                .chain(std::iter::once(ent("tests/t.rs", "t", "function")))
                .collect(),
            vec![],
        );
        let capped = idx.explore_files_capped(None, 3);
        let src_count = capped.iter().filter(|(d, _, _)| d == "src").count();
        let tests_count = capped.iter().filter(|(d, _, _)| d == "tests").count();
        assert_eq!(src_count, 3);
        assert_eq!(tests_count, 1);
    }

    #[test]
    fn list_projects_returns_single_root() {
        let idx = Index::build(vec![], vec![]);
        assert_eq!(idx.list_projects(), vec![String::new()]);
    }

    #[test]
    fn scope_parse_accepts_codeix_strings() {
        assert_eq!(Scope::parse("symbols"), Scope::Symbols);
        assert_eq!(Scope::parse("symbol"), Scope::Symbols);
        assert_eq!(Scope::parse("files"), Scope::Files);
        assert_eq!(Scope::parse("all"), Scope::All);
        assert_eq!(Scope::parse("gibberish"), Scope::All);
    }

    #[test]
    fn lang_for_covers_rust_and_sigil_native_formats() {
        assert_eq!(super::lang_for("src/a.rs"), Some("rust"));
        assert_eq!(super::lang_for("data.json"), Some("json"));
        assert_eq!(super::lang_for("config.yaml"), Some("yaml"));
        assert_eq!(super::lang_for("config.yml"), Some("yaml"));
        assert_eq!(super::lang_for("Cargo.toml"), Some("toml"));
        assert_eq!(super::lang_for("README.md"), Some("markdown"));
        assert_eq!(super::lang_for("nosuch"), None);
    }

    #[test]
    fn parent_dir_handles_root_and_nested() {
        assert_eq!(super::parent_dir("src/a.rs"), "src");
        assert_eq!(super::parent_dir("src/foo/bar.rs"), "src/foo");
        assert_eq!(super::parent_dir("top.rs"), "");
    }

    #[test]
    fn load_roundtrips_jsonl() {
        let tmp = std::env::temp_dir().join(format!("sigil_query_rt_{}", std::process::id()));
        let sigil = tmp.join(".sigil");
        std::fs::create_dir_all(&sigil).unwrap();

        let entities = vec![ent("a.rs", "foo", "function"), ent("a.rs", "bar", "function")];
        let refs = vec![refr("a.rs", Some("foo"), "bar", "call")];

        // Reuse sigil's own writer so the format on disk matches production.
        crate::writer::write_to_files(&entities, &refs, &tmp, false).unwrap();

        let idx = Index::load(&tmp).expect("load should succeed");
        assert_eq!(idx.len(), (2, 1));
        assert_eq!(idx.entities_by_name("foo").count(), 1);
        assert_eq!(idx.refs_to("bar").count(), 1);
        assert_eq!(idx.refs_from("foo").count(), 1);

        std::fs::remove_dir_all(&tmp).ok();
    }
}
