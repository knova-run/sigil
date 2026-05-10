use serde::{Serialize, Deserialize};

// Eq is dropped because `rank: Option<f64>` can't implement Eq. Callers that
// need equality still have `PartialEq`; callers that want hashability on
// rank-less Entities can compare the struct_hash field directly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entity {
    pub file: String,
    pub name: String,
    pub kind: String,
    pub line_start: u32,
    pub line_end: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Composed `<parent>::<leaf>` form when the entity sits inside a parent
    /// (class, module). Mirrors what callers — esp. retrieval pipelines that
    /// match a user question against indexed names — would otherwise have to
    /// compute themselves. Always None for top-level entities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qualified_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
    #[serde(default, skip_serializing_if = "is_none_or_empty")]
    pub meta: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig_hash: Option<String>,
    pub struct_hash: String,

    // Phase 1 additions — all Option<T> + skip_serializing_if so old v1 JSONL
    // round-trips as None and newer writes add the fields only when populated.
    // Populated after a rank pass (src/rank.rs); None when the caller opted
    // out via `--no-rank` or when the parser didn't emit visibility info.
    #[serde(default, skip_serializing_if = "is_none_or_private")]
    pub visibility: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank: Option<f64>,
    #[serde(default, skip_serializing_if = "is_none_or_zero_blast")]
    pub blast_radius: Option<BlastRadius>,
    /// Author-provided description of the entity, harvested from the
    /// language's docstring / leading doc-comment convention (Python `"""…"""`
    /// first statement, Rust `///` and `/** */`, Go godoc, etc.). Truncated
    /// to `DOC_MAX_LEN`. Surfaced in `code.context` as the `## Doc` section
    /// so downstream LLM consumers see the hand-written intent without a
    /// follow-up file read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,

    /// Heritage edges this entity participates in. Currently populated for
    /// Go struct embedding (`type Foo struct { Bar }` ⇒ Foo embeds Bar).
    /// Empty vec is elided from JSON. Interface-implementation detection is
    /// not yet wired up (Go interfaces are structural — a separate pass).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub heritage: Vec<HeritageEdge>,
}

/// One heritage relationship between two entities — e.g. struct embedding,
/// class extension, interface implementation. The `target` is the bare name
/// of the referenced entity (qualified when the parser can resolve it via
/// the file-local import table; bare otherwise). Resolution at the JSONL
/// layer is left to consumers, who already index entities by `name`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HeritageEdge {
    /// `"embed"` for Go struct embedding. Reserved values: `"extend"`,
    /// `"implement"`, `"trait_impl"`.
    pub kind: String,
    /// Name (possibly qualified) of the parent / embedded entity.
    pub target: String,
}

/// Compose the `qualified_name` field at construction time.
///
/// When the entity has no parent, returns None — top-level entities are
/// fully-qualified by their name alone.
///
/// When the entity has a parent and the name does NOT already start with
/// the parent (e.g. Rust struct methods stored as `method_name` with
/// `parent="StructName"`), returns `parent::name`.
///
/// When the parser stores the name as `Parent.leaf` (e.g. Python methods
/// stored as `UserService.get_user`), strips the parent prefix from the
/// leaf and emits `parent::leaf` form. This keeps the output uniform
/// across language conventions — callers always see `Class::method`.
pub fn compose_qualified_name(parent: Option<&str>, name: &str) -> Option<String> {
    let parent = parent?;
    if parent.is_empty() {
        return None;
    }
    // Strip a leading `parent.` (Python / Kotlin / Scala / Swift / JS-TS
    // convention) or `parent::` (PHP / C++ convention) from the name so
    // the output never doubles up the parent prefix.
    let dot_prefix = format!("{parent}.");
    let cc_prefix = format!("{parent}::");
    let leaf = name
        .strip_prefix(&dot_prefix)
        .or_else(|| name.strip_prefix(&cc_prefix))
        .unwrap_or(name);
    Some(format!("{parent}::{leaf}"))
}

/// Cap on a preserved doc string (per-entity). Tuned to keep multi-paragraph
/// Sphinx / godoc blocks inside an LLM prompt without dominating it.
pub const DOC_MAX_LEN: usize = 1024;

/// Trim, collapse interior whitespace softly (preserve paragraph breaks),
/// and cap to `DOC_MAX_LEN` characters with a trailing `…` if truncated.
pub fn truncate_doc(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() <= DOC_MAX_LEN {
        return Some(trimmed.to_string());
    }
    let mut out: String = trimmed.chars().take(DOC_MAX_LEN).collect();
    out.push('…');
    Some(out)
}

// Skip predicates used by `Entity`'s serde attributes to elide noise fields
// during JSON emission. Keeps on-disk (.sigil/entities.jsonl) and CLI output
// identical — both represent "default / absent" the same way: omitted.

/// Skip when `None` OR when the meta vec is empty. The parser emits
/// `Some(vec![])` for Rust items that have no attribute decorators at all
/// (a common case); treating that as a field-not-present avoids two lines
/// of noise per entity in typical output.
fn is_none_or_empty(v: &Option<Vec<String>>) -> bool {
    v.as_ref().map_or(true, |m| m.is_empty())
}

/// Skip when `None` OR when visibility is explicitly `"private"` — the
/// default visibility for items in most of the languages sigil parses.
/// The on-disk JSONL written by `src/writer.rs` honors this same predicate
/// (it serializes through serde just like CLI output), so a `private`
/// visibility round-trips as the field being absent. Callers that need
/// to distinguish "absent" from "explicitly private" should re-derive
/// visibility from the parser rather than the JSONL.
fn is_none_or_private(v: &Option<String>) -> bool {
    match v {
        None => true,
        Some(s) => s == "private",
    }
}

/// Skip when `None` OR when every field of `BlastRadius` is zero. Imports
/// and genuinely-unused entities populate a BlastRadius of all-zeros; that
/// tells the consumer nothing and bulks every such entity with an extra
/// nested object.
fn is_none_or_zero_blast(b: &Option<BlastRadius>) -> bool {
    match b {
        None => true,
        Some(br) => {
            br.direct_callers == 0 && br.direct_files == 0 && br.transitive_callers == 0
        }
    }
}

/// Downstream impact summary for a single entity. Used by `sigil review`,
/// `sigil map`, `sigil blast`, and the Phase 1 ranking pipeline.
///
/// `direct_callers`   — number of reference rows targeting this entity's name.
/// `direct_files`     — distinct files those references live in.
/// `transitive_callers` — BFS over the reverse-call graph, capped at depth
/// 3 to avoid cycles and runaway cost on highly-connected symbols.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlastRadius {
    pub direct_callers: u32,
    pub direct_files: u32,
    pub transitive_callers: u32,
}

/// Path-based heuristic for "is this entity part of test code?" Covers the
/// common conventions across languages sigil parses today:
///
///   Rust   — `tests/*`, `**/tests/*`, `*_test.rs`
///   Python — `test_*.py`, `*_test.py`, `tests/**/*.py`
///   JS/TS  — `*.test.{js,ts}`, `*.spec.{js,ts}`, `__tests__/**`
///   Go     — `*_test.go`
///   Java   — `*Test.java`, `src/test/**`
///
/// Deliberately pragmatic — we don't look inside the AST for `#[cfg(test)]`
/// or `@pytest.fixture`; those would need a parser-side change. This covers
/// the 90% case for `--exclude-tests` filtering on `map`/`context`/`blast`.
pub fn is_test_path(file: &str) -> bool {
    let file = file.replace('\\', "/");
    let fname = file.rsplit('/').next().unwrap_or(&file);

    // Directory-based signals
    if file.starts_with("tests/")
        || file.starts_with("test/")
        || file.contains("/tests/")
        || file.contains("/test/")
        || file.contains("/__tests__/")
        || file.contains("/src/test/")
    {
        return true;
    }

    // Suffix-based signals
    let test_suffixes = [
        "_test.rs",
        "_test.go",
        "_test.py",
        "_test.ts",
        "_test.js",
        ".test.ts",
        ".test.tsx",
        ".test.js",
        ".test.jsx",
        ".spec.ts",
        ".spec.tsx",
        ".spec.js",
        ".spec.jsx",
        "Test.java",
        "Tests.java",
        "_spec.rb",
    ];
    if test_suffixes.iter().any(|s| fname.ends_with(s)) {
        return true;
    }

    // Prefix-based signals (Python pytest, Ruby)
    if (fname.starts_with("test_") && (fname.ends_with(".py") || fname.ends_with(".rs")))
        || fname.starts_with("Test") && fname.ends_with(".java")
    {
        return true;
    }

    false
}

#[cfg(test)]
mod compose_qualified_name_tests {
    use super::compose_qualified_name;

    #[test]
    fn bare_leaf_joins_with_double_colon() {
        assert_eq!(
            compose_qualified_name(Some("Person"), "greet"),
            Some("Person::greet".to_string()),
        );
    }

    #[test]
    fn strips_python_style_parent_dot_prefix() {
        assert_eq!(
            compose_qualified_name(Some("UserService"), "UserService.get_user"),
            Some("UserService::get_user".to_string()),
        );
    }

    #[test]
    fn strips_php_style_parent_double_colon_prefix() {
        // PHP source writes class members as `Person::greet`. Without
        // stripping that prefix the composed form would double up to
        // `Person::Person::greet`.
        assert_eq!(
            compose_qualified_name(Some("Person"), "Person::greet"),
            Some("Person::greet".to_string()),
        );
        assert_eq!(
            compose_qualified_name(Some("Person"), "Person::$name"),
            Some("Person::$name".to_string()),
        );
    }

    #[test]
    fn returns_none_for_top_level_entities() {
        assert_eq!(compose_qualified_name(None, "foo"), None);
        assert_eq!(compose_qualified_name(Some(""), "foo"), None);
    }
}

#[cfg(test)]
mod is_test_path_tests {
    use super::is_test_path;

    #[test]
    fn detects_rust_test_conventions() {
        assert!(is_test_path("tests/integration.rs"));
        assert!(is_test_path("src/foo/tests/fixture.rs"));
        assert!(is_test_path("src/parser_test.rs"));
        assert!(is_test_path("src/test_utils.rs"));
        assert!(!is_test_path("src/parser.rs"));
        assert!(!is_test_path("src/entity.rs"));
    }

    #[test]
    fn detects_python_test_conventions() {
        assert!(is_test_path("tests/test_core.py"));
        assert!(is_test_path("tests/core_test.py"));
        assert!(is_test_path("src/test_utils.py"));
        assert!(!is_test_path("src/core.py"));
    }

    #[test]
    fn detects_js_ts_test_conventions() {
        assert!(is_test_path("src/foo.test.ts"));
        assert!(is_test_path("src/foo.spec.js"));
        assert!(is_test_path("src/__tests__/foo.ts"));
        assert!(is_test_path("packages/api/tests/handler.test.tsx"));
        assert!(!is_test_path("src/foo.ts"));
    }

    #[test]
    fn detects_go_test_convention() {
        assert!(is_test_path("pkg/handler_test.go"));
        assert!(!is_test_path("pkg/handler.go"));
    }

    #[test]
    fn detects_java_test_conventions() {
        assert!(is_test_path("src/main/java/FooTest.java"));
        assert!(is_test_path("src/test/java/FooTest.java"));
        assert!(is_test_path("com/example/TestFoo.java"));
        assert!(!is_test_path("src/main/java/Foo.java"));
    }

    #[test]
    fn does_not_false_positive_on_words_containing_test() {
        // "test" as a substring inside a non-test-conventional path.
        assert!(!is_test_path("src/attestation.rs"));
        assert!(!is_test_path("src/latest.py"));
        assert!(!is_test_path("contest/engine.ts"));
    }

    #[test]
    fn handles_windows_style_paths() {
        assert!(is_test_path("tests\\integration.rs"));
        assert!(is_test_path("src\\foo\\__tests__\\bar.ts"));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Reference {
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
    pub name: String,
    // Serialized as `kind` for schema parity with `Entity`. The Rust field
    // name stays `ref_kind` to avoid keyword-adjacent shadowing in code
    // that mixes both types. Deserialization accepts both `kind` (new) and
    // `ref_kind` (old pre-0.4.0 .sigil/refs.jsonl).
    #[serde(rename = "kind", alias = "ref_kind")]
    pub ref_kind: String,
    pub line: u32,
    /// Resolver confidence for this edge. `1.0` = exact same-file resolution
    /// (the caller and callee both live in this file's symbol table).
    /// `0.8` = call resolved through a file-local import alias to a
    /// qualified package path (e.g. `fmt.Println` → `fmt/Println`).
    /// `None` = bare textual reference, no resolution attempted (the legacy
    /// behaviour). Old refs.jsonl rows round-trip as `None` so existing
    /// indexes keep loading without a re-build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}
