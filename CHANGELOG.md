# Changelog

All notable changes to sigil are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`sigil semantic <query>` (Spike 1 of the semantic-search workstream).**
  BM25 retrieval over the entity index ranks symbols by relevance to a
  natural-language query, complementing the substring-matching `sigil
  search`. Identifier-aware tokenizer splits CamelCase/snake_case/
  kebab-case at the boundary. Robertson BM25 with k1=1.2, b=0.75. Indexed
  text per entity is `name + qualified_name + sig + doc`. JSON output
  mirrors `sigil search`'s shape with an added `score` field.
  `--no-doc` excludes the doc field from indexed text — used for eval
  de-biasing (queries are docstring first-sentences; including doc in
  the index inflates measured retrieval quality via self-referential
  overlap). On sigil-on-sigil eval (200 docstring queries, see
  `evals/README.md`) NDCG@10 jumps from 0.129 (`sigil search`) to 0.905
  with doc indexed and 0.370 doc-masked. Median latency ~50 ms per
  query.

## [0.6.2] — 2026-05-14 — CI speedup, contract detection expansion, workspace resolve polish

### Fixed

- **`sigil workspace resolve` major polish (issue #47).** Five
  sub-issues fixed:
  * Default focus (no `--focus`) now iterates every enabled member,
    instead of silently exiting 0 when cwd has no `.sigil/`.
  * `--focus <member-name>` is accepted in addition to a path; member
    names are looked up in `members.json` first.
  * Scope is now strictly the enabled membership — previously the
    resolver walked every `.git/`-bearing sibling under the workspace
    root and produced bogus matches against unrelated repos. This is
    a behavior change: workspaces relying on the broad sibling-scan
    must register members via `workspace add` first.
  * Stdlib stoplist filters Go (`context`, `net/url`), Python (`os`,
    `sys`, …), Node (`fs`, `path`, …), and Rust (`std::*`, `core::*`)
    so they don't resolve at 0.4 against unrelated leaf-name matches.
  * Output is now persisted to `.sigil-workspace/cross_repo_refs.jsonl`
    with `kind: "cross_repo_symbol"` rows alongside the existing
    module-level entries. Stdout output is preserved for piping.

### Added

- **Contract detection: AWS SDK + Kafka topics (issue #45).** New Go
  detectors (`framework: "aws-kinesis" | "aws-sqs" | "kafka-segmentio"
  | "kafka-confluent"`):
  * Kinesis `PutRecord(s)` → publisher; `GetRecords` /
    `SubscribeToShard` → subscriber.
  * SQS `SendMessage(Batch)?` → publisher; `ReceiveMessage` →
    subscriber. QueueUrl's trailing path segment becomes the topic.
  * Kafka segmentio `kafka.Writer{Topic: ...}` (publisher) and
    `kafka.NewReader(kafka.ReaderConfig{Topic: ...})` (subscriber).
  * Kafka confluent `Produce(&kafka.Message{TopicPartition: ...
    Topic: &t})` (publisher) and `SubscribeTopics([]string{...})`
    (subscriber).
  * Var-referenced topics (no string literal in scope) emit a
    `topic::$VAR.<varname>` placeholder for later workspace-side
    env / const resolution.

- **Contract detection: DB consumer side (issue #46).** `kind: "db",
  role: "consumer"` rows for GORM (`db.Table("X")` calls,
  `gorm:"table:X"` struct tags) and raw SQL (`Exec` / `Query` / `Select`
  / `QueryContext` / `Get` / `NamedExec` etc.). The SQL tokenizer
  extracts `FROM` / `JOIN` (read) and `INSERT INTO` / `UPDATE` /
  `DELETE FROM` (write) targets. `CREATE FUNCTION ... BEGIN ... END`
  bodies are stripped so the inner table refs don't leak into the
  outer query's matches. `contract_links.jsonl` now joins `db::<table>`
  owners with consumers across members.

### CI / build

- **`x86_64-apple-darwin` published artifact dropped.** Rosetta 2 runs
  the arm64 binary on Intel Macs transparently (installed-on-demand
  since macOS 11). Intel-only Mac users without Rosetta need `cargo
  install sigil` or `brew install` instead of the `.tar.gz` /
  `npx @knova-run/sigil`. Saves ~15 min from each release build.

- **Tree-sitter + DuckDB build cache.** Added a secondary
  `actions/cache@v4` entry under `.github/build-setup.yml` keyed by
  `Cargo.toml` dep version (with `restore-keys` fallback) so the heavy
  C/C++ compile output survives the lockfile bumps that defeat the
  primary `Swatinem/rust-cache`.

- **Windows MSVC LTO disabled in release builds.** `CARGO_PROFILE_DIST_LTO=false`
  injected into the Windows-target matrix slot only. 3–5% larger
  Windows binary, no runtime impact for the CLI workload, saves
  2–4 min per release.

### Breaking changes

- `workspace resolve` no longer matches against arbitrary sibling repos
  under the workspace root — only registered members. Workflows that
  relied on the implicit scan should run `workspace add` first.
- `RankManifest.file_rank` was switched from `HashMap` to `BTreeMap`
  in 0.6.1; consumers reading the JSON shouldn't notice (keys appear
  in sorted order now).

## [0.6.1] — 2026-05-14 — MCP workspace-aware loading + DuckDB-async + workspace rank.json

### Fixed

- **`sigil mcp` at a workspace root now union-loads members** (issue
  #43). `SigilServer` previously read `<root>/.sigil/entities.jsonl`
  directly via a hand-rolled `load_index`, which bailed when only
  `.sigil-workspace/` existed at the root. The startup path now routes
  through `Backend::load`, which auto-detects
  `.sigil-workspace/members.json` and dispatches to
  `Index::load_workspace`. Cross-member symbol queries through MCP
  (e.g. flask + werkzeug workspace) now surface entities from sibling
  repos.

### Added

- **`Backend::materialize_index()`** — lazy in-memory `Index` view on a
  `Backend`. `InMemory` returns its wrapped `Index` for free; `DuckDb`
  variants re-parse JSONL on first call and cache the result via
  `OnceLock<Result<Index, String>>`. Re-parse is lossless — preserves
  `doc`, `heritage`, `rank`, `blast_radius`, `meta` that the columnar
  schema drops. MCP's `get_context` / `get_overview` / `get_dead_code` /
  `get_answer` route through this; `sigil_search` stays on direct
  `Backend.search` so search-only sessions over DuckDB workspaces never
  trigger materialization. Failures surface as `Err(msg)` rather than
  panicking the tokio worker.

- **`DuckDbBackend.conn` wrapped in `std::sync::Mutex`** so
  `Backend: Send + Sync`. Unlocks `Arc<Backend>` across async await
  boundaries — rmcp tool-handler futures now satisfy the `Send` bound
  with DuckDB engaged. A compile-time `assert_send_sync::<Backend>()`
  test pins the contract.

- **Workspace-level `.sigil-workspace/rank.json`** — `sigil workspace
  index` computes PageRank over the union-loaded graph (cross-repo refs
  included, file paths member-prefixed) and writes it alongside the
  existing cross-repo artifacts. `workspace::load_rank_manifest` prefers
  it when present; falls back to merging per-member `.sigil/rank.json`
  files.

- **`RankManifest.file_rank` switched from `HashMap` to `BTreeMap`** —
  both per-repo and workspace `rank.json` outputs are now deterministic
  (lexicographically-sorted keys). Matches CLAUDE.md's deterministic-
  diff rule for workspace artifacts.

## [0.6.0] — 2026-05-14 — agent-loop context improvements + native MCP server

### Added

- **`sigil mcp` — native Model Context Protocol stdio server** (issue
  #39). Built on the official [rmcp 1.7](https://docs.rs/rmcp/) SDK
  with `tokio` as the async runtime. Exposes sigil's structural
  intelligence as 5 deterministic tools clients can call directly:
  * `sigil_search` — substring search over symbols + files; returns
    short-keyed hits.
  * `get_context` — per-symbol context bundle (batched over
    multiple targets). Accepts bare names, qualified forms
    (`file::Class::method`), or bare file paths.
  * `get_overview` — `sigil map`-shaped architecture map with top
    entities per subsystem.
  * `get_dead_code` — framework-aware dead-code findings
    partitioned into `safe_to_delete` (>= 0.85, file-level +
    exported-orphan tier, matching `sigil dead-code --safe-only`)
    and `review_first` (< 0.85, internal helpers).
  * `get_why` — three-mode decision-record search (free-text /
    file-path / no-args dashboard).
  All input JSON schemas are auto-derived via `schemars`. Loads the
  index once on startup, services JSON-RPC requests until stdin
  closes. No LLM dependency, no API key.

- **6th MCP tool `get_answer` — RAG synthesis via MCP sampling**
  (issue #41). Capability-aware: captures the client's
  `capabilities.sampling` from the `initialize` handshake and
  returns a structural bundle (top-ranked entities, matching
  decisions, citation-mandating synthesis prompt) plus
  `sampling_supported: true|false`. When sampling is supported, the
  client can hand `synthesis_prompt` directly to its own model via
  `sampling/createMessage`; otherwise a fallback `note` documents
  inline synthesis. Sigil itself performs no LLM calls — zero
  API-key dependency preserved.

- **`sigil context <unknown_symbol> --format agent|json` emits
  structured no-match JSON on stdout** (issue #36). Shape:
  `{q, resolved: false, reason, candidates: [{f, n, k, l}]}`.
  Candidates come from `Index::search(Scope::All, limit=10)` with
  edit-distance typo fallback via `suggest_similar`. Exit 2
  unchanged. Markdown format still emits the stderr text.

- **`sigil context <file_path>` returns a per-file digest** (issue
  #37). When the query matches an indexed file path, the bundle is
  `{q, kind:"file", f, entities:[{n,k,l:[start,end],v?,d?}],
  top_callers, top_callees}` instead of routing to symbol
  resolution. Top-level outline only (methods filtered out); cross-
  file edges deduped and sorted deterministically by (file, line).

- **Heritage in `sigil context` bundles** (issue #38). Two
  complementary additions:
  * `Context.parents` (agent JSON key `h.parents`) for class
    entities — resolved through `extend`/`implement`/`trait_impl`
    edges. Lets an agent see `class Flask(App):` → App without a
    separate `sigil heritage` call.
  * `Subclass::member` queries that miss direct resolution walk
    the parent class's inheritance chain (BFS, depth-bounded 16)
    looking for the member. On hit, the response carries
    `resolved_via: "heritage"` and a file-qualified `ancestor`
    handle (e.g. `src/flask/sansio/app.py::App`).

### Changed

- **DuckDB backend simplification.** `DuckDbBackend::open` and
  `open_workspace` now use `Connection::open_in_memory()` and
  populate TEMP tables from JSONL on every open. The on-disk
  `.sigil/index.duckdb` and `.sigil-workspace/index.duckdb`
  artifacts (and their `.stamp` siblings) are no longer written.
  ~80 LOC of stamp infrastructure removed. JSONL is the
  authoritative source. Pre-existing `.duckdb` files from prior
  versions are ignored (not read, not deleted; users can `rm` to
  reclaim disk).

- **`sigil context` CLI help** documents the new file-path form as
  a fourth accepted query shape.

### Dependencies

- Added `rmcp = "1.7"` (`server` + `macros` + `transport-io`
  features) and its required `tokio = "1.52.3"` + `schemars = "1"`
  peers.
- Refreshed `blake3 1.8.5`, `tokio 1.52.3`, `tree-sitter-c 0.24.2`
  to latest patch.

## [0.5.5] — 2026-05-13 — call-graph accuracy, 15-language heritage, DuckDB parity, cross-repo external resolve

### Added

- **Call-graph accuracy: tier-1/2/3 confidence-tagged resolver across all
  15 languages** (issue #15). `Reference.confidence` carries a stable
  per-edge confidence score, repowise-aligned. Tiers, highest to lowest:
  * `0.95` — same-file bare-identifier call; also `self.X()` (Python/
    Ruby) and `this.X()` (Java/Kotlin/JS/TS/C#/Swift) bound to the
    caller's own class.
  * `0.93` — `Class.method()` where the receiver class is defined in
    the same file.
  * `0.88` — `Class.method()` where `Class.method` has exactly one
    matching definition globally in the caller's language.
  * `0.85` — bare-name call where exactly one of the caller's imports
    (incl. Python `from X import *`) defines the name as callable.
    Closes the star-import gap.
  * `0.8` — file-local import-alias resolution (e.g. `import { foo as
    bar } from "./x"` then `bar()` → 0.8).
  * `0.7` — file-resolved edge from a manifest-aware resolver: see
    below.
  * `0.5` — tier-3 global-unique fallback (language-gated).
- **Manifest-aware import resolvers** that turn tier-2 0.8 cross-file
  edges into 0.7 file-resolved edges pointing at the actual `.kt`/`.go`/
  etc. file (and `Reference.callee_id` carries a stable `<file>::
  <symbol-path>` for downstream consumers). Languages covered:
  * **JS/TS** — `tsconfig.json` `compilerOptions.paths` longest-prefix
    rewrite plus existing barrel-follow.
  * **Go** — multi-`go.mod` workspace prefix map; `vendor/` skipped.
  * **PHP** — `composer.json` `autoload.psr-4` + `autoload-dev.psr-4`
    longest-prefix namespace → directory map.
  * **Rust** — root `Cargo.toml` `[workspace] members` glob with
    automatic hyphen↔underscore aliasing (rustc-style).
  * **Swift** — `Package.swift` `.target(name:)`/`.executableTarget`/…
    declarations via regex (defaults to `Sources/<name>` /
    `Tests/<name>` when `path:` is omitted).
  * **Kotlin + Scala** — path-based FQN disambiguation across the
    standard `src/main/<lang>/<pkg-as-dirs>/` layout.
  * **C/C++** — `compile_commands.json` `-I/-isystem/-iquote` lookup
    plus importer-relative `#include` resolution.
  * **C#** — `.csproj` + `.sln` + namespace map + project-level
    `<Using/>` + `global using` directives + `<ImplicitUsings>`
    (default SDK set + Web SDK set on `Microsoft.AspNetCore*`
    PackageReference); ranks same-project > referenced-project >
    anywhere.
  * **Ruby** — Rails autoload convention (CamelCase class →
    snake_case `.rb` under `app/**`/`lib/**`).
- **`external:<modpath>` sentinel entities** for imports that don't
  resolve to a workspace file (Go third-party, Rust non-workspace
  crates, Swift external SPM targets, JS/TS non-aliased imports,
  Python missing modules). Surfaces external dependencies as
  first-class graph nodes for cross-repo readiness.
- **`Reference.callee_id`** — optional stable per-symbol identifier
  of form `<file>::<symbol-path>` populated by every manifest
  resolver. Old refs.jsonl rows round-trip as None (additive
  schema).
- **Heritage extraction across 11 languages** (issue #15 second
  half). `Entity.heritage` populates for:
  * Go — struct embedding (`embed`).
  * Java — `extends` (extend), `implements` (implement), and
    interface-`extends` (extend) — both `superclass`/`interfaces`
    fields and the `extends_interfaces` child node.
  * TypeScript — class `extends` + `implements`; interface
    `extends` (multi-parent).
  * JavaScript — class `extends`.
  * Python — class inheritance (`class Foo(Base, Mixin)`),
    including `class Shape(ABC)` and `metaclass=Meta`.
  * Rust — `impl Trait for Type` (implement, on the impl entity)
    and `trait Sub: Super` (extend, on the trait).
  * Kotlin / Scala / C# / Swift / C++ — class supertypes from
    delegation_specifier / extends_clause / base_list /
    inheritance_specifier / base_class_clause respectively. Most
    emit as `extend` since syntactically they don't distinguish
    superclass vs interface/protocol/mixin.
  * PHP — `extends` (extend) and `implements` (implement)
    separately.

### Changed

- **`Reference.confidence` tier-1 value moved from `1.0` → `0.95`**
  to align with repowise's confidence scale (repowise leaves
  AST-uncertainty headroom even on same-file matches). Filters
  inside the resolver passes updated accordingly. Public consumers
  filtering on `confidence == Some(1.0)` should migrate to
  `Some(0.95)` (or `c >= 0.95`).

- **Kotlin + Swift + Scala + PHP language support** (issue #19, four new
  languages landing together; Luau dropped from the roadmap). New
  `lang-kotlin`, `lang-swift`, `lang-scala`, `lang-php` feature flags,
  all enabled by default, bring the supported-grammar count to 15.
  Powered by `tree-sitter-kotlin-sg` v0.4 (ast-grep fork),
  `tree-sitter-swift` v0.7, `tree-sitter-scala` v0.26, and
  `tree-sitter-php` v0.24. Kotlin support details:
  `tree-sitter-kotlin-sg` v0.4 (ast-grep fork, actively maintained against
  modern tree-sitter releases). Extracts `function_declaration` →
  `function` / `method`, `class_declaration` → `class` / `interface`
  (disambiguated by the leading keyword), `object_declaration` → `object`,
  `property_declaration` → `constant` / `property` / `variable`,
  `package_header` → `module`, `import_header` → `import` symbols and
  `kind=import` Reference rows. Nested members inside class / object /
  `companion object` bodies are emitted with qualified names
  (`Outer.member`, `Outer.Companion.member`). Visibility maps Kotlin's
  `public` (default) / `private` / `protected` (→ `internal`) /
  `internal`. `@`-prefixed annotations attach as metaprogramming markers.
  Extensions: `.kt`, `.kts`.

### Fixed

- **12 cross-language call-graph + structural-coverage gaps** (PR #26)
  surfaced by a multi-round QA pass across ~30 fresh popular OSS repos
  (one per supported language plus polyglot stacks Go+Vue and
  Python+React+Rust). Each fix is TDD-driven with a ground-truth grep
  cross-check on a real repo:
  * JS/TS builtin filter split — bare-name calls no longer blocked by
    instance-method names (`match`, `map`, `filter`, `then`, …); ts-pattern
    `callers match` 0 → 467.
  * JS/TS function-expression caller context — `const dayjs = function()
    { … }` now threads `parent_ctx = dayjs` into the body.
  * Swift type-annotation refs for parameter / return / property types
    (`emit_swift_type_refs`); swift-log `callers Logger` 8 → 206.
  * Ruby `Class.new` un-filter — receiver-aware builtin gate; faraday
    `callers Connection` 0 → 28.
  * DuckDB-backend parity with the in-memory `Index` for both
    `get_callers` and `get_callees` — slate `callers Editor` DuckDB
    137 → 1236 (matches in-memory 1237).
  * Heritage report filtered to class-like kinds; slate `heritage
    Editor` 379 → 1.
  * `contracts` skips `.yarn` / `.next` / `.turbo` / coverage / cache
    dirs; slate contracts 46 → 0.
  * `blast_radius` lookup uses entity.qualified_name + bare_leaf
    alongside literal name; faraday `blast Connection` populated.
  * `dead-code --safe-only` help text + docstring corrected to `≥ 0.85`.
  * Swift `property_declaration` walks the value expression — captures
    `let s = Session()` constructor calls; Alamofire `callers Session`
    48 → 489.
  * C++ parameter + struct/class field type-annotation refs; Catch2
    `callers SectionInfo` 4 → 37.
  * JSX `<Component />` and TSX/JS `<Component>` use sites emit
    `kind=instantiation` refs; React `callers Suspense` 25 → 1416.
  * Vue/Svelte/Astro `<template>` `<Component>` tag scanner with
    balance-counted nested templates; gitea `callers SvgIcon` 0 → 73
    (matches grep 1:1).
- **TS `type_annotation` walker** (PR #26 follow-up) — parameter /
  return / variable / generic-constraint type-position uses emit refs
  via the existing recursive walker (was only firing on class_heritage
  and type-alias bodies). excalidraw `callers ExcalidrawElement` 190 → 676.
- **`compute_blast` transitive BFS** seeds with the same 3-key
  expansion (e.name + qualified_name + bare_leaf) as the direct-caller
  lookup. Ruby/Kotlin/Scala mixed-separator qualified-name entities
  now report transitive_callers correctly (was silently 0). rspec-core
  `blast Runner` transitive 0 → 112.
- **DuckDB refs schema** (PR #34, closes #32) preserves `confidence`
  + `callee_id` on round-trip. Adds `CURRENT_SCHEMA_VERSION` + a
  `schema_version` field to the Stamp (`#[serde(default)]` for back-
  compat) so existing `.sigil/index.duckdb` rebuilds on first open
  after upgrade. gitea: a real ref now returns `confidence=0.7,
  callee_id="modules/assetfs/embed.go::GenerateEmbedBindata"` instead
  of None on both fields.

### Added (call-resolver follow-ups, PR #35)

- **Member-call Strategy 1 (module-alias receiver)** (issue #27).
  `resolve_member_call` now resolves `alias.method()` style calls where
  `alias` is bound by an import in the caller's file. Per-file
  `local_name → import_target` table maps Python `import foo as f` /
  TS `import * as utils` style aliases back to the target file. Confidence
  **0.88**, symmetric with Strategy 2 imported.
- **Gradle settings.gradle(.kts) resolver** (issue #28). `load_gradle_settings`
  parses `include(":a:b:c")` directives into a module → directory map,
  `project(":x").projectDir = file("custom")` overrides, and per-module
  `srcDirs(...)` overrides in `build.gradle(.kts)`. Wired into
  `resolve_jvm_fqn_imports` as a fallback (path-based heuristic stays
  primary so default-layout repos see no regression). 0.7 confidence on
  manifest-resolved edges.
- **Scala sbt + Mill resolver** (issue #29). `load_sbt_modules` handles
  two manifest shapes: sbt's `lazy val NAME = project.in(file("DIR"))`
  and Mill's `build.sc` (each declares a module at its parent dir).
  Same `resolve_jvm_fqn_imports` integration as Gradle.
- **`sigil workspace resolve` cross-repo subcommand** (issue #30 MVP).
  New CLI: `sigil workspace resolve --root <dir> --focus <repo>`. Walks
  the focus repo's `external:<modpath>` sentinels, then scans every
  sibling repo discovered via `workspace::scan` for a matching entity.
  Emits one JSONL row per resolution at confidence 0.4 (cross-repo
  binding inherently less certain than intra-repo). MVP scope: no
  `package-deps` intersection yet, no separate workspace manifest file.

### Schema

- **`Entity.alias: Option<String>`** added (additive,
  `#[serde(default, skip_serializing_if = "Option::is_none")]`).
  Carries the local binding name from `import X as alias` style imports.
  Old JSONL round-trips as None.

## [0.5.0] — 2026-05-09 — module-level constants, entity docstrings, top-K subsystem entities

### Added

- **`kind: "constant"` entity** across all 8 supported tree-sitter languages
  (Python, Rust, Go, TypeScript, JavaScript, Java, C#, C++). The literal RHS
  is captured directly from the AST as `Entity.sig` (truncated to 256 chars
  with `…`), so `code.context RETRY_TIMEOUT` returns "60" inline instead
  of forcing a follow-up file read. Covers Python `NAME = …` (UPPER →
  constant, else variable, class-level too), Rust `const` / `static`,
  Go `const` blocks + package-level `var`, TS/JS top-level `const`, Java
  `static final`, C# `const` / `static readonly`, and C++ `constexpr`,
  top-level `const`, and `#define`.
- **`Entity.doc: Option<String>`** populated from each language's leading
  doc-comment / docstring convention: Python `"""…"""` first-statement,
  Rust `///` / `/** */`, Go godoc `//` (blank-line-aware), JSDoc `/** */`
  for JS/TS (drilling through `export_statement` so `/** … */ export const
  FOO = …` attaches to `FOO`), Javadoc, C# XML-doc `///`, and Doxygen for
  C++ (`///`, `//!`, `/** */`, `/*! */`). Truncated to 1024 chars. Surfaced
  in `code.context` markdown as a `## Doc` section between Signature and
  Body, in the agent JSON view under short key `d`.
- **`sigil map --top-entities-per-subsystem N`** (default 0, additive)
  attaches a `top_entities[]` list to each subsystem with full
  code.context-shaped fields (callers, callees, related types, doc),
  collapsing the downstream `subsystems → files → entities → context`
  N+1 query into a single map call. Markdown form mirrors with a tight
  per-subsystem block.
- **`sigil where` resolves constants** alongside functions / classes /
  methods. Module-level tunables (`RETRY_TIMEOUT`, `ANTHROPIC_BETA_HEADER`)
  now answer "where is this defined" the same way functions do; variables
  and imports remain excluded.

### Fixed

- The Python `extract_class` walker was dropping the first class-body
  statement when it wasn't a docstring, so a class whose first member was a
  constant assignment (`class Config: CACHE_VERSION = 3`) silently lost
  the constant from the index. Surfaced while writing the class-level
  constant tests.

### Compatibility

Both new fields (`Entity.sig` for constants, `Entity.doc`) are
`Option<...>` with `skip_serializing_if = "Option::is_none"`. Existing
`entities.jsonl` rows for symbols without a sig/doc value remain
byte-identical. `top_entities[]` only appears when
`--top-entities-per-subsystem > 0`. `--format json` of `code.context`
includes `"doc"` only when populated.

### CI

- Replaced `actions/cache` with `Swatinem/rust-cache@v2` for per-crate
  cache keys + target/ pruning + incremental restore.
- Dropped the redundant `cargo build` step (`cargo test` already compiles
  the lib + test targets).
- Added `concurrency` group with `cancel-in-progress` so back-to-back
  pushes don't burn runner minutes on superseded runs.

## [0.4.2] — 2026-05-09 — first npm release (fix for 0.4.1 CI break)

Functionally identical to 0.4.1. The 0.4.1 release pipeline produced
GitHub binaries successfully but its `publish-npm` job failed at
`npm install -g npm@latest` (a known mid-upgrade race on Node 22 where
the new bundled `node_modules` is missing dependencies). 0.4.2 bumps
the `publish-npm` job to Node 24, which already ships with npm 11.x
and removes the upgrade step. This is the first version published to
npm as `@knova-run/sigil`.

## [0.4.1] — 2026-05-09 — npm distribution + eval-driven primitives

### Added

- **`sigil grep <pattern>`** — text search with structural annotation.
  Returns hits in the file:line:text shape `grep`/`rg` users expect, but
  every hit is annotated with the entity it lives in (`fn foo`, `class
  Bar`, `method Bar.baz`) so the agent doesn't need a follow-up
  `sigil context` call to locate the match in the structure.
- **`sigil outline --kind <kind>`** — filter the outline tree to a
  single entity kind (e.g. `--kind class` for "every class top-level
  in this directory"). Stacks with `--path` for scoped views.
- **`sigil where`** — rank-sorted output by default; gains `--kind`,
  `--path`, and `--limit` filters that compose the same way `sigil
  search` does.
- **`sigil context --with-body`** — bundles the resolved symbol's
  body alongside the existing signature/callers/callees view. Removes
  the `sigil context` + `read_file` chain when the agent needs to read
  the actual implementation.

### Distribution

- **Repository moved** to [knova-run/sigil](https://github.com/knova-run/sigil).
  All install URLs, `cargo install` clones, and Python bindings now point
  at the new home; old `gauravverma/sigil` URLs redirect via GitHub.
- **`npx @knova-run/sigil`** — sigil now ships on npm using the
  esbuild-style optionalDependencies layout: a thin
  `@knova-run/sigil` wrapper plus per-platform binary packages
  (`-darwin-arm64`, `-darwin-x64`, `-linux-x64-gnu`, `-linux-arm64-gnu`,
  `-win32-x64-msvc`). npm picks the matching one via `os`/`cpu`/`libc`
  fields, so install is fast, lockfile-friendly, and works under
  `npm install --ignore-scripts`. Publishing runs from CI under npm
  OIDC trusted publishing — no `NPM_TOKEN` in repo secrets.

### Eval coverage

- New E4 swebench-like cases: `003-django-filefield-to-python`,
  `004-django-migrations-operations-outline`. Branch tested at N=3
  on both Sonnet 4.6 and Haiku 4.5; Haiku median tokens_in
  124k → 36k (≈3.4× reduction); Sonnet wins on per-task pass rate
  while running close to control on tokens.

## [0.4.0] — 2026-04-21

### Added — gap-widening primitives

Based on E4 SWE-bench-like trace analysis, five improvements aimed at
reducing the per-turn tool-result cost that dominates agent token usage:

- **`sigil where <symbol>`** — single-shot definition locator. Returns
  one row per defining (file, parent, kind) with signature preview,
  overload count, and test-file flag. Tail-segment matching
  (`get_default` matches `Parameter.get_default` and `Option.get_default`
  but not `CliRunner.get_default_prog_name`). Replaces the common
  `sigil search` + `read_file` + `grep` chain with one call.
- **`sigil outline [--path DIR]`** — hierarchical top-level tree of
  classes / functions / structs / enums / traits grouped by file.
  Complements `sigil map` (rank-ordered, budget-aware) with a plain
  structural view — no token budget, every eligible entity listed once.
  `src/click/` on pallets/click yields 17 files / 210 symbols in ~30 KB.
- **`sigil context` now surfaces inheritance delta.** When the chosen
  symbol is a method with a parent class, other classes in the codebase
  that define a method with the same tail segment appear in an
  `overrides: []` block (capped at 5, with `skipped_overrides` for the
  truncation count). Agents no longer need a second `sigil where`
  call to spot polymorphism — the override list is in the same bundle.
- **`sigil symbols --depth 1`** — outline mode. Filters a file's
  entity list down to top-level items (classes, top-level functions,
  structs, enums, traits, sections) — drops imports, variables,
  constants, and nested methods. Measured 95% byte reduction on
  `src/click/core.py` (87 KB -> 3.9 KB).
- **`sigil callers <name> --group-by file`** (also on `callees`) —
  collapse per-call-site output to a `{file: count}` map. Turns a
  128-ref flat list into a dozen-entry summary when the agent only
  needs distribution.

### Added — other

- **`sigil search` carries a signature preview.** Each row now
  includes `sig` when the entity has one, eliminating a follow-up
  `read_file` for common "look at the signature" flows. ~30-50 bytes
  per row overhead; typically saves a 2-5 KB file read.
- **Auto-index on first query.** Running `sigil where`, `sigil
  context`, `sigil outline`, `sigil symbols`, etc. in a directory
  without `.sigil/` now transparently runs `sigil index` (including
  a full rank + blast pass) with a one-line stderr heads-up. Zero-
  config onboarding for fresh clones. Opt out with
  `SIGIL_NO_AUTO_INDEX=1`.

### Changed — JSON output schema (breaking)

Script-facing commands with `--json` now emit a **compact** schema designed
for machine consumers. Agents re-ingest the returned JSON on every turn;
cutting the payload directly cuts downstream token cost.

- **Minified by default.** `sigil symbols / children / callers / callees /
  search / explore --json` emit one-line JSON. Add `--pretty` for indented
  output if a human is reading.
- **Hash columns dropped by default.** `struct_hash`, `body_hash`, and
  `sig_hash` are no longer included in `--json` output of `symbols` /
  `children`. Pass `--with-hashes` for the legacy shape. The on-disk
  `.sigil/entities.jsonl` still carries hashes — they're sigil's internal
  content-identity columns.
- **Default/absent fields elided.** `visibility: "private"` (the language
  default for most items), `blast_radius` of all-zeros, and empty `meta: []`
  arrays are now omitted from both JSON output and `.sigil/entities.jsonl`.
  Consumers should use `.get("field", default)` patterns rather than
  expecting every field.
- **`Reference.ref_kind` is serialized as `kind`.** Schema parity with
  `Entity.kind` — the two types now use the same field name for their
  "kind-of-thing" discriminator. Old `.sigil/refs.jsonl` with `ref_kind`
  still deserializes via a serde alias; fresh writes use `kind`. The
  DuckDB materialized table column also renamed.
- **`sigil search` JSON output is tighter and deduped.** Same-symbol
  overloads (Python `@overload` stubs, repeated variable declarations
  across method bodies) now collapse into one row per `(file, name,
  kind)` with `overloads: N` when there's more than one. The `type:
  "symbol"` field is elided (implied by the now-default `--scope
  symbol`); file hits keep `type: "file"`. `line: [a, b]` flattens to
  `line: N` with an optional `line_end: M` when they differ. `parent:
  null` and `overloads: 1` are elided. Example: `search get_default`
  on pallets/click drops from 17 rows / ~2.7KB to 11 rows / 1.68KB
  (~38% smaller, overload noise removed).
- **`sigil search --scope` now defaults to `symbol`**, not `all`. Agents
  almost always want symbol hits on a keyword query; including file-
  path matches inflated the response. Pass `--scope all` or `--scope
  file` to widen.

Size impact on sigil-self:
- `sigil symbols src/rank.rs --json`: 19,102 → **8,866 bytes (54% smaller)**
- `sigil callers parse_file --kind call --json`: 19,352 → **14,191 bytes
  (27% smaller)**

### Eval validation — deterministic agent uptake

After adding didactic stderr + fuzzy suggestions on empty sigil results,
and exposing sigil primitives as first-class Anthropic tools in the
treatment arm (alongside a directive flowchart blurb + one worked
example), Sonnet N=3 on the E4 click task converged to:

| Arm | Median tokens_in | Turns | Pass |
|---|---:|---:|---:|
| control | 12,269 | 6 | 3/3 |
| **treatment** | **5,521** | **2** | 3/3 |

Ratio: **2.22× (sigil wins)**. All 3 treatment seeds produced
byte-identical runs — `sigil_where(symbol="get_default")` as turn 1,
answer emitted as turn 2. Haiku N=1 ratio: 2.64×. No single-seed
variance of any kind; sigil tools as tool_use entries produce
deterministic agent paths.

Cumulative journey on the same task since pre-0.4.0: 0.49× (sigil
losing 2×) → 2.22× (sigil winning 2.2×). 4.5× total swing, driven by
three stacked changes — compact JSON, new primitives (`sigil where`,
`sigil outline`, signature preview, group-by aggregates), and agent-
uptake fixes (native tool_use exposure + directive blurb + worked
example).

### Eval validation — sigil now wins on the external-repo task

E4 "find-the-method" task against pallets/click (2.3k LOC, cloned at
04ef3a6) — a SWE-bench-Lite-style phase-1 exploration of an unfamiliar
codebase, where the agent must locate the method that resolves option
default values.

| Model | Arm | Median tokens_in | Pass |
|---|---|---:|---:|
| Sonnet 4.6 (N=3) | control | 23,270 | 3/3 |
| Sonnet 4.6 (N=3) | **treatment** | **16,698** | 3/3 |
| Haiku 4.5 (N=1) | control | 71,190 | 1/1 |
| Haiku 4.5 (N=1) | **treatment** | **43,330** | 1/1 |

Sonnet ratio: **1.39× (sigil wins)**. Haiku ratio: **1.64× (sigil wins)**.
Pre-0.4.0 numbers on the same task had Sonnet at 0.49× (sigil losing
2×) — a net 2.8× swing from the combined effect of compact entity/
reference JSON, sharper treatment-blurb hints, `--scope symbol` as the
search default, and the search overload-dedup + line-flatten.

Also notable: Sonnet treatment seeds 1/2/3 landed at 16,908 / 16,698 /
16,698 tokens — near-identical paths. Sigil appears to produce more
deterministic agent behavior than pure grep on the same question.

Full per-arm traces and archived pre-fix baselines under
`evals/results/2026-04-21/{haiku-4-5,sonnet-4-6}/E4{,-preblurbfix,-prescope}/`.

Upgrade note: pre-0.4.0 `.sigil/refs.jsonl` loads fine via the Rust alias,
but the DuckDB backend's materialized table definition has a renamed
column. Re-run `sigil index` once after the upgrade to rebuild the
derived DuckDB artifact.

### Fixed

- Script-facing commands (`symbols`, `children`, `callers`, `callees`) now
  default to unbounded results (`--limit 0`) as documented in the plan's
  agent-facing-vs-script-facing taxonomy. Previously defaulted to `100`,
  which silently truncated large result sets — `sigil callers parse_file
  --kind call` returned 100 refs across 8 files when the true answer was
  128 refs across 11 files. Users who want the previous behavior can pass
  `--limit 100` explicitly.
- `sigil callers <name>` now also surfaces refs whose stored name is a
  `::`-qualified path ending in `::<name>`. Previously the Rust extractor
  emitted a call site like `crate::parser::treesitter::parse_file(...)`
  under its full qualified name, so `sigil callers parse_file` missed it.
  Both the in-memory backend (`Index::build`) and the DuckDB backend
  (`get_callers` SQL) index/query under the trailing segment. Searches
  for an already-qualified name keep their exact-match semantics.
  Combined with the `--limit` fix above, `sigil callers parse_file
  --kind call` now returns 129 refs across 12 files (grep parity).

### Added

- Eval harness (`evals/runner/`) and `E2_navigation` task set. First
  end-to-end eval with a model in the loop; N=3 Sonnet numbers published
  against sigil-self. See `evals/runner/README.md` for methodology.

## [0.3.3] — 2026-04-21

### Changed

- Agent-facing skill (`skills/sigil/SKILL.md`) rewritten to cover the
  full v0.3.x command surface: `map`, `context`, `review`, `blast`,
  `duplicates`, `query`, `cochange`, `benchmark`. Previous skill only
  documented the 0.2.x primitives.

### Fixed

- CLI flag documentation across README, CLAUDE.md, and the skill. The
  valid `sigil search --scope` values are `symbol | file | text`
  (singular); `sigil callers --kind` does not accept `definition`;
  `sigil query` no longer requires `--features db` on shipped binaries
  since 0.3.2.

### CI / build

- `release.yml`: inject `Swatinem/rust-cache@v2` before every matrix
  `dist build` via cargo-dist's `github-build-setup` hook. First
  (cold-cache) run after this change is still full-compile; warm runs
  should drop Windows from ~20 min to ~3–5 min and total wall-clock
  from ~22 min to ~7 min.

## [0.3.2] — 2026-04-21

### Changed

- Release artifacts now ship a single full-feature binary (~20 MB).
  `cargo-dist` builds with `--features db,tokenizer` via a new
  `features` entry in `dist-workspace.toml`; the separate
  `release-full.yml` workflow and `sigil-full-*` assets are gone.
  Source builds via `cargo build --release` still default to lean —
  only the shipped artifact shape changes.
- Python wheels switch to PyO3 `abi3-py39`: one wheel per platform
  replaces six per-interpreter wheels. `python/pyproject.toml`
  version is now `dynamic`, read from `python/Cargo.toml`.

### CI / build

- `release-full.yml`: retired.
- `python-publish.yml`: single abi3 wheel per platform, `sccache`
  enabled, Python 3.8 dropped (EOL 2024-10). `requires-python`
  bumped to `>=3.9`.
- `release.yml`: regenerated by `dist generate` to reflect the
  `features` config.

## [0.3.1] — 2026-04-20

### Changed

- Dependency bumps: `tiktoken-rs` 0.7 → 0.11, `toml` 0.8 → 1, `similar`
  2 → 3, `serde_yaml` → `serde_yml` (unmaintained → maintained fork),
  `pyo3` 0.24 → 0.28 with migration to `attach` / `Py<PyAny>`. Plus
  SemVer-compatible patch updates via `cargo update`.

### Fixed

- `.github/workflows/release-full.yml`: drop `x86_64-apple-darwin`
  matrix entry. GitHub retired the `macos-13` hosted runner image on
  2025-12-08, so the Intel matrix job on v0.3.0 queued indefinitely
  with no runner. Remaining targets: `aarch64-apple-darwin`,
  `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`.

## [0.3.0] — 2026-04-20 — Phase 0 + Phase 1: in-house parsing, agent surface, DuckDB backend

Three bundled releases shipping together: the codeix-free parser layer,
the agent-adoption command surface (rank + blast + map/context/review),
and the DuckDB-materialized backend for monorepo scale. See the
[agent-adoption PR](https://github.com/knova-run/sigil/pull/3) for the
full rationale.

### Agent-adoption surface (Phase 1)

- `sigil map [--tokens N]` — budget-aware ranked codebase digest with
  label-propagation subsystems. Cold-start orientation artifact.
- `sigil context <symbol> [--budget N]` — signature + callers + callees
  + related types for a single symbol, capped to a token budget.
- `sigil review <refspec>` — PR-review wrapper: structural diff ranked
  by blast radius, plus co-change misses mined from `git log`.
- `sigil blast <symbol>` — impact summary (direct callers / files /
  transitive reach).
- `sigil duplicates` — body-hash clone report across the codebase.
- `sigil benchmark [--tokenizer o200k_base|cl100k_base|p50k_base]` —
  publishes median token-reduction vs raw alternatives. BPE-accurate
  counts via `--features tokenizer` (tiktoken-rs).
- `sigil cochange` — mines `git log --name-only` for file-pair
  co-change weights; written to `.sigil/cochange.json`.
- `src/rank.rs` — file-level PageRank over the import graph + per-entity
  blast-radius BFS (depth-capped at 3); persisted to `.sigil/rank.json`.
- `Entity.rank` / `Entity.blast_radius` / `Entity.visibility` fields
  added (serde-skipped when absent, back-compatible with 0.2.x indexes).

### Phase 0.5 — DuckDB-materialized backend

- `--features db` → `src/query/duckdb_backend.rs` ships a DuckDB-backed
  query engine with identical API to the in-memory `Index`. Lazily
  built from `.sigil/*.jsonl` on first query, refreshed on staleness
  stamp mismatch.
- Auto-engages when total JSONL size ≥ 5 MB (tunable via
  `SIGIL_AUTO_ENGAGE_THRESHOLD_MB`); force via `SIGIL_BACKEND=db|memory`.
  Unknown values are a hard error (no silent fallback).
- `sigil query 'SELECT ...'` — power-user escape hatch for ad-hoc SQL
  against the materialized index.

### Phase 0 — decodeix

### Added

- `src/parser/` — vendored tree-sitter extractors for 11 languages
  (C, C++, C#, Go, Java, JavaScript, Python, Ruby, Rust, TypeScript,
  Markdown) plus Vue/Svelte SFC support. Originally forked from codeix
  v0.5.0 under Apache-2.0; see `src/parser/NOTICE` for attribution.
  Feature-gated per language via `lang-<name>` flags.
- `src/query/index.rs` — in-house `Index` struct: loads
  `.sigil/entities.jsonl` + `refs.jsonl`, precomputes five lookup maps,
  exposes `get_callers`, `get_callees`, `get_file_symbols`,
  `get_children`, `search`, `explore_dir_overview`,
  `explore_files_capped`, `list_projects`.
- `Scope` enum (`All`, `Symbols`, `Files`) for `sigil search`; parses
  codeix-compatible scope strings.
- 23 unit tests for the query layer covering filter/limit semantics,
  substring/case matching, directory grouping, and parser fallbacks.

### Changed

- **Breaking (internal; no CLI change): `src/query.rs` replaced by a
  `src/query/` module.** `load_index()` → `load()`, returning an owned
  `Index` instead of `Arc<Mutex<SearchDb>>`. The mutex dance in
  `main.rs` is gone.
- `sigil search` result format: `SearchHit::Symbol(&Entity)` /
  `SearchHit::File(FileHit)` instead of codeix's three-variant
  `SearchResult`. JSON output now uses a `type` discriminator
  (`"symbol"` / `"file"`). Text-block hits dropped — sigil doesn't
  index docstring/comment bodies today; deferred until a clear
  consumer surfaces.
- `sigil explore` queries run against the in-house Index; output shape
  unchanged.
- Module reference in `CLAUDE.md` updated to reflect in-house ownership.

### Removed

- `codeix` git dependency (`github.com/montanetech/codeix`). Removed
  from `Cargo.toml` and no longer appears in `Cargo.lock`.
- `.codeindex/` directory — sigil no longer generates it. Added to
  `.gitignore` for repos that still have it around from an older
  install.
- Transitive deps pulled in by codeix (rusqlite, tokio, notify,
  tracing, rmcp, walkdir, …). Binary size drops by several MB on
  release builds.

### Fixed

- `.gitignore` had a concatenated typo (`.codeindexpython/.venv/`)
  that ignored neither `.codeindex/` nor `python/.venv/`. Split into
  two correct entries and added Phase 0.5 DuckDB reservations.
- `src/output.rs`: internal comment referenced "the codeix index";
  now correctly says "the sigil index".

### Python bindings (`sigil-diff` on PyPI)

- `python/pyproject.toml`: add `readme = "README.md"`, author,
  project URLs (Homepage / Repository / Issues), 14 trove
  classifiers (Python 3.8–3.13, Rust, MIT, OS support, topic
  taxonomy), plus `tree-sitter` and `ast` keywords. The next release
  will publish a complete PyPI project page instead of a blank one.
- All four bindings (`diff_json`, `diff_files`, `diff_refs`,
  `index_json`) verified end-to-end against the in-house code path.
  No Python-side code changes needed — the crate depends on
  `sigil_core` by path, which picked up the decodeix work
  transparently.

### Platform integrations

- Eight idempotent, marker-scoped, content-preserving installers:
  Claude Code, Cursor, Codex, Gemini CLI, OpenCode, Aider, GitHub
  Copilot CLI, and git post-commit / post-checkout hooks. Each
  installer has a matching `uninstall` that reverses exactly what
  was written. All preserve sibling user content — running
  `sigil claude install` on a repo with a hand-edited `CLAUDE.md`
  leaves user sections untouched.
- `git sigil <cmd>` alias via a tiny shim in `scripts/git-sigil`
  (`exec sigil "$@"`). Symlink or install the shim onto `PATH` and
  every `sigil <cmd>` becomes `git sigil <cmd>` — piggybacks on
  git's pretrained name recognition for agents that know `git diff`.

### CI / distribution

- `.github/workflows/release-full.yml` — new workflow ships a
  full-feature binary (`--features db,tokenizer`) alongside the
  existing lean cargo-dist build for macOS (arm64/x86_64), Linux
  (x86_64), and Windows (x86_64). Attached to the same GitHub
  Release as `sigil-full-<target>.{tar.gz,zip}`.
- README install flow switched from `cargo install --git` to
  pre-built release archives (no Rust toolchain required).

### Docs

- README.md rewritten as a single end-to-end document: hero hook →
  install (lean + full) → 5-minute tour → `git sigil` setup →
  agent installers → benchmarks → architecture → supported
  languages → command reference → backend selection → CI/CD →
  honest caveats → FAQ.
- `CLAUDE.md` refreshed to reflect Phase 1 modules, cargo features
  (`db`, `tokenizer`), and the full command surface.
- Planning scratches removed from git (`agent-adoption-plan.md`,
  `blog-agent-adoption.md`, `ARCHITECTURE.md`, `worked/`,
  `docs/superpowers/`).

## [0.2.4] — 2026-04-16

- CI: use `--find-interpreter` for Linux manylinux builds in the
  Python publish workflow.

## [0.2.3] — 2026-04-16

- ci: add GitHub Actions workflow for publishing Python wheels to
  PyPI.
- docs: add Python SDK documentation, rename package to `sigil-diff`.
- feat: add Python bindings via PyO3 — `import sigil;
  sigil.diff_json(old, new)`.

---

For versions 0.2.2 and earlier see `git log`.
