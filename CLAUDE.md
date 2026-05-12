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
  rank.rs          — File-level PageRank + per-entity blast-radius BFS (pure fn)
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
  contracts.rs          — `sigil contracts` — HTTP routes, gRPC services, queue topics
  workspace.rs          — `sigil workspace scan` — discover child git repos
  cross_repo_cochange.rs — `sigil cochange --workspace` — cross-repo file-pair mining
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
- `Entity.heritage` is an optional `Vec<HeritageEdge { kind, target }>` populated for Go struct embedding today (kind `"embed"`). Empty vecs are elided from JSONL. The on-disk shape is forward-compatible with future kinds (`"extend"`, `"implement"`, `"trait_impl"`).
- `Reference.confidence` is an optional `f64` carried on edges resolved through a file-local symbol/import table or the post-index tier-3 pass. Tiers:
  - `1.0` — same-file bare-identifier call (caller and callee both in this file's symbol set). Verified by `resolve_tier3` in `src/index.rs`: optimistic 1.0 from a parser with no actual same-file match gets demoted, then re-considered for tier-3.
  - `0.95` — `self`/`this` member-call resolution: a `self.X()` (Python/Ruby) or `this.X()` (Java/Kotlin/JS/TS/C#/Swift) call where `X` matches a method on the caller's own class, looked up in a (file, class, method) index built from `kind=="method"` entities. Same-file by construction, unambiguous binding. Implemented in `resolve_member_call` Strategy 3 in `src/index.rs`.
  - `0.93` — same-file known-class receiver: a `Class.method()` call where `Class` is a class entity defined in the same file as the caller and has the named method. Strategy 2 (same-file branch).
  - `0.88` — imported known-class receiver: a `Class.method()` call where `Class.method` resolves to exactly one definition globally (same language as the caller). Upgrades tier-2's bare 0.8. Strategy 2 (imported branch).
  - `0.85` — tier-2b imported-file fallback: a bare-name call where the caller has `import` entities (incl. Python `from X import *`), and exactly one of the resolved imported files defines the name as a callable. Closes the star-import gap. Implemented in `resolve_tier2b_imported_fallback` in `src/index.rs`.
- TS path aliases: `resolve_module_path` consults `tsconfig.json` `compilerOptions.paths` at the index root, longest-prefix wins. Enables tier-3 barrel-follow (and tier-2b) to fire for aliased imports like `import { x } from "@/utils"` when `"@/*": ["src/*"]`. Only `*`-suffixed pattern/target pairs are honored today; exact-match aliases and multi-target arrays are a follow-up.
- Go module paths: `load_go_modules` walks the index root for `go.mod` files (multi-module monorepos supported; `vendor/`, `node_modules/`, `.git/`, `.sigil/`, dotdirs skipped; depth-bounded at 4). For tier-2 0.8 call edges of the form `<canonical>/<func>` whose canonical prefix matches a workspace module, `resolve_go_module_imports` emits an additional 0.7 edge `<file>/<func>` pointing at the actual `.go` file. Third-party imports (no matching `go.mod`) are intentionally not promoted.
- PHP PSR-4 autoload: `load_php_psr4` reads `composer.json` (`autoload.psr-4` + `autoload-dev.psr-4`) at the index root, building a longest-prefix namespace → directory map. For tier-2 0.8 PHP call edges, `resolve_php_psr4_imports` splits the canonical namespace path, PSR-4s the namespace prefix to a directory, and scans for a callable matching the trailing leaf. Emits a 0.7 file-resolved edge. Today covers the `use function Foo\bar` import path; static-method and instantiation refs require parser-side support before they can be upgraded.
  - `0.8` — call resolved via a file-local import alias to a fully-qualified path, with a second edge in `<import-path>/<rest>` form (e.g. Go `fmt.Println` → `fmt/Println`, Python `np.array` after `import numpy as np` → `numpy/array`, Rust `Baz::greet` after `use foo::Bar as Baz;` → `foo::Bar/greet`). Tier-2 ships for Go, Java, Kotlin, PHP, Python, JS, TS, Scala, and Rust; C# is deferred pending tree-sitter-c-sharp grammar work.
  - `0.7` — tier-3 barrel-follow: an additional edge layered on a tier-2 0.8 edge when the import lands on a JS/TS `index.{ts,js}` / Python `__init__.py` style barrel that re-exports from elsewhere. Points at the underlying definition file. JS/TS and Python only — other languages need manifest-aware path resolvers (Cargo.toml, go.mod, composer.json PSR-4, etc.) as a separate follow-up.
  - `0.5` — tier-3 global-unique: a bare-name call with no same-file match and no file-local import binding, where exactly one callable definition with that name exists across the index (language-gated).
  - `None` — unresolved.
- Tier-3 runs by default during `sigil index`. Opt out with `sigil index --no-tier3` for strict-only call graphs.
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
