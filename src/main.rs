use sigil::diff;
use sigil::formatter;
use sigil::git;
use sigil::grouping;
use sigil::index;
use sigil::markdown_formatter;
use sigil::output;
use sigil::query;
use sigil::writer;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "sigil",
    about = "Deterministic structural code intelligence for AI coding agents",
    long_about = "\
Deterministic structural code intelligence for AI coding agents.

sigil groups commands into two tiers:

  AGENT-FACING (narrated, budget-aware, markdown-first):
    map         Ranked codebase digest for cold-start orientation
    context     Signature + callers + callees + related types, budget-capped
    review      PR review: structural diff + rank + blast + co-change misses
    blast       Impact summary — callers, files, transitive reach
    benchmark   Publishes median token reduction vs raw alternatives

  SCRIPT-FACING (raw, unbounded, JSON-friendly):
    search        Substring search over symbols + file paths
    symbols       All entities in a file
    children      Entities under a parent
    callers       All refs targeting a symbol (unbounded)
    callees       What a symbol calls
    explore       Directory overview
    duplicates    Clone report across the codebase
    cochange      Git-history file-pair co-change miner
    identifiers   Symbol-shaped tokens lifted from arbitrary text
    decisions     `WHY:` / `DECISION:` / `TRADEOFF:` comment markers
    package-deps  Dependency edges from manifest files (go.mod, package.json)
    contracts     HTTP routes, gRPC services, queue topics
    workspace     Discover child git repos under a parent directory
    hotspots      File churn × line count risk score
    ownership     Per-file primary author from git history
    security-scan Lightweight regex security-signal extractor
    heritage      Struct embedding / extension / impl graph for a symbol

  INSTALLERS (platform integrations, all idempotent):
    claude · cursor · codex · gemini · opencode · aider · copilot · hook

Plus `index` (build the .sigil/ index), `diff` (the 0.2.x structural diff
engine), `update` (self-update via axoupdater).",
    version
)]
enum Cli {
    /// Build the entity index for a project
    Index {
        /// Project root directory
        #[arg(short, long, default_value = ".")]
        root: PathBuf,

        /// Only index specific files
        #[arg(long)]
        files: Vec<PathBuf>,

        /// Write to stdout instead of .sigil/ files
        #[arg(long)]
        stdout: bool,

        /// Pretty-print JSON output
        #[arg(long)]
        pretty: bool,

        /// Force full re-index, ignore cache
        #[arg(long)]
        full: bool,

        /// Skip reference extraction
        #[arg(long)]
        no_refs: bool,

        /// Skip the rank + blast-radius pass (Phase 1). Rank is on by
        /// default; this flag is a one-off opt-out for CI/speed cases.
        #[arg(long)]
        no_rank: bool,

        /// Print progress information
        #[arg(short, long)]
        verbose: bool,
    },
    /// Structural diff between two git refs or two files
    Diff {
        /// Ref spec: HEAD~1, main..HEAD, abc123..def456
        #[arg(required_unless_present = "files")]
        ref_spec: Option<String>,

        /// Compare two files directly instead of git refs
        #[arg(long, num_args = 2, value_names = ["OLD", "NEW"])]
        files: Vec<PathBuf>,

        /// Project root directory
        #[arg(short, long, default_value = ".")]
        root: PathBuf,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Pretty-print JSON output
        #[arg(long)]
        pretty: bool,

        /// Print progress information
        #[arg(short, long)]
        verbose: bool,

        /// Show line numbers next to entity names
        #[arg(long)]
        lines: bool,

        /// Lines of context around changes (default 3, use --no-context to disable)
        #[arg(long, default_value = "3")]
        context: usize,

        /// Disable code context in output
        #[arg(long)]
        no_context: bool,

        /// Output as GitHub-flavored Markdown
        #[arg(long)]
        markdown: bool,

        /// Use ASCII glyphs instead of emoji (with --markdown)
        #[arg(long)]
        no_emoji: bool,

        /// Disable ANSI color output
        #[arg(long)]
        no_color: bool,

        /// Skip caller analysis for breaking changes
        #[arg(long)]
        no_callers: bool,

        /// Show one-line summary of changes
        #[arg(long)]
        summary: bool,

        /// Group related changes together
        #[arg(long)]
        group: bool,
    },
    /// Explore project structure: files grouped by directory
    Explore {
        /// Project root directory
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        /// Filter to a subdirectory
        #[arg(long)]
        path: Option<String>,
        /// Max entries to show
        #[arg(long, default_value = "200")]
        max_entries: usize,
        /// Output as JSON (compact by default — see --pretty)
        #[arg(long)]
        json: bool,
        /// Pretty-print JSON output (default: minified)
        #[arg(long)]
        pretty: bool,
    },
    /// Search across symbols, files, and texts
    Search {
        /// Search query (FTS5 syntax, supports * wildcards)
        query: String,
        /// Project root directory
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        /// Filter by scope: symbol (default), file, all. Defaults to
        /// `symbol` because agents almost always want symbol hits —
        /// file-path matches add noise on keyword queries. Pass `--scope
        /// all` or `--scope file` to widen.
        #[arg(long, default_value = "symbol")]
        scope: Vec<String>,
        /// Filter by kind (e.g., function, class, method)
        #[arg(long)]
        kind: Vec<String>,
        /// Filter by file path (GLOB pattern)
        #[arg(long)]
        path: Option<String>,
        /// Max results
        #[arg(long, default_value = "20")]
        limit: u32,
        /// Output as JSON (compact by default — see --pretty)
        #[arg(long)]
        json: bool,
        /// Pretty-print JSON output (default: minified)
        #[arg(long)]
        pretty: bool,
    },
    /// List all symbols in a file
    Symbols {
        /// File path (supports GLOB patterns)
        file: String,
        /// Project root directory
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        /// Max results (0 = unlimited, the default — script-facing commands are unbounded)
        #[arg(long, default_value = "0")]
        limit: u32,
        /// Outline depth. `1` = top-level items only (classes, top-level
        /// functions, structs, enums, traits, sections) — drops imports,
        /// nested methods, variables, constants. `0` (default) = every
        /// entity extracted for the file.
        #[arg(long, default_value = "0")]
        depth: u32,
        /// Output as JSON (compact by default — see --pretty, --with-hashes)
        #[arg(long)]
        json: bool,
        /// Pretty-print JSON output (default: minified)
        #[arg(long)]
        pretty: bool,
        /// Emit just a flat array of tail-segment names — answers "list
        /// the Xs in this file" in the minimum possible payload. Typical
        /// drop: ~140 bytes/row of entity JSON → 10-20 bytes/name.
        /// Compose with `--depth 1` for top-level names only.
        #[arg(long)]
        names_only: bool,
        /// Include BLAKE3 hash columns (struct_hash, body_hash, sig_hash) in
        /// the JSON output. Off by default — useful for scripts that need
        /// the raw on-disk shape.
        #[arg(long)]
        with_hashes: bool,
    },
    /// Get children of a class or module
    Children {
        /// File containing the parent symbol
        file: String,
        /// Parent symbol name
        parent: String,
        /// Project root directory
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        /// Max results (0 = unlimited, the default — script-facing commands are unbounded)
        #[arg(long, default_value = "0")]
        limit: u32,
        /// Output as JSON (compact by default — see --pretty, --with-hashes)
        #[arg(long)]
        json: bool,
        /// Pretty-print JSON output (default: minified)
        #[arg(long)]
        pretty: bool,
        /// Include BLAKE3 hash columns in the JSON output.
        #[arg(long)]
        with_hashes: bool,
    },
    /// Find all callers/references to a symbol
    Callers {
        /// Symbol name to find callers of
        name: String,
        /// Project root directory
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        /// Filter by reference kind (call, import, type_annotation, instantiation)
        #[arg(long)]
        kind: Option<String>,
        /// Max results (0 = unlimited, the default — script-facing commands are unbounded)
        #[arg(long, default_value = "0")]
        limit: u32,
        /// Collapse output to {file: count} aggregation. Useful when you
        /// only need the file distribution, not per-call-site detail.
        /// 128 refs → a few-entry map.
        #[arg(long, value_name = "DIM")]
        group_by: Option<String>,
        /// Output as JSON (compact by default — see --pretty)
        #[arg(long)]
        json: bool,
        /// Pretty-print JSON output (default: minified)
        #[arg(long)]
        pretty: bool,
    },
    /// Find all symbols that a function calls
    Callees {
        /// Caller symbol name
        caller: String,
        /// Project root directory
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        /// Filter by reference kind
        #[arg(long)]
        kind: Option<String>,
        /// Max results (0 = unlimited, the default — script-facing commands are unbounded)
        #[arg(long, default_value = "0")]
        limit: u32,
        /// Collapse output to {name: count} aggregation (what does <caller>
        /// call most?).
        #[arg(long, value_name = "DIM")]
        group_by: Option<String>,
        /// Output as JSON (compact by default — see --pretty)
        #[arg(long)]
        json: bool,
        /// Pretty-print JSON output (default: minified)
        #[arg(long)]
        pretty: bool,
    },
    /// Text search + structural annotation. Reads like grep, returns
    /// like grep, but each hit is annotated with the enclosing entity
    /// (class / method / function). Collapses the common `grep X` +
    /// `read_file F` chain into one call — the hit itself tells you
    /// what class or method you're looking at.
    ///
    /// Default output is `file:line:entity:kind:text`. Drop the
    /// structural column with `--no-entity` for strict grep-compatible
    /// output. When the hit lands outside any indexed entity (license
    /// comment, top-level imports) the structural columns are omitted
    /// and the row falls back to `file:line:text`.
    Grep {
        /// Regex pattern. Pass `-F` for literal strings.
        pattern: String,
        /// Project root directory.
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        /// Case-insensitive match (ripgrep-compatible `-i`).
        #[arg(short = 'i', long = "ignore-case")]
        ignore_case: bool,
        /// Whole-word match (ripgrep-compatible `-w`).
        #[arg(short = 'w', long = "word")]
        word: bool,
        /// Treat pattern as a fixed string, not a regex (ripgrep-compatible `-F`).
        #[arg(short = 'F', long = "fixed-strings")]
        fixed_strings: bool,
        /// Only hits whose file path contains SUBSTR (repeatable).
        #[arg(long, value_name = "SUBSTR")]
        file: Vec<String>,
        /// Glob patterns to match file paths (ripgrep-compatible).
        #[arg(long, value_name = "PATTERN")]
        glob: Vec<String>,
        /// Only hits whose enclosing entity's parent class tail-equals C.
        /// Folds the old `sigil where --parent C` pattern into grep's
        /// scope plane — `sigil grep X --class FileField` finds `X` only
        /// inside FileField methods.
        #[arg(long, value_name = "C")]
        class: Option<String>,
        /// Only hits whose enclosing entity's name tail-equals FN. Use
        /// for "find every usage of X *inside* render_template."
        #[arg(long, value_name = "FN")]
        caller: Option<String>,
        /// Max hits to return. 0 = unlimited. Default 50.
        #[arg(long, default_value = "50")]
        limit: usize,
        /// Aggregate counts instead of returning rows. Values:
        /// `file`, `class`, `entity`, `kind`.
        #[arg(long, value_name = "KEY")]
        group_by: Option<String>,
        /// Drop the structural column from every row. The output then
        /// looks exactly like ripgrep (`file:line:text`).
        #[arg(long)]
        no_entity: bool,
        /// Output format: `text` (default, ripgrep-shaped) or `json`.
        #[arg(long, default_value = "text")]
        format: String,
        /// Pretty-print when `--format=json`.
        #[arg(long)]
        pretty: bool,
    },
    /// Hierarchical outline — every top-level class / function / struct /
    /// enum / trait grouped by file. Complements `sigil map` (rank-
    /// ordered, budget-aware) by giving a plain structural tree for
    /// "what's in this directory" questions.
    Outline {
        /// Project root directory
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        /// Restrict to files starting with this prefix (e.g. `src/click/`).
        #[arg(long)]
        path: Option<String>,
        /// Restrict to entities of these kinds (repeatable, or comma-
        /// separated). Useful for matching `grep -n "^class "` one-liners
        /// exactly — e.g. `--kind class` drops top-level functions and
        /// module-level helpers that bloat the payload on outline-shaped
        /// questions. Default: all outline-eligible kinds (classes +
        /// top-level functions + structs + enums + traits).
        #[arg(long, value_delimiter = ',')]
        kind: Vec<String>,
        /// Output format: markdown (default) or json.
        #[arg(long, default_value = "markdown")]
        format: String,
        /// Pretty-print when --format=json.
        #[arg(long)]
        pretty: bool,
    },
    /// Single-shot definition locator — where is `<symbol>` defined?
    ///
    /// Returns one record per definition (class / method / function /
    /// struct / enum / trait / type alias), deduped across Python
    /// @overload stubs, with signature preview + inheritance siblings.
    /// Intended as the first-call "find the relevant code" primitive
    /// for agents on unfamiliar codebases.
    Where {
        /// Symbol name to locate. Matches on the last `::` or `.`-
        /// separated segment, so `get_default` matches both
        /// `Parameter.get_default` and `Option.get_default` but NOT
        /// `CliRunner.get_default_prog_name`.
        symbol: String,
        /// Project root directory
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        /// Include definitions that live under typical test paths.
        /// Off by default — test files dilute a "find the implementation"
        /// answer.
        #[arg(long)]
        include_tests: bool,
        /// Only return definitions whose enclosing class/module matches
        /// NAME exactly. Matches against both the raw parent and its
        /// tail segment, so `--parent ModelChoiceField` works even when
        /// the index stores `django.forms.models.ModelChoiceField`. Pass
        /// an empty string (`--parent ""`) to require top-level only.
        #[arg(long, value_name = "NAME")]
        parent: Option<String>,
        /// Only return definitions whose file path contains SUBSTR.
        /// Useful when many hits are scattered across a monorepo.
        #[arg(long, value_name = "SUBSTR")]
        file: Option<String>,
        /// Cap on rows returned, ordered by file-rank desc. 0 = no cap.
        /// When the cap hits, stderr gets a one-line "narrow" hint.
        #[arg(long, default_value_t = sigil::where_cmd::DEFAULT_LIMIT)]
        limit: usize,
        /// Output format: markdown (default) or json.
        #[arg(long, default_value = "markdown")]
        format: String,
        /// Pretty-print when --format=json.
        #[arg(long)]
        pretty: bool,
    },
    /// Impact summary for a symbol — blast counts + top callers by file rank.
    Blast {
        /// Symbol name.
        symbol: String,
        /// Project root directory
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        /// How many top callers to surface. 0 = all.
        #[arg(long, default_value = "10")]
        depth: usize,
        /// Output format: markdown (default), json, or agent (compact JSON).
        #[arg(long, default_value = "markdown")]
        format: String,
        /// Pretty-print when --format=json.
        #[arg(long)]
        pretty: bool,
        /// Drop test-file callers and test-file candidates.
        #[arg(long)]
        exclude_tests: bool,
    },
    /// Clone report — groups entities by body_hash to surface duplicated code.
    Duplicates {
        /// Project root directory
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        /// Ignore entities whose body is fewer than this many lines.
        #[arg(long, default_value = "3")]
        min_lines: u32,
        /// Drop groups larger than this (likely auto-generated). 0 = no cap.
        #[arg(long, default_value = "0")]
        max_group_size: usize,
        /// Output format: markdown (default) or json.
        #[arg(long, default_value = "markdown")]
        format: String,
        /// Pretty-print when --format=json.
        #[arg(long)]
        pretty: bool,
    },
    /// Execute ad-hoc SQL against the DuckDB-materialized sigil index.
    /// Requires `--features db` at build time. Power-user escape hatch
    /// for analytics the built-in commands don't cover.
    Query {
        /// SQL statement. The `entities` and `refs` tables are
        /// populated from `.sigil/entities.jsonl` and `.sigil/refs.jsonl`.
        sql: String,
        /// Project root directory
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        /// Output format: markdown (default) or json.
        #[arg(long, default_value = "markdown")]
        format: String,
        /// Truncate each cell to this many chars in markdown output.
        /// 0 = no truncation.
        #[arg(long, default_value = "60")]
        max_cell_width: usize,
        /// Pretty-print when --format=json.
        #[arg(long)]
        pretty: bool,
    },
    /// Token-reduction benchmark: sigil commands vs raw alternatives.
    Benchmark {
        /// Project root directory
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        /// Git refspec for the PR-review query.
        #[arg(long, default_value = "HEAD~1..HEAD")]
        refspec: String,
        /// Symbol for the context query. Defaults to the highest-blast entity.
        #[arg(long)]
        symbol: Option<String>,
        /// Output format: markdown (default) or json.
        #[arg(long, default_value = "markdown")]
        format: String,
        /// Pretty-print when --format=json.
        #[arg(long)]
        pretty: bool,
        /// Token counter. `proxy` (default) is the zero-dep bytes/4
        /// heuristic. `cl100k_base`, `o200k_base`, `p50k_base` require
        /// the `tokenizer` cargo feature and give BPE-accurate counts.
        #[arg(long, default_value = "proxy")]
        tokenizer: String,
    },
    /// PR review artifact — structural diff enriched with rank, blast
    /// radius, and co-change misses. Reviewer reads this instead of
    /// `git diff`.
    Review {
        /// Ref spec: HEAD~1, main..HEAD, abc123..def456
        ref_spec: String,
        /// Project root directory
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        /// Output format: markdown (default) or json.
        #[arg(long, default_value = "markdown")]
        format: String,
        /// Max entries in the "Most impactful" section.
        #[arg(long, default_value = "5")]
        top_k: usize,
        /// Skip co-change miss detection.
        #[arg(long)]
        no_cochange: bool,
    },
    /// Build / refresh the co-change cache (`.sigil/cochange.json`).
    /// Reads `git log --name-only` and weights file pairs that move together.
    Cochange {
        /// Project root directory
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        /// Number of historical commits to walk.
        #[arg(long, default_value = "500")]
        commits: u32,
        /// Drop pairs with fewer than this many co-occurrences.
        #[arg(long, default_value = "2")]
        min_support: u32,
        /// Ignore commits that touch more than this many files.
        #[arg(long, default_value = "30")]
        max_files_per_commit: u32,
        /// Pretty-print the JSON output.
        #[arg(long)]
        pretty: bool,
        /// Workspace mode: scan this parent directory for child git repos
        /// and emit cross-repo file pairs that change in the same time
        /// window. JSONL on stdout (one edge per row). Bypasses the
        /// per-repo `.sigil/cochange.json` cache.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Workspace mode: max seconds between two commits for them to
        /// count as temporally-correlated.
        #[arg(long, default_value = "86400")]
        workspace_window_secs: i64,
    },
    /// Minimum-viable context for a symbol — signature, callers, callees,
    /// related types. One call replaces the read-6-files orientation loop.
    Context {
        /// Symbol name, or qualified form like `file::name`,
        /// `Parent::name`, `file::Parent::name`.
        query: String,
        /// Project root directory
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        /// Output token cap. 0 = unlimited.
        #[arg(long, default_value = "1500")]
        budget: usize,
        /// How many callers / callees / related types to show per section.
        #[arg(long, default_value = "10")]
        depth: usize,
        /// Output format: `markdown` (default), `agent` (compact JSON for
        /// LLM ingestion), `json` / `full` (structured JSON).
        #[arg(long, default_value = "markdown")]
        format: String,
        /// Pretty-print when --format=json. Ignored otherwise.
        #[arg(long)]
        pretty: bool,
        /// Drop test-file candidates and test-file callers from the bundle.
        #[arg(long)]
        exclude_tests: bool,
        /// Also include the symbol's source body (lines line_start..=line_end)
        /// inline in the bundle. Saves a follow-up `read_file` in the common
        /// "locate then read" pattern. Off by default — bodies are large.
        #[arg(long)]
        with_body: bool,
    },
    /// Budget-aware ranked digest of the codebase — drop into an agent's
    /// context for cold-start orientation.
    Map {
        /// Project root directory
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        /// Token budget. 0 = unlimited.
        #[arg(long, default_value = "4000")]
        tokens: usize,
        /// Boost entities under this path prefix so the digest centers on
        /// that subtree (useful for subsystem-focused runs).
        #[arg(long)]
        focus: Option<String>,
        /// Max entities surfaced per file.
        #[arg(long, default_value = "5")]
        depth: usize,
        /// Output format: markdown (default) or json.
        #[arg(long, default_value = "markdown")]
        format: String,
        /// Also write the Markdown form to .sigil/SIGIL_MAP.md for the
        /// agent-platform hook installers to point at.
        #[arg(long)]
        write: bool,
        /// Drop test-file entities (matching `tests/`, `*_test.rs`,
        /// `*.spec.ts`, etc.) from the map output.
        #[arg(long)]
        exclude_tests: bool,
        /// Skip the community-detection pass. Default is to include a
        /// `## Subsystems` section in the markdown output.
        #[arg(long)]
        no_clusters: bool,
        /// When > 0, attach this many top entities (full `code.context`
        /// bundle) to each subsystem in the JSON / Markdown output.
        /// Collapses the "list subsystem files → list entities → call
        /// `code.context` per entity" N+1 pattern into a single map call.
        /// 0 (default) preserves the legacy shape.
        #[arg(long, default_value = "0")]
        top_entities_per_subsystem: usize,
    },
    /// Install or uninstall the Claude Code integration
    /// (CLAUDE.md capability block + PreToolUse hint hook).
    Claude {
        #[command(subcommand)]
        action: InstallAction,
    },
    /// Install or uninstall the Cursor integration
    /// (`.cursor/rules/sigil.mdc` with `alwaysApply: true`).
    Cursor {
        #[command(subcommand)]
        action: InstallAction,
    },
    /// Install or uninstall the Codex integration
    /// (`AGENTS.md` capability block + `.codex/hooks.json` Bash hint hook).
    Codex {
        #[command(subcommand)]
        action: InstallAction,
    },
    /// Install or uninstall the Gemini CLI integration
    /// (`GEMINI.md` capability block + `.gemini/settings.json` BeforeTool hint hook).
    Gemini {
        #[command(subcommand)]
        action: InstallAction,
    },
    /// Install or uninstall the OpenCode integration
    /// (`AGENTS.md` + `.opencode/plugins/sigil.js` + `opencode.json`).
    Opencode {
        #[command(subcommand)]
        action: InstallAction,
    },
    /// Install or uninstall the Aider integration (`AGENTS.md` block).
    Aider {
        #[command(subcommand)]
        action: InstallAction,
    },
    /// Install or uninstall the GitHub Copilot CLI skill
    /// (`~/.copilot/skills/sigil/SKILL.md`).
    Copilot {
        #[command(subcommand)]
        action: InstallAction,
    },
    /// Install git hooks (post-commit + post-checkout) that auto-rebuild
    /// the sigil index in the background.
    Hook {
        #[command(subcommand)]
        action: InstallAction,
    },
    /// Update sigil to the latest release
    Update,
    /// Extract symbol-shaped identifiers from arbitrary text.
    ///
    /// Deterministic regex-based extractor for CamelCase, snake_case, and
    /// dotted-path tokens (e.g. `NearestCentroid`, `_local_density`,
    /// `Class::method`). Used by retrieval pipelines that want to join a
    /// natural-language question against indexed entity names. JSON array
    /// of strings on stdout.
    Identifiers {
        /// Source text. Pass on the command line or via stdin (use `-`).
        text: String,
    },
    /// Extract architectural-decision markers from source-file comments.
    ///
    /// Scans for `# DECISION:`, `# WHY:`, `# RATIONALE:`, `# TRADEOFF:`
    /// (and `//` / `--` comment-style equivalents) anchors in source. Emits
    /// one JSONL row per match — designed to feed the Knova runner's
    /// decision intelligence layer.
    Decisions {
        /// Project root directory
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
    },
    /// Extract package dependency edges from manifest files.
    ///
    /// Currently supports `go.mod`. Emits one JSONL row per (manifest,
    /// dependency, version) edge. Used by workspace-mode tooling to
    /// detect cross-repo dependency relationships without an LLM call.
    #[command(name = "package-deps")]
    PackageDeps {
        /// Project root directory
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
    },
    /// Extract HTTP / gRPC / queue contract entries (providers + consumers)
    /// from source code.
    ///
    /// Used by workspace-mode tooling to match a route handler in one repo
    /// against the HTTP client that calls it in another, without an LLM
    /// call. MVP covers FastAPI HTTP providers; more frameworks land
    /// incrementally.
    Contracts {
        /// Project root directory
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
    },
    /// Coordinator over multiple git repos under a parent directory.
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
    /// Rank files by commit-count churn × line-count complexity.
    /// JSONL on stdout: { file, churn, lines, hotspot_score } sorted desc.
    Hotspots {
        /// Project root (must be a git repo)
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        /// How many recent commits to consider for churn.
        #[arg(long, default_value = "500")]
        commits: usize,
    },
    /// Per-file ownership from git log. JSONL on stdout:
    /// { file, primary_owner, ownership_pct, author_count, commit_count }.
    Ownership {
        /// Project root (must be a git repo)
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        /// How many recent commits to walk.
        #[arg(long, default_value = "500")]
        commits: usize,
    },
    /// Regex-based security signal scan. JSONL on stdout:
    /// { file, line, kind, severity }.
    #[command(name = "security-scan")]
    SecurityScan {
        /// Project root directory
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
    },
    /// Heritage graph for a symbol — outgoing edges (this symbol embeds /
    /// extends / implements X) and incoming edges (Y embeds / extends /
    /// implements this symbol).
    ///
    /// Currently only Go struct embedding populates the graph; future
    /// extractors will add class extension, interface implementation, and
    /// trait impl edges through the same schema.
    Heritage {
        /// Symbol name to query. Matches the entity's bare name, or the
        /// tail segment of a `pkg.Foo`-shaped heritage target.
        symbol: String,
        /// Project root directory
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        /// Pretty-print the JSON output.
        #[arg(long)]
        pretty: bool,
    },
}

#[derive(Subcommand)]
enum WorkspaceAction {
    /// List child git repos under the workspace root. Emits one JSONL row
    /// per repo with { repo, path }. Used by callers that iterate the
    /// workspace and run per-repo primitives (decisions, contracts,
    /// package-deps, ...).
    Scan {
        /// Workspace root directory (parent of the child git repos)
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
    },
}

#[derive(Subcommand)]
enum InstallAction {
    Install {
        /// Project root directory
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
    },
    Uninstall {
        /// Project root directory
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();

    match cli {
        Cli::Index { root, files, stdout, pretty, full, no_refs, no_rank, verbose } => {
            let files_arg = if files.is_empty() { None } else { Some(files.as_slice()) };
            let mut result = index::build_index(&root, files_arg, full, !no_refs, verbose);

            // Phase 1 rank pass. On by default; `--no-rank` skips it (useful
            // in CI or when refs are also skipped). Rank is a whole-repo
            // computation — a changed subset of files still re-ranks globally
            // because cross-file references affect the graph.
            let rank_manifest = if !no_rank && !result.refs.is_empty() {
                let cfg = sigil::rank::RankConfig::default();
                let ranked = sigil::rank::rank_with_config(&result.entities, &result.refs, &cfg);
                sigil::rank::apply_blast_radius(&mut result.entities, &ranked);
                Some(sigil::rank::RankManifest::from_ranked(&ranked, &cfg))
            } else {
                // If the user opted out, also wipe any stale rank/blast_radius
                // that cached entities carried over from a previous run — the
                // on-disk output should reflect the requested mode.
                for e in &mut result.entities {
                    e.rank = None;
                    e.blast_radius = None;
                }
                None
            };

            if stdout {
                let out = std::io::stdout();
                let mut out = out.lock();
                writer::write_entities_jsonl(&result.entities, &mut out, pretty)
                    .expect("Failed to write to stdout");
                // Write refs to stderr in stdout mode to avoid mixing
                if !result.refs.is_empty() {
                    let err = std::io::stderr();
                    let mut err = err.lock();
                    writer::write_refs_jsonl(&result.refs, &mut err, pretty)
                        .expect("Failed to write refs to stderr");
                }
                // rank.json is a project-level artifact; we don't emit it on
                // stdout. blast_radius is already on each entity above.
            } else {
                writer::write_to_files(&result.entities, &result.refs, &root, pretty)
                    .expect("Failed to write index");
                match &rank_manifest {
                    Some(m) => writer::write_rank_json(m, &root, pretty)
                        .expect("Failed to write rank.json"),
                    None => {
                        // Clean up any stale rank.json from a prior run when
                        // the user explicitly disables ranking.
                        let _ = writer::remove_rank_json(&root);
                    }
                }
                if verbose {
                    let rank_note = match &rank_manifest {
                        Some(m) => format!(", {} files ranked", m.file_count),
                        None => " (rank skipped)".to_string(),
                    };
                    eprintln!(
                        "Wrote {} entities and {} refs to .sigil/{}",
                        result.entities.len(),
                        result.refs.len(),
                        rank_note
                    );
                }
            }
        }
        Cli::Diff { ref_spec, files, root, json, pretty, verbose, lines, context, no_context, markdown, no_emoji, no_color, no_callers, summary, group } => {
            // Handle --no-color
            if no_color {
                colored::control::set_override(false);
            }

            // Compute diff result
            let include_context = !no_context;
            let context_lines = context;
            let result = if files.len() == 2 {
                let opts = diff::DiffOptions { include_unchanged: false, verbose, include_context, context_lines };
                diff::compute_file_diff(&files[0], &files[1], &opts)
                    .unwrap_or_else(|e| { eprintln!("error: {}", e); std::process::exit(3); })
            } else {
                let ref_spec = ref_spec.unwrap();
                let (base_ref, head_ref) = git::parse_ref_spec(&ref_spec)
                    .unwrap_or_else(|e| { eprintln!("error: {}", e); std::process::exit(3); });
                let opts = diff::DiffOptions { include_unchanged: false, verbose, include_context, context_lines };
                diff::compute_diff(&root, &base_ref, &head_ref, &opts)
                    .unwrap_or_else(|e| { eprintln!("error: {}", e); std::process::exit(3); })
            };

            // Build DiffOutput
            let mut output = output::DiffOutput::from_result(&result, include_context, context_lines);
            if !summary {
                output.summary.summary_line = None;
            }

            // Caller analysis for breaking changes
            if !no_callers && output.summary.has_breaking {
                // Collect files touched by the diff
                let diff_files: std::collections::HashSet<String> = output.files.iter()
                    .map(|f| f.file.clone())
                    .collect();

                // Try to load index for caller queries
                match query::load(&root) {
                    Ok(idx) => {
                        let callers_fn = |name: &str| -> Vec<(String, u32, String)> {
                            idx.get_callers(name, None, 100)
                                .into_iter()
                                .map(|r| (r.file.clone(), r.line, r.caller.clone().unwrap_or_default()))
                                .collect()
                        };
                        output::enrich_breaking_with_callers(&mut output.breaking, &callers_fn, &diff_files);
                    }
                    Err(_) => {
                        // Index not available — skip caller analysis silently
                        if verbose {
                            eprintln!("note: run `sigil index` to enable caller impact analysis");
                        }
                    }
                }
            }

            // Compute groups if --group flag is set
            if group {
                output.groups = Some(grouping::compute_groups(&output));
            }

            // Dispatch to formatter
            if json {
                let out = std::io::stdout();
                let mut out = out.lock();
                if pretty {
                    serde_json::to_writer_pretty(&mut out, &output)
                } else {
                    serde_json::to_writer(&mut out, &output)
                }.expect("Failed to write JSON");
                println!();
            } else if markdown {
                let opts = markdown_formatter::MarkdownOptions {
                    use_emoji: !no_emoji,
                    show_context: include_context,
                };
                print!("{}", markdown_formatter::format_markdown(&output, &opts));
            } else {
                let opts = formatter::FormatOptions {
                    show_lines: lines,
                    show_context: include_context,
                    use_color: !no_color,
                };
                print!("{}", formatter::format_terminal_v2(&output, &opts));
            }

        }
        Cli::Explore { root, path, max_entries, json, pretty } => {
            let backend = sigil::query::Backend::load(&root)
                .unwrap_or_else(|e| { eprintln!("error: {}", e); std::process::exit(1); });
            if json {
                let files = backend.explore_files_capped(path.as_deref(), max_entries);
                let values: Vec<serde_json::Value> = files.iter().map(|(dir, path, lang)| {
                    serde_json::json!({"directory": dir, "path": path, "language": lang})
                }).collect();
                let stdout = std::io::stdout();
                let mut lock = stdout.lock();
                if pretty {
                    serde_json::to_writer_pretty(&mut lock, &values).ok();
                } else {
                    serde_json::to_writer(&mut lock, &values).ok();
                }
                println!();
            } else {
                let dirs = backend.explore_dir_overview(path.as_deref());
                if dirs.is_empty() {
                    print!("No files found.\n");
                } else {
                    let visible = dirs.len().max(1);
                    let cap = (max_entries / visible).max(1);
                    let files = backend.explore_files_capped(path.as_deref(), cap);
                    print!("{}", sigil::query::render_explore(&dirs, &files));
                }
            }
        }
        Cli::Search { query: q, root, scope, kind, path, limit, json, pretty } => {
            let original_q = q.clone();
            let backend = sigil::query::Backend::load(&root)
                .unwrap_or_else(|e| { eprintln!("error: {}", e); std::process::exit(1); });
            // `scope` / `kind` are Vec<String> for CLI multi-arg compatibility;
            // first value wins for filtering (matches codeix's prior behavior).
            // Default is "symbol" (see the CLI arg definition) — agents want
            // symbol hits almost all the time.
            let scope_enum = scope
                .first()
                .map(|s| query::index::Scope::parse(s))
                .unwrap_or(query::index::Scope::Symbols);
            let kind_filter = kind.first().map(|s| s.as_str());
            let results = backend.search(&q, scope_enum, kind_filter, path.as_deref(), limit as usize);
            if json {
                // Compact schema — one row per unique (file, name, kind).
                // Repeated hits with the same key (Python @overload stubs,
                // parent=None variables that duplicate a method entry) get
                // collapsed into a single row with `overloads: N` so the
                // agent sees "this method exists here" without skimming
                // 3-5 near-identical rows.
                //
                // Elision rules, matching the rest of the 0.4.0 compact
                // output:
                //   - `type: "symbol"` dropped (implied by --scope symbol,
                //     the new default). File hits keep `type: "file"` so
                //     mixed-scope results stay discriminable.
                //   - `line_end` elided when equal to `line`.
                //   - `parent` elided when null.
                //   - `overloads` elided when 1.
                use std::collections::BTreeMap;
                let mut order: Vec<(String, String, String)> = Vec::new();
                // (line_start, line_end, parent, overloads, sig_preview)
                let mut groups: BTreeMap<
                    (String, String, String),
                    (u32, u32, Option<String>, u32, Option<String>),
                > = BTreeMap::new();
                let mut file_rows: Vec<serde_json::Value> = Vec::new();
                for h in &results {
                    match h {
                        sigil::query::SearchHitOwned::Symbol(e) => {
                            let key = (e.file.clone(), e.name.clone(), e.kind.clone());
                            groups
                                .entry(key.clone())
                                .and_modify(|(_, _, _, n, _)| *n += 1)
                                .or_insert_with(|| {
                                    order.push(key);
                                    (e.line_start, e.line_end, e.parent.clone(), 1, e.sig.clone())
                                });
                        }
                        sigil::query::SearchHitOwned::File(f) => {
                            let mut row = serde_json::json!({
                                "type": "file",
                                "path": f.path,
                                "lang": f.lang,
                            });
                            if f.entity_count > 0 {
                                row["entity_count"] = serde_json::json!(f.entity_count);
                            }
                            file_rows.push(row);
                        }
                    }
                }

                let mut json_hits: Vec<serde_json::Value> = Vec::with_capacity(order.len() + file_rows.len());
                for key in order {
                    let (line_start, line_end, parent, overloads, sig) = groups[&key].clone();
                    let (file, name, kind) = key;
                    let mut row = serde_json::Map::new();
                    row.insert("file".into(), serde_json::Value::from(file));
                    row.insert("name".into(), serde_json::Value::from(name));
                    row.insert("kind".into(), serde_json::Value::from(kind));
                    row.insert("line".into(), serde_json::Value::from(line_start));
                    if line_end != line_start {
                        row.insert("line_end".into(), serde_json::Value::from(line_end));
                    }
                    if let Some(p) = parent {
                        row.insert("parent".into(), serde_json::Value::from(p));
                    }
                    // Signature preview saves a follow-up read_file. ~50-120 extra
                    // bytes per row, but typically eliminates a 2-5 KB file read.
                    if let Some(s) = sig {
                        if !s.is_empty() {
                            row.insert("sig".into(), serde_json::Value::from(s));
                        }
                    }
                    if overloads > 1 {
                        row.insert("overloads".into(), serde_json::Value::from(overloads));
                    }
                    json_hits.push(serde_json::Value::Object(row));
                }
                json_hits.extend(file_rows);

                let stdout = std::io::stdout();
                let mut lock = stdout.lock();
                if pretty {
                    serde_json::to_writer_pretty(&mut lock, &json_hits).ok();
                } else {
                    serde_json::to_writer(&mut lock, &json_hits).ok();
                }
                println!();
            } else {
                print!("{}", query::format_search_hits_owned(&results));
            }
            if results.is_empty() {
                let idx = query::load(&root).ok();
                let sugg = idx
                    .as_ref()
                    .map(|i| query::suggest_similar(i, &original_q, 5))
                    .unwrap_or_default();
                if sugg.is_empty() {
                    eprintln!(
                        "sigil: 0 matches for `{original_q}`. Try a shorter substring (`sigil search {}`) or `sigil where <exact-name>` for a definition lookup.",
                        first_few(&original_q, 4),
                    );
                } else {
                    eprintln!(
                        "sigil: 0 matches for `{original_q}`. Did you mean: {}?",
                        sugg.join(", ")
                    );
                }
            }
        }
        Cli::Symbols { file, root, limit, depth, json, pretty, names_only, with_hashes } => {
            let backend = sigil::query::Backend::load(&root)
                .unwrap_or_else(|e| { eprintln!("error: {}", e); std::process::exit(1); });
            let symbols = backend.get_file_symbols(&file, None, limit as usize);
            let was_empty_before_depth = symbols.is_empty();
            let filtered: Vec<sigil::entity::Entity> = if depth == 1 {
                symbols.into_iter().filter(query::is_top_level_outline).collect()
            } else {
                symbols
            };
            let refs: Vec<&sigil::entity::Entity> = filtered.iter().collect();
            if names_only {
                // Flat JSON array of tail-segment names — the minimum
                // payload for "list the Xs in this file" questions.
                let names: Vec<String> = refs
                    .iter()
                    .map(|e| query::tail_segment(&e.name).to_string())
                    .collect();
                let stdout = std::io::stdout();
                let mut lock = stdout.lock();
                if pretty {
                    serde_json::to_writer_pretty(&mut lock, &names).ok();
                } else {
                    serde_json::to_writer(&mut lock, &names).ok();
                }
                println!();
            } else if json {
                query::emit_entities_json(std::io::stdout(), &refs, pretty, with_hashes).ok();
            } else {
                print!("{}", query::format_entities(&refs));
            }
            if refs.is_empty() {
                if was_empty_before_depth {
                    eprintln!(
                        "sigil: no entities in `{file}`. File may not be indexed — try `sigil index` or check the path spelling."
                    );
                } else {
                    eprintln!(
                        "sigil: `{file}` has entries, but none qualify as `--depth 1` top-level items. Re-run without `--depth 1` to see nested / variable / import entries."
                    );
                }
            }
        }
        Cli::Children { file, parent, root, limit, json, pretty, with_hashes } => {
            let backend = sigil::query::Backend::load(&root)
                .unwrap_or_else(|e| { eprintln!("error: {}", e); std::process::exit(1); });
            let children = backend.get_children(&file, &parent, None, limit as usize);
            let refs: Vec<&sigil::entity::Entity> = children.iter().collect();
            if json {
                query::emit_entities_json(std::io::stdout(), &refs, pretty, with_hashes).ok();
            } else {
                print!("{}", query::format_entities(&refs));
            }
        }
        Cli::Callers { name, root, kind, limit, group_by, json, pretty } => {
            let backend = sigil::query::Backend::load(&root)
                .unwrap_or_else(|e| { eprintln!("error: {}", e); std::process::exit(1); });
            let refs = backend.get_callers(&name, kind.as_deref(), limit as usize);
            let borrowed: Vec<&sigil::entity::Reference> = refs.iter().collect();
            if let Some(dim) = group_by.as_deref() {
                query::emit_refs_grouped(std::io::stdout(), &borrowed, dim, pretty).unwrap_or_else(|e| {
                    eprintln!("error: {}", e);
                    std::process::exit(1);
                });
            } else if json {
                query::emit_references_json(std::io::stdout(), &borrowed, pretty).ok();
            } else {
                print!("{}", query::format_refs(&borrowed));
            }
            if borrowed.is_empty() {
                emit_empty_hint(&root, &name, "callers");
            }
        }
        Cli::Callees { caller, root, kind, limit, group_by, json, pretty } => {
            let backend = sigil::query::Backend::load(&root)
                .unwrap_or_else(|e| { eprintln!("error: {}", e); std::process::exit(1); });
            let refs = backend.get_callees(&caller, kind.as_deref(), limit as usize);
            let borrowed: Vec<&sigil::entity::Reference> = refs.iter().collect();
            if let Some(dim) = group_by.as_deref() {
                query::emit_refs_grouped(std::io::stdout(), &borrowed, dim, pretty).unwrap_or_else(|e| {
                    eprintln!("error: {}", e);
                    std::process::exit(1);
                });
            } else if json {
                query::emit_references_json(std::io::stdout(), &borrowed, pretty).ok();
            } else {
                print!("{}", query::format_refs(&borrowed));
            }
            if borrowed.is_empty() {
                emit_empty_hint(&root, &caller, "callees");
            }
        }
        Cli::Grep {
            pattern,
            root,
            ignore_case,
            word,
            fixed_strings,
            file,
            glob,
            class,
            caller,
            limit,
            group_by,
            no_entity,
            format,
            pretty,
        } => {
            let idx = query::load(&root)
                .unwrap_or_else(|e| { eprintln!("error: {}", e); std::process::exit(1); });
            let group_by = match group_by.as_deref() {
                None => None,
                Some(s) => match sigil::grep_cmd::GroupBy::parse(s) {
                    Some(g) => Some(g),
                    None => {
                        eprintln!("error: unknown --group-by `{}`. expected: file | class | entity | kind", s);
                        std::process::exit(1);
                    }
                },
            };
            let opts = sigil::grep_cmd::GrepOptions {
                pattern,
                case_insensitive: ignore_case,
                word_match: word,
                fixed_strings,
                file_filter: file,
                globs: glob,
                class_filter: class,
                caller_filter: caller,
                limit,
                no_entity,
                group_by,
            };
            let report = match sigil::grep_cmd::run_grep(&root, &idx, &opts) {
                Ok(r) => r,
                Err(e) => { eprintln!("error: {}", e); std::process::exit(1); }
            };
            match format.as_str() {
                "text" => print!("{}", sigil::grep_cmd::render_text(&report)),
                "json" => println!("{}", sigil::grep_cmd::render_json(&report, pretty)),
                other => {
                    eprintln!("error: unknown --format `{}`. expected: text | json", other);
                    std::process::exit(1);
                }
            }
            if report.total_hits == 0 {
                std::process::exit(1);
            }
        }
        Cli::Outline { root, path, kind, format, pretty } => {
            let idx = query::load(&root)
                .unwrap_or_else(|e| { eprintln!("error: {}", e); std::process::exit(1); });
            let report = sigil::outline::build_outline(&idx, path.as_deref(), &kind);
            match format.as_str() {
                "markdown" => print!("{}", sigil::outline::render_markdown(&report)),
                "json" => println!("{}", sigil::outline::render_json(&report, pretty)),
                other => {
                    eprintln!("error: unknown --format {}. expected: markdown | json", other);
                    std::process::exit(1);
                }
            }
        }
        Cli::Where { symbol, root, include_tests, parent, file, limit, format, pretty } => {
            let idx = query::load(&root)
                .unwrap_or_else(|e| { eprintln!("error: {}", e); std::process::exit(1); });
            let filters = sigil::where_cmd::WhereFilters {
                parent,
                file,
                include_tests,
            };
            let report = sigil::where_cmd::find_definitions(&idx, &symbol, &filters, limit);
            match format.as_str() {
                "markdown" => print!("{}", sigil::where_cmd::render_markdown(&report)),
                "json" => println!("{}", sigil::where_cmd::render_json(&report, pretty)),
                other => {
                    eprintln!("error: unknown --format {}. expected: markdown | json", other);
                    std::process::exit(1);
                }
            }
            if report.definitions.is_empty() {
                let sugg = query::suggest_similar(&idx, &symbol, 5);
                if sugg.is_empty() {
                    eprintln!(
                        "sigil: no definition of `{symbol}` found. Try `sigil search {}` with a shorter substring.",
                        first_few(&symbol, 4),
                    );
                } else {
                    eprintln!(
                        "sigil: no definition of `{symbol}` found. Did you mean: {}?",
                        sugg.join(", ")
                    );
                }
            } else if let Some(hint) = sigil::where_cmd::narrow_hint(&report) {
                eprintln!("{}", hint);
            }
        }
        Cli::Blast { symbol, root, depth, format, pretty, exclude_tests } => {
            let idx = query::load(&root)
                .unwrap_or_else(|e| { eprintln!("error: {}", e); std::process::exit(1); });
            let rank = sigil::map::load_rank_manifest(&root).unwrap_or_default();
            let Some(fmt) = sigil::blast::BlastFormat::parse(&format) else {
                eprintln!("error: unknown --format {}. expected markdown|json|agent", format);
                std::process::exit(1);
            };
            let opts = sigil::blast::BlastOptions {
                depth,
                format: fmt,
                exclude_tests,
            };
            let Some(report) = sigil::blast::run_blast(&idx, &rank, &symbol, &opts) else {
                eprintln!("no entity named `{}` (skipping imports)", symbol);
                eprintln!("hint: try `sigil search {}` to find similar symbols", symbol);
                std::process::exit(2);
            };
            match fmt {
                sigil::blast::BlastFormat::Markdown => print!("{}", sigil::blast::render_markdown(&report)),
                sigil::blast::BlastFormat::Json => println!("{}", sigil::blast::render_json(&report, pretty)),
                sigil::blast::BlastFormat::Agent => println!("{}", sigil::blast::render_agent(&report)),
            }
        }
        Cli::Duplicates { root, min_lines, max_group_size, format, pretty } => {
            let idx = query::load(&root)
                .unwrap_or_else(|e| { eprintln!("error: {}", e); std::process::exit(1); });
            let Some(fmt) = sigil::duplicates::DuplicatesFormat::parse(&format) else {
                eprintln!("error: unknown --format {}. expected markdown|json", format);
                std::process::exit(1);
            };
            let opts = sigil::duplicates::DuplicatesOptions {
                min_lines,
                max_group_size,
                format: fmt,
                ..sigil::duplicates::DuplicatesOptions::default()
            };
            let report = sigil::duplicates::find_duplicates(&idx, &opts);
            match fmt {
                sigil::duplicates::DuplicatesFormat::Markdown => {
                    print!("{}", sigil::duplicates::render_markdown(&report));
                }
                sigil::duplicates::DuplicatesFormat::Json => {
                    println!("{}", sigil::duplicates::render_json(&report, pretty));
                }
            }
        }
        Cli::Query { sql, root, format, max_cell_width, pretty } => {
            run_query(&sql, &root, &format, max_cell_width, pretty);
        }
        Cli::Benchmark { root, refspec, symbol, format, pretty, tokenizer } => {
            let Some(fmt) = sigil::benchmark::BenchmarkFormat::parse(&format) else {
                eprintln!("error: unknown --format {}. expected markdown|json", format);
                std::process::exit(1);
            };
            let Some(tok) = sigil::tokens::Tokenizer::parse(&tokenizer) else {
                eprintln!("error: unknown --tokenizer {}. expected proxy|cl100k_base|o200k_base|p50k_base", tokenizer);
                std::process::exit(1);
            };
            let opts = sigil::benchmark::BenchmarkOptions {
                refspec,
                symbol,
                format: fmt,
                tokenizer: tok,
            };
            match sigil::benchmark::run_benchmark(&root, &opts) {
                Ok(report) => match fmt {
                    sigil::benchmark::BenchmarkFormat::Markdown => {
                        print!("{}", sigil::benchmark::render_markdown(&report));
                    }
                    sigil::benchmark::BenchmarkFormat::Json => {
                        println!("{}", sigil::benchmark::render_json(&report, pretty));
                    }
                },
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Cli::Review { ref_spec, root, format, top_k, no_cochange } => {
            let Some(fmt) = sigil::review::ReviewFormat::parse(&format) else {
                eprintln!("error: unknown --format {}. expected `markdown` or `json`", format);
                std::process::exit(1);
            };
            let opts = sigil::review::ReviewOptions {
                format: fmt,
                top_k,
                show_cochange: !no_cochange,
                ..sigil::review::ReviewOptions::default()
            };
            match sigil::review::run_review(&root, &ref_spec, &opts) {
                Ok(rendered) => {
                    if matches!(fmt, sigil::review::ReviewFormat::Json) {
                        println!("{}", rendered);
                    } else {
                        print!("{}", rendered);
                    }
                }
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Cli::Cochange { root, commits, min_support, max_files_per_commit, pretty, workspace, workspace_window_secs } => {
            // Workspace mode short-circuits the per-repo path: cross-repo
            // edges over child git repos under `workspace`, JSONL on stdout.
            if let Some(parent) = workspace {
                let cfg = sigil::cross_repo_cochange::CrossRepoConfig {
                    window_secs: workspace_window_secs,
                    commits_per_repo: commits as usize,
                    min_strength: 0.0,
                };
                match sigil::cross_repo_cochange::mine(&parent, &cfg) {
                    Ok(edges) => {
                        for edge in edges {
                            match serde_json::to_string(&edge) {
                                Ok(s) => println!("{}", s),
                                Err(e) => {
                                    eprintln!("cochange: failed to serialize: {}", e);
                                    std::process::exit(1);
                                }
                            }
                        }
                        return;
                    }
                    Err(e) => {
                        eprintln!("cochange --workspace: {}", e);
                        std::process::exit(1);
                    }
                }
            }
            let cfg = sigil::cochange::CochangeConfig { commits, min_support, max_files_per_commit };
            match sigil::cochange::mine(&root, &cfg) {
                Ok(manifest) => {
                    if let Err(e) = sigil::cochange::save(&manifest, &root, pretty) {
                        eprintln!("error writing .sigil/cochange.json: {}", e);
                        std::process::exit(1);
                    }
                    eprintln!(
                        "scanned {} commits, {} files, {} pairs (min_support={})",
                        manifest.commits_scanned,
                        manifest.file_count,
                        manifest.pairs.len(),
                        manifest.min_support,
                    );
                }
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Cli::Context { query: q, root, budget, depth, format, pretty, exclude_tests, with_body } => {
            let idx = query::load(&root)
                .unwrap_or_else(|e| { eprintln!("error: {}", e); std::process::exit(1); });
            let Some(fmt) = sigil::context::ContextFormat::parse(&format) else {
                eprintln!("error: unknown --format {}. expected `markdown`, `agent`, or `json`", format);
                std::process::exit(1);
            };
            let opts = sigil::context::ContextOptions {
                budget,
                depth,
                format: fmt,
                exclude_tests,
                with_body,
                project_root: root.clone(),
            };
            let Some(ctx) = sigil::context::build_context(&idx, &q, &opts) else {
                eprintln!("no entity matches `{}`", q);
                eprintln!("hint: try `sigil search {}` to find similar symbols", q);
                std::process::exit(2);
            };
            match fmt {
                sigil::context::ContextFormat::Markdown => {
                    print!("{}", sigil::context::render_markdown(&ctx));
                }
                sigil::context::ContextFormat::Agent => {
                    println!("{}", sigil::context::render_agent_json(&ctx));
                }
                sigil::context::ContextFormat::Full => {
                    println!("{}", sigil::context::render_full_json(&ctx, pretty));
                }
            }
        }
        Cli::Map { root, tokens, focus, depth, format, write, exclude_tests, no_clusters, top_entities_per_subsystem } => {
            let idx = query::load(&root)
                .unwrap_or_else(|e| { eprintln!("error: {}", e); std::process::exit(1); });
            let rank_manifest = sigil::map::load_rank_manifest(&root)
                .unwrap_or_else(|e| { eprintln!("error: {}", e); std::process::exit(1); });
            if rank_manifest.file_rank.is_empty() {
                eprintln!("note: .sigil/rank.json not found — files will be listed without rank ordering");
                eprintln!("      run `sigil index` (rank is on by default) to populate it");
            }

            let opts = sigil::map::MapOptions {
                tokens,
                focus,
                depth,
                exclude_tests,
                clusters: !no_clusters,
                top_entities_per_subsystem,
                ..sigil::map::MapOptions::default()
            };
            let map = sigil::map::build_map(&idx, &rank_manifest, &opts);

            match format.as_str() {
                "json" => {
                    serde_json::to_writer_pretty(std::io::stdout(), &map).ok();
                    println!();
                }
                "markdown" | "md" => {
                    print!("{}", sigil::map::render_markdown(&map));
                }
                other => {
                    eprintln!("error: unknown --format {}. expected `markdown` or `json`", other);
                    std::process::exit(1);
                }
            }

            if write {
                sigil::map::write_sigil_map(&map, &root)
                    .unwrap_or_else(|e| { eprintln!("error writing .sigil/SIGIL_MAP.md: {}", e); std::process::exit(1); });
            }
        }
        Cli::Claude { action } => match action {
            InstallAction::Install { root } => {
                match sigil::install::claude::install(&root) {
                    Ok(steps) => {
                        for s in &steps {
                            eprintln!("claude: {:?}", s);
                        }
                        eprintln!("sigil Claude Code integration installed at {}", root.display());
                    }
                    Err(e) => {
                        eprintln!("error installing Claude integration: {}", e);
                        std::process::exit(1);
                    }
                }
            }
            InstallAction::Uninstall { root } => {
                match sigil::install::claude::uninstall(&root) {
                    Ok(steps) => {
                        for s in &steps {
                            eprintln!("claude: {:?}", s);
                        }
                    }
                    Err(e) => {
                        eprintln!("error uninstalling Claude integration: {}", e);
                        std::process::exit(1);
                    }
                }
            }
        },
        Cli::Cursor { action } => match action {
            InstallAction::Install { root } => match sigil::install::cursor::install(&root) {
                Ok(r) => eprintln!("cursor: {:?}", r),
                Err(e) => { eprintln!("error: {}", e); std::process::exit(1); }
            },
            InstallAction::Uninstall { root } => match sigil::install::cursor::uninstall(&root) {
                Ok(true) => eprintln!("cursor: removed"),
                Ok(false) => eprintln!("cursor: nothing to remove"),
                Err(e) => { eprintln!("error: {}", e); std::process::exit(1); }
            },
        },
        Cli::Codex { action } => match action {
            InstallAction::Install { root } => match sigil::install::codex::install(&root) {
                Ok(steps) => {
                    for s in &steps {
                        eprintln!("codex: {:?}", s);
                    }
                }
                Err(e) => { eprintln!("error: {}", e); std::process::exit(1); }
            },
            InstallAction::Uninstall { root } => match sigil::install::codex::uninstall(&root) {
                Ok(steps) => {
                    for s in &steps {
                        eprintln!("codex: {:?}", s);
                    }
                }
                Err(e) => { eprintln!("error: {}", e); std::process::exit(1); }
            },
        },
        Cli::Gemini { action } => match action {
            InstallAction::Install { root } => match sigil::install::gemini::install(&root) {
                Ok(steps) => {
                    for s in &steps {
                        eprintln!("gemini: {:?}", s);
                    }
                }
                Err(e) => { eprintln!("error: {}", e); std::process::exit(1); }
            },
            InstallAction::Uninstall { root } => match sigil::install::gemini::uninstall(&root) {
                Ok(steps) => {
                    for s in &steps {
                        eprintln!("gemini: {:?}", s);
                    }
                }
                Err(e) => { eprintln!("error: {}", e); std::process::exit(1); }
            },
        },
        Cli::Opencode { action } => match action {
            InstallAction::Install { root } => match sigil::install::opencode::install(&root) {
                Ok(steps) => {
                    for s in &steps {
                        eprintln!("opencode: {:?}", s);
                    }
                }
                Err(e) => { eprintln!("error: {}", e); std::process::exit(1); }
            },
            InstallAction::Uninstall { root } => match sigil::install::opencode::uninstall(&root) {
                Ok(steps) => {
                    for s in &steps {
                        eprintln!("opencode: {:?}", s);
                    }
                }
                Err(e) => { eprintln!("error: {}", e); std::process::exit(1); }
            },
        },
        Cli::Aider { action } => match action {
            InstallAction::Install { root } => match sigil::install::aider::install(&root) {
                Ok(r) => eprintln!("aider: {:?}", r),
                Err(e) => { eprintln!("error: {}", e); std::process::exit(1); }
            },
            InstallAction::Uninstall { root } => match sigil::install::aider::uninstall(&root) {
                Ok(true) => eprintln!("aider: removed"),
                Ok(false) => eprintln!("aider: nothing to remove"),
                Err(e) => { eprintln!("error: {}", e); std::process::exit(1); }
            },
        },
        Cli::Copilot { action } => match action {
            InstallAction::Install { root } => match sigil::install::copilot::install(&root) {
                Ok(r) => eprintln!("copilot: {:?}", r),
                Err(e) => { eprintln!("error: {}", e); std::process::exit(1); }
            },
            InstallAction::Uninstall { root } => match sigil::install::copilot::uninstall(&root) {
                Ok(true) => eprintln!("copilot: removed"),
                Ok(false) => eprintln!("copilot: nothing to remove"),
                Err(e) => { eprintln!("error: {}", e); std::process::exit(1); }
            },
        },
        Cli::Hook { action } => match action {
            InstallAction::Install { root } => match sigil::install::githook::install(&root) {
                Ok(steps) => {
                    for s in &steps {
                        eprintln!("hook {}: {:?}", s.name, s.result);
                    }
                }
                Err(e) => { eprintln!("error: {}", e); std::process::exit(1); }
            },
            InstallAction::Uninstall { root } => match sigil::install::githook::uninstall(&root) {
                Ok(steps) => {
                    for s in &steps {
                        eprintln!("hook {}: {:?}", s.name, s.result);
                    }
                }
                Err(e) => { eprintln!("error: {}", e); std::process::exit(1); }
            },
        },
        Cli::Update => {
            eprintln!("Checking for updates...");
            let mut updater = axoupdater::AxoUpdater::new_for("sigil");
            let version: axoupdater::Version = env!("CARGO_PKG_VERSION").parse()
                .unwrap_or_else(|e| { eprintln!("error parsing version: {}", e); std::process::exit(1); });
            if let Err(e) = updater.set_current_version(version) {
                eprintln!("Update failed: {}", e);
                std::process::exit(1);
            }
            if let Err(e) = updater.load_receipt() {
                eprintln!("Update failed: {}", e);
                eprintln!("hint: self-update only works when sigil was installed via the official installer.");
                eprintln!("      Reinstall with: curl --proto '=https' --tlsv1.2 -LsSf https://github.com/knova-run/sigil/releases/latest/download/sigil-installer.sh | sh");
                std::process::exit(1);
            }
            match updater.run_sync() {
                Ok(Some(result)) => {
                    eprintln!("Updated sigil to {}", result.new_version);
                }
                Ok(None) => {
                    eprintln!("Already on the latest version ({})", env!("CARGO_PKG_VERSION"));
                }
                Err(e) => {
                    eprintln!("Update failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Cli::Identifiers { text } => {
            let ids = sigil::identifiers::extract(&text);
            match serde_json::to_string(&ids) {
                Ok(s) => println!("{}", s),
                Err(e) => {
                    eprintln!("identifiers: failed to serialize: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Cli::Decisions { root } => {
            for marker in sigil::decisions::extract_from_root(&root) {
                match serde_json::to_string(&marker) {
                    Ok(s) => println!("{}", s),
                    Err(e) => {
                        eprintln!("decisions: failed to serialize: {}", e);
                        std::process::exit(1);
                    }
                }
            }
        }
        Cli::PackageDeps { root } => {
            for edge in sigil::package_deps::extract_from_root(&root) {
                match serde_json::to_string(&edge) {
                    Ok(s) => println!("{}", s),
                    Err(e) => {
                        eprintln!("package-deps: failed to serialize: {}", e);
                        std::process::exit(1);
                    }
                }
            }
        }
        Cli::Contracts { root } => {
            for row in sigil::contracts::extract_from_root(&root) {
                match serde_json::to_string(&row) {
                    Ok(s) => println!("{}", s),
                    Err(e) => {
                        eprintln!("contracts: failed to serialize: {}", e);
                        std::process::exit(1);
                    }
                }
            }
        }
        Cli::Workspace { action } => match action {
            WorkspaceAction::Scan { root } => {
                for entry in sigil::workspace::scan(&root) {
                    match serde_json::to_string(&entry) {
                        Ok(s) => println!("{}", s),
                        Err(e) => {
                            eprintln!("workspace scan: failed to serialize: {}", e);
                            std::process::exit(1);
                        }
                    }
                }
            }
        },
        Cli::Hotspots { root, commits } => match sigil::hotspots::mine(&root, commits) {
            Ok(rows) => {
                for row in rows {
                    match serde_json::to_string(&row) {
                        Ok(s) => println!("{}", s),
                        Err(e) => {
                            eprintln!("hotspots: failed to serialize: {}", e);
                            std::process::exit(1);
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("hotspots: {}", e);
                std::process::exit(1);
            }
        },
        Cli::SecurityScan { root } => {
            for finding in sigil::security_scan::scan_root(&root) {
                match serde_json::to_string(&finding) {
                    Ok(s) => println!("{}", s),
                    Err(e) => {
                        eprintln!("security-scan: failed to serialize: {}", e);
                        std::process::exit(1);
                    }
                }
            }
        }
        Cli::Ownership { root, commits } => match sigil::ownership::mine(&root, commits) {
            Ok(rows) => {
                for row in rows {
                    match serde_json::to_string(&row) {
                        Ok(s) => println!("{}", s),
                        Err(e) => {
                            eprintln!("ownership: failed to serialize: {}", e);
                            std::process::exit(1);
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("ownership: {}", e);
                std::process::exit(1);
            }
        },
        Cli::Heritage { symbol, root, pretty } => {
            let idx = query::load(&root)
                .unwrap_or_else(|e| { eprintln!("error: {}", e); std::process::exit(1); });
            let report = sigil::heritage::build_report(&idx, &symbol);
            println!("{}", sigil::heritage::render_json(&report, pretty));
            // Exit non-zero when the symbol has no edges in either direction
            // and no matching definition — same convention as `sigil blast`.
            if report.definitions.is_empty()
                && report.incoming.is_empty()
                && report.outgoing.is_empty()
            {
                std::process::exit(2);
            }
        }
    }
}

/// Short prefix of a query string — used in didactic hints that
/// suggest narrowing a miss to a shorter substring.
fn first_few(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Emit a didactic stderr hint when `sigil callers` / `sigil callees`
/// returns empty. Loads the index (best effort) to suggest close
/// matches via `query::suggest_similar`. The "did you mean?" line is
/// the key recovery point — it keeps the agent inside sigil's loop
/// after a bad-guess query, instead of falling back to grep.
fn emit_empty_hint(root: &std::path::Path, name: &str, verb: &str) {
    let idx = query::load(root).ok();
    let sugg = idx
        .as_ref()
        .map(|i| query::suggest_similar(i, name, 5))
        .unwrap_or_default();
    if sugg.is_empty() {
        eprintln!(
            "sigil: no {verb} for `{name}`. Try `sigil where {name}` to confirm the name exists, or `sigil search {}` with a shorter substring.",
            first_few(name, 4),
        );
    } else {
        eprintln!(
            "sigil: no {verb} for `{name}`. Did you mean: {}?",
            sugg.join(", ")
        );
    }
}

/// Handler for `sigil query 'SQL'`. Feature-gated: builds without
/// `--features db` get a helpful error pointing at the feature flag.
#[cfg(feature = "db")]
fn run_query(sql: &str, root: &std::path::Path, format: &str, max_cell_width: usize, pretty: bool) {
    let db = match sigil::query::duckdb_backend::DuckDbBackend::open(root) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("error: {}", e);
            eprintln!("hint: run `sigil index` first — the DuckDB backend is built from .sigil/*.jsonl.");
            std::process::exit(1);
        }
    };
    // Empty-index guard mirrors `Backend::load`'s check for the router
    // path — an empty-tables open (post-aa86ac9) would silently return
    // zero rows for every query, which is surprising to users who
    // expected a "run sigil index first" nudge.
    match db.len() {
        Ok((0, 0)) => {
            eprintln!(
                "error: sigil index is empty at {} — run `sigil index` first to populate .sigil/*.jsonl.",
                root.display()
            );
            std::process::exit(1);
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    }
    let result = match db.exec_query(sql) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };
    match format {
        "markdown" | "md" => print!("{}", result.to_markdown(max_cell_width)),
        "json" => println!("{}", result.to_json(pretty)),
        other => {
            eprintln!("error: unknown --format {}. expected markdown|json", other);
            std::process::exit(1);
        }
    }
}

#[cfg(not(feature = "db"))]
fn run_query(_sql: &str, _root: &std::path::Path, _format: &str, _max_cell_width: usize, _pretty: bool) {
    eprintln!("error: `sigil query` requires the `db` feature — rebuild with `cargo install sigil --features db`.");
    std::process::exit(1);
}
