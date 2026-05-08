# Changelog

All notable changes to sigil are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org/spec/v2.0.0.html).

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
