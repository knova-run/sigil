# CLAUDE.md

## Project Overview

sigil is a Rust CLI tool for structural code fingerprinting and diffing. It uses tree-sitter to parse source files, extract code entities (functions, classes, methods), compute content hashes, and produce entity-level diffs. Parsing and code-intelligence queries are fully in-house — no external indexer required.

## Build & Test

```bash
cargo build                          # default (lean) build
cargo build --features db,tokenizer  # full build: DuckDB backend + BPE tokenizer
cargo test                           # all tests
cargo test --lib                     # unit tests only
cargo test --test integration        # Index integration tests
cargo test --test diff_integration   # Diff integration tests
cargo test --test markdown_integration
```

## Architecture

```
src/
  lib.rs           — Library crate: re-exports modules for Python bindings and tests
  main.rs          — CLI binary (clap). Two-tier command surface:
                      Agent-facing: map, context, review, blast, benchmark
                      Script-facing: search, symbols, children, callers, callees,
                                     explore, duplicates, cochange, query, diff, index,
                                     identifiers, decisions, package-deps, contracts,
                                     workspace, hotspots, ownership, bus-factor, log,
                                     security-scan, communities, dead-code, heritage
                      Installers:    claude, cursor, codex, gemini, opencode, aider,
                                     copilot, hook
  entity.rs        — Entity + Reference structs (serde); visibility, rank,
                      blast_radius slots used by Phase 1 commands
  hasher.rs        — BLAKE3 (struct_hash, body_hash, sig_hash)
  signature.rs     — Signature extraction, language-aware
  meta.rs          — Metaprogramming marker detection
  cache.rs         — Incremental indexing cache (.sigil/cache.json)
  writer.rs        — JSONL output writer
  index.rs         — Index orchestration + parse_single_file
  json_index.rs    — JSON parsing (sigil-native); array item expansion, derived marking
  yaml_index.rs    — YAML parsing (sigil-native)
  toml_index.rs    — TOML parsing (sigil-native)
  markdown_index.rs — Markdown parsing (headings, code blocks, tables, lists, front matter)
  parser/          — Vendored tree-sitter extractors for 15 languages; see parser/NOTICE
  git.rs           — Git operations (changed_files, file_at_ref, git log for cochange)
  matcher.rs       — Entity matching across versions (exact/moved/renamed)
  classifier.rs    — Change classification (sig/body hash matrix)
  diff.rs          — Diff orchestration (refspec or --files → parse → match → classify)
  diff_json.rs     — Diff output structs (EntityDiff, DiffResult)
  inline_diff.rs   — Line-level diffs within entities
  change_detail.rs — Token-level change extraction
  output.rs        — DiffOutput intermediate model for formatters
  formatter.rs     — Colored terminal output
  markdown_formatter.rs — GitHub-flavored Markdown output

  # Phase 1 — rank, blast radius, agent commands
  rank.rs          — File-level PageRank + per-entity blast-radius BFS.
                      Reuses `query::index::{bare_leaf, head_prefixes_with_sep}`
                      for alias-aware caller lookup so qualified-name
                      entities (Ruby/Kotlin/Scala) find their inbound refs.
  community.rs     — Label-propagation subsystem detection for `sigil map`
  communities.rs   — Leiden modularity clustering for `sigil communities`
                      (issue #17). Modularity-greedy local-moving plus a
                      refinement pass that splits any internally-disconnected
                      community by BFS, guaranteeing every output cluster
                      is connected. `cluster_id` surface for downstream
                      consumers via `sigil map`
  map.rs           — `sigil map` — budget-aware ranked codebase digest
  context.rs       — `sigil context <symbol>` — minimum-viable symbol bundle
  blast.rs         — `sigil blast <symbol>` — impact summary
  review.rs        — `sigil review <refspec>` — diff + blast + co-change
  benchmark.rs     — `sigil benchmark` — token-reduction vs raw alternatives
  duplicates.rs    — `sigil duplicates` — body_hash clone report
  cochange.rs      — Git-history file-pair co-change mining (.sigil/cochange.json)
  tokens.rs        — Tokenizer enum (proxy / cl100k / o200k / p50k); BPE feature-gated

  # Wiki-substrate — code-intelligence signals for downstream runners
  identifiers.rs        — `sigil identifiers` — symbol-shaped token extraction
  decisions.rs          — `sigil decisions` — WHY:/DECISION:/RATIONALE:/TRADEOFF:/ADR:/REJECTED: marker scan
  package_deps.rs       — `sigil package-deps` — go.mod / package.json edges
  contracts.rs          — `sigil contracts [--root <repo-or-workspace>]` —
                          extracts HTTP / WebSocket / gRPC / topic / task /
                          RPC / GraphQL / db contracts across Python/JS-TS/
                          Go/Rust/Java/Kotlin/Ruby/PHP/C# +
                          .proto/.graphql/.sql/.yaml/.json. Workspace-aware
                          via `extract_workspace_or_repo`. `ContractRow.kind`
                          enum: `http | websocket | event | grpc | topic |
                          task | rpc | graphql | db`. Env-var-aware:
                          `os.environ['X']` / `process.env.X` / `ENV['X']` etc.
                          become `topic::$ENV.<NAME>` contracts, resolved
                          via `.env`/`docker-compose.yml` at workspace match
                          time.
  workspace.rs          — `sigil workspace {init,add,remove,enable,disable,
                          set-default,list,index,install,uninstall,scan,
                          resolve}` — explicit multi-repo membership +
                          cross-repo intelligence (call graphs, co-change,
                          contract matching with confidence tiers `1.0`
                          literal / `0.9` env_value / `0.8` mixed / `0.6`
                          env_name unresolved / `0.4` env_name disagree;
                          reverse-proxy URL rewrite awareness for
                          nginx/Caddy/Vercel; DuckDB workspace backend
                          auto-engaging at 5 MB). See
                          WORKSPACE_INDEXING_PLAN.md for the 6-phase design.
                          Output JSONL artifacts (`cross_repo_refs.jsonl`,
                          `contract_links.jsonl`, `contracts.jsonl`,
                          `co_changes.jsonl`) are lexicographically sorted
                          for deterministic diffs. `rank.json` (workspace-
                          level PageRank over union graph, member-prefixed
                          file keys via `BTreeMap` for sorted JSON) is
                          written alongside; `load_rank_manifest` in
                          workspace.rs prefers it, falls back to merging
                          per-member `.sigil/rank.json` files.
  cross_repo_cochange.rs — `sigil cochange --workspace` — cross-repo file-pair
                          mining with exp-decay (τ=180d) + `min_strength=1.0`
                          + 200-edge cap (matches repowise). Workspace
                          integration via `mine_members` taking explicit
                          member paths.
  hotspots.rs           — `sigil hotspots` — file churn × line count risk score
  ownership.rs          — `sigil ownership` — per-file primary author from git log
  bus_factor.rs         — `sigil bus-factor` — per-file knowledge-concentration risk
  log_significant.rs    — `sigil log --significant` — intent-filtered git log per file
  security_scan.rs      — `sigil security-scan` — regex security-signal extractor
  dead_code.rs          — `sigil dead-code` — framework-aware dead-code detection
                          with confidence tiers (file 1.00, exported orphan 0.85,
                          internal helper 0.70); excludes Flask/FastAPI/Django,
                          chi/gin/echo, Express/NestJS route files and
                          `*Handler`/`*Plugin`/`*Service` dynamic-name exports
  heritage.rs           — `sigil heritage <symbol>` — heritage (embed/extend/impl) graph

  query/
    mod.rs               — Backend router (InMemory | DuckDb), format_* helpers
    index.rs             — In-memory Index: loads .sigil/ jsonl, hash-map lookups
    duckdb_backend.rs    — DuckDB-backed Index (feature = "db"); materialized index
                            at .sigil/index.duckdb with staleness stamp

  install/
    mod.rs               — Shared marker-scoped idempotent upsert helpers
    claude.rs            — CLAUDE.md + .claude/settings.json PreToolUse hook
    cursor.rs            — .cursor/rules/sigil.mdc (alwaysApply: true)
    codex.rs             — AGENTS.md + .codex/hooks.json Bash hook
    gemini.rs            — GEMINI.md + .gemini/settings.json BeforeTool hook
    opencode.rs          — AGENTS.md + .opencode/plugins/sigil.js
    aider.rs             — AGENTS.md block
    copilot.rs           — ~/.copilot/skills/sigil/SKILL.md
    githook.rs           — .git/hooks/post-commit + post-checkout auto-rebuild

python/
  Cargo.toml       — PyO3 crate (sigil-python) depending on sigil lib
  pyproject.toml   — maturin config; package name: sigil-diff, import name: sigil
  src/lib.rs       — Python bindings: diff_json, diff_files, diff_refs, index_json
  README.md        — Python API documentation

scripts/
  git-sigil               — Shim enabling `git sigil <cmd>` on PATH
  publish-npm.mjs         — Release-time helper: extracts each cargo-dist
                             per-target archive, stages a thin
                             @knova-run/sigil-<platform> npm package, then
                             stages and publishes the @knova-run/sigil
                             wrapper (esbuild-style optionalDependencies).
                             Run by the publish-npm job in release.yml.
  bootstrap-npm-stubs.mjs — One-time helper to claim the 5 platform package
                             names with 0.0.0 stubs so trusted publishers
                             can be configured per package before the first
                             real release.

evals/
  bench_multilang.py, compare_rg.py, corpus.tsv, cross_repo.sh, run.sh
  results/         — Benchmark writeups (ripgrep/fastapi/zod/cobra + self-benchmarks)
```

## Cargo features

- `db` — DuckDB backend (`dep:duckdb`); adds `sigil query 'SQL'` and the persistent
  materialized index at `.sigil/index.duckdb`. Auto-engages over 5 MB of JSONL
  (override with `SIGIL_AUTO_ENGAGE_THRESHOLD_MB`; force with `SIGIL_BACKEND=db|memory`).
- `tokenizer` — tiktoken-rs for BPE-accurate token counting in `sigil benchmark`.
- Per-language grammars gated as `lang-<name>` flags; default enables all 15.

## Key Dependencies

- **tree-sitter** — AST parsing (vendored in `src/parser/`, forked from codeix v0.5.0 under Apache-2.0; see `src/parser/NOTICE`)
- **blake3** — content hashing
- **similar** — line and word diffing
- **clap** — CLI argument parsing
- **colored** — terminal colors
- **serde / serde_json** — (de)serialization
- **anyhow** — error handling
- **toml** — TOML parsing
- **duckdb** (feature = db) — persistent columnar index backend
- **tiktoken-rs** (feature = tokenizer) — BPE tokenization

## Conventions

- All hashes are BLAKE3, truncated to 16 hex characters
- Entity output is sorted deterministically by (file, line_start)
- Incremental indexing: only re-parses changed files
- `sigil diff` shells out to git (no git2 dependency)
- `sigil diff` always exits 0 on success (error handling exits non-zero via `std::process::exit(3)`)
- `kind: "constant"` covers Python ALL_CAPS module/class assignments, Rust `const`/`static`, Go `const`/package-level `var`, TS/JS top-level `const NAME`, Java `static final`, C# `const`/`static readonly`, C++ `constexpr`/`#define`. `Entity.sig` is the literal RHS value text (truncated at 256 chars with `…`). Lowercase Python/JS variables stay `kind: "variable"` with the same sig wiring.
- `sigil where` includes constants in `DEFINITION_KINDS` — module-level tunables resolve like functions; variables and imports stay excluded.
- `Entity.doc` carries the author-provided description (Python docstring first-statement, Rust `///` / `/** */`, godoc, JSDoc `/** */` for JS/TS, Javadoc for Java, XML-doc `///` for C#, Doxygen `///` / `//!` / `/** */` / `/*! */` for C++) when present, truncated at 1024 chars. Surfaced in `code.context` markdown as a `## Doc` section between Signature and Body, and in the agent JSON view under short key `d`.
- `Entity.heritage` is an optional `Vec<HeritageEdge { kind, target }>` populated across 12 languages: `embed` (Go struct embedding), `extend` + `implement` (Java/TS/JS/Kotlin/Scala/C#/Swift/C++/PHP), `extend` only (Python — `ABC` subclassing + `metaclass=` keyword args), `trait_impl` (Rust `impl Trait for Type` + trait super-bounds `trait Sub: Super`). Empty vecs are elided from JSONL.
- `Reference.confidence` is an optional `f64` carried on edges resolved through a file-local symbol/import table or the post-index tier-3 pass. Tiers (repowise-aligned post-P5.17):
  - `0.95` — same-file bare-identifier call (caller and callee both in this file's symbol set). Verified by `resolve_tier3` in `src/index.rs`: optimistic 0.95 from a parser with no actual same-file match gets demoted, then re-considered for tier-3. Repowise's tier-1 value — leaves AST-uncertainty headroom (shadowing, nested-scope binding) even on a successful local match. `self`/`this` member-call resolution from `resolve_member_call` Strategy 3 also lands at this tier.
  - `0.93` — same-file known-class receiver: a `Class.method()` call where `Class` is a class entity defined in the same file as the caller and has the named method. Strategy 2 (same-file branch).
  - `0.88` — imported known-class receiver: a `Class.method()` call where `Class.method` resolves to exactly one definition globally (same language as the caller). Upgrades tier-2's bare 0.8. Strategy 2 (imported branch).
  - `0.85` — tier-2b imported-file fallback: a bare-name call where the caller has `import` entities (incl. Python `from X import *`), and exactly one of the resolved imported files defines the name as a callable. Closes the star-import gap. Implemented in `resolve_tier2b_imported_fallback` in `src/index.rs`.
- TS path aliases: `resolve_module_path` consults `tsconfig.json` `compilerOptions.paths` at the index root, longest-prefix wins. Enables tier-3 barrel-follow (and tier-2b) to fire for aliased imports like `import { x } from "@/utils"` when `"@/*": ["src/*"]`. Only `*`-suffixed pattern/target pairs are honored today; exact-match aliases and multi-target arrays are a follow-up.
- Go module paths: `load_go_modules` walks the index root for `go.mod` files (multi-module monorepos supported; `vendor/`, `node_modules/`, `.git/`, `.sigil/`, dotdirs skipped; depth-bounded at 4). For tier-2 0.8 call edges of the form `<canonical>/<func>` whose canonical prefix matches a workspace module, `resolve_go_module_imports` emits an additional 0.7 edge `<file>/<func>` pointing at the actual `.go` file. Third-party imports (no matching `go.mod`) are intentionally not promoted.
- PHP PSR-4 autoload: `load_php_psr4` reads `composer.json` (`autoload.psr-4` + `autoload-dev.psr-4`) at the index root, building a longest-prefix namespace → directory map. For tier-2 0.8 PHP call edges, `resolve_php_psr4_imports` splits the canonical namespace path, PSR-4s the namespace prefix to a directory, and scans for a callable matching the trailing leaf. Emits a 0.7 file-resolved edge. Today covers the `use function Foo\bar` import path; static-method and instantiation refs require parser-side support before they can be upgraded.
- Rust Cargo workspace: `load_cargo_workspace` reads the root `Cargo.toml`, expands `[workspace] members` glob patterns (`crates/*`), parses each member's `[package] name`, and registers both hyphen and underscore variants (mirrors rustc's import aliasing — `use my_crate` for `name = "my-crate"`). For tier-2 0.8 Rust call edges of form `<crate>::<path>/<rest>`, `resolve_cargo_workspace_imports` looks up the crate, scans `.rs` files under its directory for a callable matching the trailing name, and emits a 0.7 file-resolved edge.
- Ruby Rails autoload: `resolve_rails_autoload` follows the Rails CamelCase → snake_case file-naming convention (`UserMailer` → `user_mailer.rb`) under `app/**` and `lib/**`. For each `.rb` call ref of form `ClassName.method` that matches a Rails-conventional file defining the class with that method, emits a 0.7 file-resolved edge. `app/` paths win over `lib/` on ties (mirrors default Rails autoload paths). Additive to Strategy 2 (P0.2): both can fire — Strategy 2 binds at 0.88 when global-unique, this pass adds the file pointer; when global is ambiguous, only this pass fires.
- Swift SPM: `load_swift_spm` walks for `Package.swift` files and extracts `.target(name:)`, `.executableTarget(...)`, `.testTarget(...)`, `.systemLibrary(...)`, `.binaryTarget(...)`, `.plugin(...)` declarations via regex + paren-balance (no Swift parser dependency — mirrors repowise's approach). `path:`-less targets default to `Sources/<name>` (or `Tests/<name>` for test targets). `resolve_swift_spm_imports` matches bare-name `.swift` calls against callables under the caller's imported target directories — when exactly one target's directory yields a hit, emits a 0.7 file-resolved edge. Disambiguates cases where global-unique sees the same function name in multiple modules.
- JVM FQN imports (Kotlin + Scala): `resolve_jvm_fqn_imports` uses the standard `<root>/<pkg-as-dirs>/<File>.<ext>` layout to disambiguate same-named symbols across packages. For each bare `.kt`/`.kts`/`.scala` call ref, scan the file's `import com.x.y.<name>` entries; when exactly one file under `*/com/x/y/` defines the callable, emit a 0.7 file-resolved edge. Tries two needle forms: prefix-before-leaf (Kotlin top-level) and prefix-before-`Class.leaf` (Scala member). Path-based — no `settings.gradle(.kts)` / `build.sbt` parsing required for the standard layout; non-standard `srcDirs(...)` overrides are a follow-up.
- C/C++ `#include`: `load_compile_commands` reads `compile_commands.json` (at root or `build/`), tokenises each entry's `arguments` array (or `command` string), extracts `-I`/`-isystem`/`-iquote` dirs (joined and split forms), and resolves them relative to each entry's `directory`. For each `.c`/`.cpp`/`.h`/etc. call ref whose caller has a matching `#include`, `resolve_cpp_includes` probes (1) compile_commands include dirs, then (2) the importer's directory. When exactly one resolved header defines the called name, emits a 0.7 file-resolved edge. Mirrors repowise's `cpp.py` 3-step ladder minus the stem-match fallback (over-binds on header/impl pairs).
- C# .NET: `load_dotnet_index` walks for `.csproj`, `.sln`, and `.cs` files (skipping `bin/obj/.vs/packages/TestResults/node_modules`). Builds a `DotNetIndex` with: a namespace → file map (regex over both block-form `namespace Foo {` and C# 10+ file-scoped `namespace Foo;`), file → project map (longest csproj-dir prefix wins), per-project ProjectReference and PackageReference sets, and per-project global+implicit usings (built from `<ImplicitUsings>enable</ImplicitUsings>` + the default + Web SDK sets when an `Microsoft.AspNetCore*` PackageReference is present + `<Using Include="X"/>` items + `global using X;` directives scanned across the project's `.cs` files). `parse_sln_csproj_paths` surfaces orphan csprojs declared only in solution files (regex over the Visual Studio `Project("{type-guid}") = "Name", "rel\\path.csproj", ...` line format; folder type-GUID `2150E333-...-46DE8` skipped). `resolve_csharp_usings` resolves `Class.Method` calls by intersecting candidate files (via `namespace_map`) with the caller's `using` directives + project globals, ranks by same-project → referenced-project → anywhere, and emits a 0.7 file-resolved edge.
- `Reference.callee_id` is an optional stable per-symbol identifier of form `<file>::<symbol-path>` (e.g. `src/foo.rs::Foo::bar` for a method, `src/foo.rs::helper` for a top-level callable). Populated by every manifest resolver when it binds the edge to a specific file + entity. Lets downstream consumers (heritage, blast-radius, IDE jump-to-def) reach the target entity without re-doing name matching. Old refs.jsonl rows round-trip as None — additive schema change.
- `external:` sentinel entities: `emit_external_sentinels` walks import entities after tier-3 resolution and emits a synthetic `{kind: "external", name: "external:<modpath>", file: "<external>"}` entity for every unique import target that doesn't resolve to a workspace file. Languages with resolvers today: Go (via `go.mod` prefix match), Rust (via cargo workspace `crates` map), Swift (via SPM `targets` map), JS/TS (via `resolve_module_path` + tsconfig paths), Python (via `resolve_module_path`). Other languages skip emission conservatively until they have richer resolvers (avoids false-positive sentinels). The `<external>` file marker lets downstream consumers filter by path; `kind == "external"` lets them filter by type.
  - `0.8` — call resolved via a file-local import alias to a fully-qualified path, with a second edge in `<import-path>/<rest>` form (e.g. Go `fmt.Println` → `fmt/Println`, Python `np.array` after `import numpy as np` → `numpy/array`, Rust `Baz::greet` after `use foo::Bar as Baz;` → `foo::Bar/greet`). Tier-2 ships for Go, Java, Kotlin, PHP, Python, JS, TS, Scala, and Rust; C# is deferred pending tree-sitter-c-sharp grammar work.
  - `0.7` — tier-3 barrel-follow: an additional edge layered on a tier-2 0.8 edge when the import lands on a JS/TS `index.{ts,js}` / Python `__init__.py` style barrel that re-exports from elsewhere. Points at the underlying definition file. JS/TS and Python only — other languages need manifest-aware path resolvers (Cargo.toml, go.mod, composer.json PSR-4, etc.) as a separate follow-up.
  - `0.5` — tier-3 global-unique: a bare-name call with no same-file match and no file-local import binding, where exactly one callable definition with that name exists across the index (language-gated).
  - `None` — unresolved.
- Tier-3 runs by default during `sigil index`. Opt out with `sigil index --no-tier3` for strict-only call graphs.
- In-memory index lookup symmetry (`src/query/index.rs`): `refs_by_name` and `entities_by_name` index each entry under (a) its literal name, (b) its `bare_leaf` (segment after the latest `.` or `::`), and (c) the immediate head + its bare leaf via `head_prefixes_with_sep`, run TWICE — once for `::` separators (`Regex` from `Regex::new`), once for `.` separators (`Connection` from `Faraday::Connection.new`). Both passes gate single-segment heads on uppercase first-char to avoid module-namespace pollution (`callers std`, `callers crate`, `callers parser`, `callers requests` stay empty even though `std::*`, `crate::*`, `crate::parser::*`, `requests.*` refs exist). `cc_head_prefixes` is preserved as a thin wrapper that fixes `sep=::` for backwards-compatible call sites. Together these let `callers Session` reach `requests.Session` (Python tier-2), `callers Regex` reach `Regex::new` (Rust associated fns), `callers Base` reach `Rack.Protection.Base#call` (Ruby), and `callers Connection` reach `Faraday::Connection.new` (Ruby mixed-separator chains). The DuckDB backend (`src/query/duckdb_backend.rs`) replicates the same expansion via SQL `LIKE` alternation so `Backend::load` returns identical row sets on either side of the 5 MB auto-engage threshold.
- `caller_prefixes` (`src/query/index.rs`) fans `refs_by_caller` out so `callees Base` reaches refs whose stored caller is `Sinatra::Base.foo` or `Rack.Protection.Base.call`. Walks both `::` and `.` separators, picking the longer head at each step, and emits the `bare_leaf` of each prefix so the inner class name is reachable even when the outer namespace is present.
- Go parser type-annotation emission covers (in addition to function parameter/return types and struct field types): method receivers (`func (e *Engine) Foo()`), composite literals (`&Engine{…}`/`Engine{…}`/`[]Engine{…}`), parenthesized type casts (`(*Engine)(nil)`, `(Engine)(x)` — tree-sitter parses these as `unary_expression`/`identifier` not `pointer_type`/`type_identifier`, so handled via `emit_cast_type_refs`), and var/const spec explicit type fields (`var x *Engine`). All emitted with `kind="type_annotation"`, `confidence=None`.
- JSX `<Component />` and Vue/Svelte/Astro `<Component>` template tags emit `kind="instantiation"` refs (`confidence=None`). For JS/TS the JSX parser arm handles `.tsx`/`.jsx`/`.ts`/`.js`; for SFCs the `<template>` scanner balance-counts nested `<template v-if>` blocks and normalises kebab-case (`<my-comp>` → `MyComp`). Both gate on uppercase first-char so DOM intrinsics (`<div>`, `<span>`) don't pollute. This is why `sigil callers MyComponent --kind instantiation` is the canonical "where is this component used?" lookup for React/Vue/Svelte codebases.
- `sigil dead-code --safe-only` filters to `confidence >= 0.85` (file-level + exported-orphan only; the default 0.70 also includes internal helpers). Each row has a `primary_owner` field populated from `sigil ownership` git log scan. External sentinel entities (file=`<external>`) and non-source files (`.md`, `.toml`, `.json`, etc. — see `is_non_source_file` in `src/dead_code.rs`) are excluded from both `dead-code` and `communities` to avoid polluting either output.
- JSON diff: parent-aware matching `(file, parent, name)` prevents cross-matching (e.g., `body.text` vs `header.text`)
- JSON diff: `_`-prefixed fields are marked as derived and suppressed from output
- JSON diff: array items expanded with identity key heuristic (`id` > `key` > `name` > `text` > `type`), positional fallback
- JSON diff: minified JSON auto-formatted before parsing for correct per-entity hashing
- JSON diff: parent objects suppressed when children carry the detail; qualified names used (e.g., `body.text`)
- Python bindings: `pip install sigil-diff`, `import sigil`; built via PyO3 + maturin

## Useful Commands

```bash
# Build the index (produces .sigil/entities.jsonl + refs.jsonl + rank.json)
sigil index -v
sigil index --full        # force re-parse
sigil index --no-rank     # skip PageRank + blast radius

# Structural diff
sigil diff HEAD~1
sigil diff main..HEAD --markdown           # PR-ready
sigil diff HEAD~1 --json --pretty          # script input
sigil diff --files old.py new.py           # no git required
sigil diff HEAD~1 --summary --group --lines --context 5

# Agent-facing (Phase 1)
sigil map --tokens 2000 [--write]          # codebase digest → .sigil/SIGIL_MAP.md
sigil map --top-entities-per-subsystem 5   # adds top_entities[] to each subsystem
sigil context Entity --budget 1000         # minimum-viable symbol context (incl. doc)
sigil review HEAD~3..HEAD [--markdown]     # diff + blast + co-change
sigil blast Entity --depth 5               # impact summary
sigil benchmark --tokenizer o200k_base     # BPE-accurate token reduction

# Navigation (script-facing, unbounded, JSON-friendly)
sigil explore
sigil search "parse" --scope symbol
sigil symbols src/main.rs
sigil children src/entity.rs Entity
sigil callers struct_hash [--kind call|import|type_annotation|instantiation]
sigil callees build_index
sigil duplicates --min-lines 10
sigil cochange --commits 500               # → .sigil/cochange.json
sigil communities --resolution 1.0         # Leiden file clusters (NDJSON)
sigil communities --pretty                 # pretty-printed JSON array form
sigil heritage Embedder                    # heritage graph (in/out edges)

# DuckDB (baked into shipped release binaries since 0.3.2)
sigil query "SELECT kind, COUNT(*) FROM entities GROUP BY kind ORDER BY 2 DESC"

# Agent / editor integrations (idempotent, content-preserving)
sigil claude install    # and: cursor / codex / gemini / opencode / aider / copilot / hook
sigil <name> uninstall  # matching uninstaller for each
```
