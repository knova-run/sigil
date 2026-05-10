//! `sigil grep <pattern>` — text search with structural annotation.
//!
//! The everyday workhorse. Reads like grep, returns like grep, but each
//! hit is annotated with the enclosing entity (class/method/function) via
//! a binary search over `Index.entities`. That collapses the agent-side
//! `grep` + `read_file` chain that the E4_003 eval showed loses 2–3×
//! tokens when the bug report names a specific literal like
//! `FILE_INPUT_CONTRADICTION`.
//!
//! Design intent:
//!   * **strict superset of ripgrep-style output**. `file:line:text` is the
//!     anchor; structural columns are appended in a stable position. Turn
//!     them off with `--no-entity` and the tool looks exactly like grep.
//!   * **flags in two planes only**: TEXT (what pattern matches) and
//!     SCOPE (where to look). No cross-plane flags.
//!   * **orthogonal composability**: every filter is a separate knob;
//!     chaining is addition, never mode-switching.
//!
//! Walking discipline: `ignore::WalkBuilder` + respect for `.gitignore`,
//! same conventions the indexer uses. We don't open binaries, skip
//! unreadable files silently (grep's default behavior), and return exit
//! code 1 on "no matches" to match ripgrep.

use std::fs;
use std::path::Path;

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use regex::RegexBuilder;
use serde::Serialize;

use crate::entity::Entity;
use crate::query::index::Index;

/// A single grep hit, annotated with the enclosing entity when one exists.
///
/// `entity` / `kind` / `parent` are all `None` when the match landed in
/// top-level module code / a license comment / a generated file that's
/// indexed but not scoped. Agents must tolerate empty fields — the JSON
/// schema uses `skip_serializing_if` so the field just vanishes.
#[derive(Debug, Clone, Serialize)]
pub struct GrepHit {
    pub file: String,
    pub line: u32,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

/// Aggregate shape when `--group-by` is set. Simpler than a full text
/// dump; agents reading a grouped result need the key, the count, and a
/// couple of sample lines to sanity-check.
#[derive(Debug, Clone, Serialize)]
pub struct GrepGroup {
    pub key: String,
    pub count: u32,
    /// Up to 3 sample lines, newest-first (by file order within group).
    pub sample_lines: Vec<String>,
}

/// Full report. Exactly one of `hits` / `groups` is populated depending on
/// whether `--group-by` was requested. Keeping them sibling fields (not an
/// enum) so JSON consumers don't have to discriminate on a tag — empty
/// vectors are unambiguous.
#[derive(Debug, Clone, Serialize)]
pub struct GrepReport {
    pub pattern: String,
    pub total_hits: u32,
    /// True iff the `limit` truncated the output.
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hits: Vec<GrepHit>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<GrepGroup>,
}

/// What the `--group-by` flag accepts. Each variant is a mutually
/// exclusive aggregation key. Keep the set small — more keys = more
/// schemas to document to the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    File,
    /// Aggregate by enclosing class (`parent`). Top-level hits group
    /// under the synthetic key `<top-level>`.
    Class,
    /// Aggregate by the full enclosing entity name
    /// (e.g. `FileField.clean`). Most granular.
    Entity,
    Kind,
}

impl GroupBy {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "file" => Some(Self::File),
            "class" | "parent" => Some(Self::Class),
            "entity" => Some(Self::Entity),
            "kind" => Some(Self::Kind),
            _ => None,
        }
    }
}

/// Everything needed to drive a single `sigil grep` invocation. All
/// fields are pure data — no filesystem handles — so the module is
/// testable on synthetic fixtures.
#[derive(Debug, Clone)]
pub struct GrepOptions {
    pub pattern: String,
    pub case_insensitive: bool,
    pub word_match: bool,
    pub fixed_strings: bool,
    /// File path substring filter. Repeatable via comma separation on
    /// the CLI side; stored here as an already-split Vec.
    pub file_filter: Vec<String>,
    /// Glob patterns (ripgrep-style). Multiple globs = OR.
    pub globs: Vec<String>,
    /// Only hits whose enclosing entity's parent tail-equals this name.
    /// Collapses the old `sigil where --parent C` flag into grep's scope
    /// plane. An empty string means "top-level only."
    pub class_filter: Option<String>,
    /// Only hits whose enclosing entity's name tail-equals this. Useful
    /// for "show me every call-site to X *inside* `render_template`."
    pub caller_filter: Option<String>,
    /// Max hits to return. 0 = unlimited. Default 50 (empirical — one
    /// screen of grep output is what agents can hold in context without
    /// losing the thread).
    pub limit: usize,
    /// Drop the structural column (entity/kind/parent). Useful for
    /// tooling that expects strict ripgrep output.
    pub no_entity: bool,
    /// Aggregate instead of returning rows.
    pub group_by: Option<GroupBy>,
}

impl Default for GrepOptions {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            case_insensitive: false,
            word_match: false,
            fixed_strings: false,
            file_filter: Vec::new(),
            globs: Vec::new(),
            class_filter: None,
            caller_filter: None,
            limit: 50,
            no_entity: false,
            group_by: None,
        }
    }
}

/// Last `::`- or `.`-separated segment of a qualified name. Mirrors the
/// tail-segment rule used elsewhere (`where_cmd`, `context`) so
/// `--class ModelChoiceField` matches `django.forms.models.ModelChoiceField`.
fn tail_segment(name: &str) -> &str {
    name.rsplit(|c| c == ':' || c == '.').next().unwrap_or(name)
}

/// Binary-search `entities` (already sorted by `(file, line_start)` by the
/// Index) for the innermost entity containing `(file, line)`.
///
/// "Innermost" matters for nested fns (Python/JS) — we want the most-
/// specific enclosing method, not the outer class. Entities are sorted by
/// line_start, so the **latest** match is the innermost; we walk
/// backward once we find the first candidate.
///
/// Returns `None` when the hit lands outside any indexed entity (license
/// comment, top-of-file imports, etc.).
fn enclosing_entity<'a>(entities: &'a [Entity], file: &str, line: u32) -> Option<&'a Entity> {
    // Find the slice of entities for this file via binary search on
    // (file, _). Sort order guarantees all same-file entries are
    // contiguous.
    let lo = entities.partition_point(|e| e.file.as_str() < file);
    let hi = entities.partition_point(|e| e.file.as_str() <= file);
    if lo == hi {
        return None;
    }
    let slice = &entities[lo..hi];

    // Among same-file entries, find the innermost whose
    // [line_start, line_end] contains `line`. Prefer the entity with the
    // largest line_start (innermost when nested). Ignore module-kind
    // rows; they span the whole file and would shadow real functions.
    slice
        .iter()
        .rev()
        .find(|e| e.line_start <= line && line <= e.line_end && e.kind != "module")
}

/// Build a `regex::Regex` honoring the TEXT-plane flags. Returns an
/// `anyhow::Error` wrapping the pattern on compile failure so the caller
/// can print a one-line explanation rather than a stack trace.
pub fn compile_pattern(opts: &GrepOptions) -> anyhow::Result<regex::Regex> {
    let pattern = if opts.fixed_strings {
        regex::escape(&opts.pattern)
    } else {
        opts.pattern.clone()
    };
    let pattern = if opts.word_match {
        format!(r"\b(?:{pattern})\b")
    } else {
        pattern
    };
    RegexBuilder::new(&pattern)
        .case_insensitive(opts.case_insensitive)
        .build()
        .map_err(|e| anyhow::anyhow!("invalid pattern `{}`: {e}", opts.pattern))
}

/// Build a globset from the `globs` list. Empty input returns `None`,
/// meaning "no glob filter." Malformed globs error out at build time so
/// the CLI prints one clean message and exits rather than silently
/// matching nothing.
fn build_globset(globs: &[String]) -> anyhow::Result<Option<GlobSet>> {
    if globs.is_empty() {
        return Ok(None);
    }
    let mut b = GlobSetBuilder::new();
    for g in globs {
        b.add(Glob::new(g).map_err(|e| anyhow::anyhow!("invalid glob `{g}`: {e}"))?);
    }
    Ok(Some(b.build()?))
}

/// Main entry point. Scans the tree under `root`, collects matches, and
/// returns a `GrepReport` shaped per `opts.group_by`.
pub fn run_grep(root: &Path, idx: &Index, opts: &GrepOptions) -> anyhow::Result<GrepReport> {
    let re = compile_pattern(opts)?;
    let globset = build_globset(&opts.globs)?;
    let mut hits: Vec<GrepHit> = Vec::new();
    let cap = if opts.limit == 0 {
        usize::MAX
    } else {
        opts.limit
    };

    // Walker: respect .gitignore + skip hidden directories. `.sigil/`,
    // `.git/`, `node_modules/` etc. shouldn't surface in a grep for
    // source-level patterns. Same discipline as ripgrep's default.
    let walker = WalkBuilder::new(root)
        .hidden(true)
        .follow_links(false)
        .build();
    let mut total_hits = 0u32;

    for dent in walker.filter_map(Result::ok) {
        let path = dent.path();
        if !path.is_file() {
            continue;
        }
        // Repo-relative path for output + filtering. We store paths the
        // same way `Index.entities` does so lookups match.
        let rel = match path.strip_prefix(root) {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(_) => path.to_string_lossy().to_string(),
        };

        if !opts.file_filter.is_empty()
            && !opts.file_filter.iter().any(|substr| rel.contains(substr))
        {
            continue;
        }
        if let Some(gs) = globset.as_ref() {
            if !gs.is_match(&rel) {
                continue;
            }
        }

        // Binary file guard: skip files whose first 1 KB has a NUL byte.
        // Mirrors ripgrep's default. Prevents scanning e.g. .pyc files.
        let content = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        for (idx_line, line_text) in content.lines().enumerate() {
            if !re.is_match(line_text) {
                continue;
            }
            let line = (idx_line + 1) as u32;
            let ent = if opts.no_entity {
                None
            } else {
                enclosing_entity(&idx.entities, &rel, line)
            };

            // class/caller filters kick in after we know the enclosing
            // entity; rows with no enclosing entity are dropped when
            // either filter is set (structural filter can't be satisfied
            // without structure).
            if let Some(cls) = opts.class_filter.as_deref() {
                let matches = match ent.and_then(|e| e.parent.as_deref()) {
                    Some(p) if cls.is_empty() => false, // --class "" = top-level
                    Some(p) => p == cls || tail_segment(p) == cls,
                    None => cls.is_empty() && ent.is_some(),
                };
                if !matches {
                    continue;
                }
            }
            if let Some(caller) = opts.caller_filter.as_deref() {
                let matches = match ent.map(|e| e.name.as_str()) {
                    Some(n) => n == caller || tail_segment(n) == caller,
                    None => false,
                };
                if !matches {
                    continue;
                }
            }

            total_hits += 1;
            if hits.len() < cap {
                let (entity, kind, parent) = match ent {
                    Some(e) => (
                        Some(match e.parent.as_deref() {
                            Some(p) => format!("{}.{}", tail_segment(p), tail_segment(&e.name)),
                            None => tail_segment(&e.name).to_string(),
                        }),
                        Some(e.kind.clone()),
                        e.parent.clone(),
                    ),
                    None => (None, None, None),
                };
                hits.push(GrepHit {
                    file: rel.clone(),
                    line,
                    text: line_text.trim_end().to_string(),
                    entity,
                    kind,
                    parent,
                });
            }
        }
    }

    let truncated = (total_hits as usize) > hits.len();

    if let Some(gb) = opts.group_by {
        let groups = aggregate(&hits, gb);
        return Ok(GrepReport {
            pattern: opts.pattern.clone(),
            total_hits,
            truncated,
            hits: Vec::new(),
            groups,
        });
    }

    Ok(GrepReport {
        pattern: opts.pattern.clone(),
        total_hits,
        truncated,
        hits,
        groups: Vec::new(),
    })
}

/// Build the grouped view. Agents typically don't need full text on
/// grouped output — `sample_lines` gives a 3-item preview so they can
/// verify the aggregation without another call.
fn aggregate(hits: &[GrepHit], gb: GroupBy) -> Vec<GrepGroup> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<String, (u32, Vec<String>)> = BTreeMap::new();
    for h in hits {
        let key = match gb {
            GroupBy::File => h.file.clone(),
            GroupBy::Class => h.parent.clone().unwrap_or_else(|| "<top-level>".to_string()),
            GroupBy::Entity => h.entity.clone().unwrap_or_else(|| "<top-level>".to_string()),
            GroupBy::Kind => h.kind.clone().unwrap_or_else(|| "<none>".to_string()),
        };
        let entry = map.entry(key).or_insert_with(|| (0, Vec::new()));
        entry.0 += 1;
        if entry.1.len() < 3 {
            entry.1.push(format!("{}:{} {}", h.file, h.line, h.text.trim()));
        }
    }
    // Sort groups by count desc, then key asc for a stable tiebreaker.
    let mut groups: Vec<GrepGroup> = map
        .into_iter()
        .map(|(key, (count, sample_lines))| GrepGroup {
            key,
            count,
            sample_lines,
        })
        .collect();
    groups.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.key.cmp(&b.key)));
    groups
}

pub fn render_text(report: &GrepReport) -> String {
    if !report.groups.is_empty() {
        let mut out = String::with_capacity(512);
        for g in &report.groups {
            out.push_str(&format!("{:>6}  {}\n", g.count, g.key));
            for s in &g.sample_lines {
                out.push_str(&format!("          {s}\n"));
            }
        }
        if report.truncated {
            out.push_str(&format!(
                "\n(truncated; {} total hits — rerun with --limit 0 to see all before grouping)\n",
                report.total_hits
            ));
        }
        return out;
    }
    let mut out = String::with_capacity(512);
    for h in &report.hits {
        match (h.entity.as_deref(), h.kind.as_deref()) {
            (Some(ent), Some(kind)) => {
                out.push_str(&format!(
                    "{}:{}:{}:{}: {}\n",
                    h.file, h.line, ent, kind, h.text
                ));
            }
            _ => {
                // No structural context — fall back to classic grep shape.
                out.push_str(&format!("{}:{}: {}\n", h.file, h.line, h.text));
            }
        }
    }
    if report.truncated {
        out.push_str(&format!(
            "(showing {}/{} hits — narrow with --file/--glob/--class or `--limit 0` for all)\n",
            report.hits.len(),
            report.total_hits,
        ));
    }
    out
}

pub fn render_json(report: &GrepReport, pretty: bool) -> String {
    if pretty {
        serde_json::to_string_pretty(report).expect("GrepReport serializes infallibly")
    } else {
        serde_json::to_string(report).expect("GrepReport serializes infallibly")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{BlastRadius, Entity};
    use crate::query::index::Index;
    use std::io::Write;
    use tempfile_or_self::TempDir;

    // Minimal tempdir shim (the crate isn't in our deps). Creates a
    // unique subdir under `std::env::temp_dir()` and cleans up on drop.
    mod tempfile_or_self {
        use std::path::PathBuf;
        pub struct TempDir {
            path: PathBuf,
        }
        impl TempDir {
            pub fn new(tag: &str) -> Self {
                let path = std::env::temp_dir().join(format!(
                    "sigil-grep-test-{}-{}",
                    std::process::id(),
                    tag
                ));
                let _ = std::fs::remove_dir_all(&path);
                std::fs::create_dir_all(&path).unwrap();
                Self { path }
            }
            pub fn path(&self) -> &std::path::Path {
                &self.path
            }
        }
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.path);
            }
        }
    }

    fn ent(file: &str, name: &str, kind: &str, parent: Option<&str>, lo: u32, hi: u32) -> Entity {
        Entity {
            file: file.into(),
            name: name.into(),
            kind: kind.into(),
            line_start: lo,
            line_end: hi,
            parent: parent.map(String::from),
            qualified_name: None,
            sig: None,
            meta: None,
            body_hash: None,
            sig_hash: None,
            struct_hash: "h".into(),
            visibility: None,
            rank: None,
            blast_radius: Some(BlastRadius::default()),
            doc: None,
        }
    }

    #[test]
    fn enclosing_entity_picks_innermost_when_nested() {
        let entities = vec![
            ent("a.py", "Outer", "class", None, 1, 100),
            ent("a.py", "Outer.foo", "method", Some("Outer"), 10, 20),
            ent("a.py", "Outer.bar", "method", Some("Outer"), 30, 40),
        ];
        // hit on line 15 should resolve to Outer.foo, not Outer
        let e = enclosing_entity(&entities, "a.py", 15).expect("found");
        assert_eq!(e.name, "Outer.foo");
    }

    #[test]
    fn enclosing_entity_skips_module_kind() {
        let entities = vec![
            ent("a.py", "a", "module", None, 1, 100),
            ent("a.py", "foo", "function", None, 5, 8),
        ];
        let e = enclosing_entity(&entities, "a.py", 6).expect("found");
        assert_eq!(e.name, "foo");
    }

    #[test]
    fn enclosing_entity_returns_none_when_outside() {
        let entities = vec![
            ent("a.py", "foo", "function", None, 5, 8),
        ];
        assert!(enclosing_entity(&entities, "a.py", 20).is_none());
    }

    #[test]
    fn enclosing_entity_handles_multiple_files() {
        let entities = vec![
            ent("a.py", "A", "class", None, 1, 10),
            ent("b.py", "B", "class", None, 1, 10),
        ];
        assert_eq!(enclosing_entity(&entities, "a.py", 5).unwrap().name, "A");
        assert_eq!(enclosing_entity(&entities, "b.py", 5).unwrap().name, "B");
        assert!(enclosing_entity(&entities, "c.py", 5).is_none());
    }

    #[test]
    fn grep_annotates_match_with_enclosing_class() {
        let tmp = TempDir::new("basic");
        let file = tmp.path().join("forms.py");
        let body = "\
# top of file
class FileField:
    def clean(self, data, initial=None):
        if data is FILE_INPUT_CONTRADICTION:
            raise ValidationError('bad')
        return super().clean(data)
";
        std::fs::File::create(&file)
            .unwrap()
            .write_all(body.as_bytes())
            .unwrap();
        let entities = vec![
            ent("forms.py", "FileField", "class", None, 2, 7),
            ent("forms.py", "FileField.clean", "method", Some("FileField"), 3, 6),
        ];
        let idx = Index::build(entities, vec![]);
        let opts = GrepOptions {
            pattern: "FILE_INPUT_CONTRADICTION".into(),
            ..Default::default()
        };
        let rep = run_grep(tmp.path(), &idx, &opts).unwrap();
        assert_eq!(rep.total_hits, 1);
        let hit = &rep.hits[0];
        assert_eq!(hit.file, "forms.py");
        assert_eq!(hit.line, 4);
        assert_eq!(hit.entity.as_deref(), Some("FileField.clean"));
        assert_eq!(hit.kind.as_deref(), Some("method"));
        assert_eq!(hit.parent.as_deref(), Some("FileField"));
    }

    #[test]
    fn grep_class_filter_restricts_hits_to_one_class() {
        let tmp = TempDir::new("class-filter");
        let file = tmp.path().join("forms.py");
        let body = "\
class Foo:
    def clean(self):
        raise ValidationError('bad')
class Bar:
    def clean(self):
        raise ValidationError('bad')
";
        std::fs::File::create(&file)
            .unwrap()
            .write_all(body.as_bytes())
            .unwrap();
        let entities = vec![
            ent("forms.py", "Foo", "class", None, 1, 3),
            ent("forms.py", "Foo.clean", "method", Some("Foo"), 2, 3),
            ent("forms.py", "Bar", "class", None, 4, 6),
            ent("forms.py", "Bar.clean", "method", Some("Bar"), 5, 6),
        ];
        let idx = Index::build(entities, vec![]);
        let opts = GrepOptions {
            pattern: "ValidationError".into(),
            class_filter: Some("Foo".into()),
            ..Default::default()
        };
        let rep = run_grep(tmp.path(), &idx, &opts).unwrap();
        assert_eq!(rep.hits.len(), 1);
        assert_eq!(rep.hits[0].parent.as_deref(), Some("Foo"));
    }

    #[test]
    fn grep_group_by_class_aggregates_counts() {
        let tmp = TempDir::new("group-class");
        let file = tmp.path().join("a.py");
        let body = "\
class Foo:
    def a(self): raise X
    def b(self): raise X
class Bar:
    def c(self): raise X
";
        std::fs::File::create(&file)
            .unwrap()
            .write_all(body.as_bytes())
            .unwrap();
        let entities = vec![
            ent("a.py", "Foo", "class", None, 1, 3),
            ent("a.py", "Foo.a", "method", Some("Foo"), 2, 2),
            ent("a.py", "Foo.b", "method", Some("Foo"), 3, 3),
            ent("a.py", "Bar", "class", None, 4, 5),
            ent("a.py", "Bar.c", "method", Some("Bar"), 5, 5),
        ];
        let idx = Index::build(entities, vec![]);
        let opts = GrepOptions {
            pattern: "raise X".into(),
            group_by: Some(GroupBy::Class),
            ..Default::default()
        };
        let rep = run_grep(tmp.path(), &idx, &opts).unwrap();
        assert_eq!(rep.groups.len(), 2);
        assert_eq!(rep.groups[0].key, "Foo");
        assert_eq!(rep.groups[0].count, 2);
        assert_eq!(rep.groups[1].key, "Bar");
        assert_eq!(rep.groups[1].count, 1);
    }

    #[test]
    fn grep_word_match_respects_boundaries() {
        let tmp = TempDir::new("word");
        let file = tmp.path().join("a.py");
        let body = "foo = 1\nfoobar = 2\nfoo_bar = 3\n";
        std::fs::File::create(&file)
            .unwrap()
            .write_all(body.as_bytes())
            .unwrap();
        let idx = Index::build(vec![], vec![]);
        let opts = GrepOptions {
            pattern: "foo".into(),
            word_match: true,
            ..Default::default()
        };
        let rep = run_grep(tmp.path(), &idx, &opts).unwrap();
        // Matches `foo` on line 1 (surrounded by whitespace). `foobar`
        // on line 2 doesn't match (trailing `b` is a word-char). `foo_bar`
        // on line 3 doesn't match either — `_` is also a word-char in
        // `\b`, same as ripgrep. Net: exactly one hit.
        assert_eq!(rep.hits.len(), 1);
        assert_eq!(rep.hits[0].line, 1);
    }

    #[test]
    fn grep_no_entity_falls_back_to_classic_shape() {
        let tmp = TempDir::new("no-entity");
        let file = tmp.path().join("a.py");
        std::fs::File::create(&file)
            .unwrap()
            .write_all(b"hello world\n")
            .unwrap();
        let idx = Index::build(
            vec![ent("a.py", "top", "function", None, 1, 1)],
            vec![],
        );
        let opts = GrepOptions {
            pattern: "hello".into(),
            no_entity: true,
            ..Default::default()
        };
        let rep = run_grep(tmp.path(), &idx, &opts).unwrap();
        assert_eq!(rep.hits.len(), 1);
        assert!(rep.hits[0].entity.is_none(), "no-entity strips structural column");
        assert!(rep.hits[0].kind.is_none());
    }

    #[test]
    fn grep_limit_truncates_and_sets_flag() {
        let tmp = TempDir::new("limit");
        let file = tmp.path().join("a.py");
        let body = (0..10)
            .map(|i| format!("hello {i}\n"))
            .collect::<String>();
        std::fs::File::create(&file)
            .unwrap()
            .write_all(body.as_bytes())
            .unwrap();
        let idx = Index::build(vec![], vec![]);
        let opts = GrepOptions {
            pattern: "hello".into(),
            limit: 3,
            ..Default::default()
        };
        let rep = run_grep(tmp.path(), &idx, &opts).unwrap();
        assert_eq!(rep.hits.len(), 3);
        assert_eq!(rep.total_hits, 10);
        assert!(rep.truncated);
    }
}
