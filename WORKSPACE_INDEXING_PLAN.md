# Workspace indexing — design plan

Build a single unified index across N sibling repos so call-graph,
heritage, and structural queries work cross-repo. Per-repo `sigil index`
is unchanged — workspace mode is additive and opt-in.

## Why

Sigil already understands one repo at a time. A typical engineering org
has 5–50 sibling repos under one umbrella (services + shared libraries +
SDKs). Today an agent has to:

  1. Index each repo separately.
  2. For "where is X used across the org?", run `sigil callers X` in
     every child repo and merge by hand.
  3. Cross-repo edges (an API call from service A into shared lib B) are
     invisible — they materialise as `external:<modpath>` sentinels in
     each repo's index.

The cross-repo follow-ups we already shipped paper around this:

- `sigil workspace scan` — discovers child repos
- `sigil workspace resolve` (issue #30, MVP) — re-binds external sentinels
  to provider repos at query time
- `sigil cochange --workspace` — file-pair coupling across repos via git
  history

Those are workspace-aware queries against per-repo indexes. **This plan
adds a workspace-aware INDEX**, so all subsequent queries see one graph.

## Non-goals

- Cross-language linking (a TS service calling a Python service). Sigil
  remains language-local per file. Cross-repo just merges separately-
  parsed per-language graphs.
- Replacing per-repo `.sigil/`. Per-repo indexes still ship and remain
  the primary unit of incremental work.
- A workspace manifest format. Discovery stays sibling-walk via
  `workspace::scan`.

## CLI surface

One new subcommand, no changes to existing commands.

```
sigil workspace index <root>          # build workspace-level merged view
sigil workspace index <root> --full   # force re-merge of all children
```

All query commands already take `--root` / `-r`. If `--root` points at a
directory containing a `.sigil-workspace/` directory, the command reads
the workspace index. If it points at a directory with a `.sigil/`, it
reads the per-repo index. No new flag.

Behind the scenes the dispatch is identical to today's `db`-feature
auto-engage: `Backend::load(root)` already picks among in-memory vs
DuckDB; we extend it to pick workspace vs per-repo first.

## Storage layout

```
<workspace-root>/
├── repo-a/
│   └── .sigil/              # unchanged per-repo index
│       ├── entities.jsonl
│       ├── refs.jsonl
│       ├── rank.json
│       └── cache.json
├── repo-b/
│   └── .sigil/
└── .sigil-workspace/        # NEW — workspace-level merged view
    ├── manifest.json        # version + per-child stamp (mtime + size)
    ├── entities.jsonl       # all children's entities, file-path prefixed
    ├── refs.jsonl           # all children's refs + cross-repo additions
    ├── rank.json            # PageRank computed across the merged graph
    └── index.duckdb         # optional, auto-engaged at the same 5 MB threshold
```

`.sigil-workspace/` is gitignored by the workspace-install hook (matches
`.sigil/`'s treatment). It can be regenerated from per-repo indexes at
any time — it's a derived artifact.

### Identity rules

- **File paths** in the merged JSONL are rewritten as `<repo>/<rel-path>`
  so `repo-a/src/foo.py` and `repo-b/src/foo.py` don't collide.
  Per-repo `.sigil/` keeps the unprefixed form unchanged.
- **Entity names** stay as the language-native qualified form. The
  existing `(file, name)` natural key already handles cross-repo
  same-name entities (`run` in repo-a/utils.py vs repo-b/utils.py both
  live under the same Index, just different `file` fields).
- **`Reference.caller`** stays as the caller's qualified name. Cross-
  repo refs carry the rewritten file path on `Reference.file`.

## Pipeline

```
                  ┌──────────────────────────┐
                  │  sigil workspace index   │
                  └────────────┬─────────────┘
                               │
                               ▼
   ┌──────────────────────────────────────────────────────┐
   │ 1. workspace::scan(root) → list of child repos      │
   └────────────────────┬─────────────────────────────────┘
                        │
        ┌───────────────┴─────────────────┐
        │ 2. For each child:              │
        │    - Read its .sigil/           │
        │    - Or call build_index(child) │
        │      if .sigil/ missing/stale   │
        │      (delegate to existing      │
        │      per-repo pipeline)         │
        └───────────────┬─────────────────┘
                        │
                        ▼
   ┌──────────────────────────────────────────────────────┐
   │ 3. Merge per-child entities + refs:                  │
   │    - Prefix `file` with `<repo>/`                    │
   │    - Append to workspace entities.jsonl / refs.jsonl │
   │    - Stamp each child (size + mtime) into manifest   │
   └────────────────────┬─────────────────────────────────┘
                        │
                        ▼
   ┌──────────────────────────────────────────────────────┐
   │ 4. Cross-repo resolution pass (extends PR #35):      │
   │    - For each external:<modpath> entity, scan        │
   │      sibling repos for a matching definition         │
   │    - Constrain via `package-deps` edges when         │
   │      available (declared dep A→B required)           │
   │    - Emit new Reference rows at workspace-resolve    │
   │      confidence tier (see below)                     │
   │    - Set callee_id = <repo>/<rel>::<symbol-path>     │
   └────────────────────┬─────────────────────────────────┘
                        │
                        ▼
   ┌──────────────────────────────────────────────────────┐
   │ 5. PageRank + blast-radius across the merged graph   │
   │    (reuses src/rank.rs, just bigger input)           │
   └────────────────────┬─────────────────────────────────┘
                        │
                        ▼
   ┌──────────────────────────────────────────────────────┐
   │ 6. (optional) materialise .sigil-workspace/index.duckdb │
   │    if total JSONL exceeds the 5 MB auto-engage gate  │
   └──────────────────────────────────────────────────────┘
```

## Cross-repo confidence tier

Extending the existing scale (see `CALL_RESOLVER_ROADMAP.md`):

| Tier      | What                                                       |
|-----------|------------------------------------------------------------|
| 0.95–0.85 | unchanged — same-file / known-class / receiver-aware paths |
| 0.80      | tier-2 import-alias resolution                             |
| 0.70      | tier-3 barrel + manifest resolvers                         |
| **0.60**  | **cross-repo via declared `package-deps` edge (NEW)**      |
| 0.50      | tier-3 global-unique                                       |
| **0.40**  | **cross-repo without dep-graph constraint (NEW, MVP)**     |
| None      | unresolved                                                 |

Two cross-repo tiers because the workspace might or might not have
`package-deps` edges. With them, we know A genuinely declares B as a
dep — that's a stronger signal than blind sibling-scan. Without them,
fall back to the MVP behaviour shipped in `sigil workspace resolve`.

## Incremental indexing

Per-repo `.sigil/cache.json` already handles incremental at the file
level (changed files only). The workspace layer adds a coarser tier:

- `.sigil-workspace/manifest.json` stores, per child:
  - `entities_len` + `entities_mtime_ms`
  - `refs_len` + `refs_mtime_ms`
- On `sigil workspace index`, compare each child's current fingerprint
  to the stored stamp:
  - **No change** → reuse the merged JSONL slice for that child.
  - **Changed** → re-read that child's JSONL and re-emit its slice.
- Cross-repo resolution re-runs only if any child changed.
- PageRank recomputes only if entities or refs changed.

Storage cost: a merge of 10 repos × 50 MB ≈ 500 MB of `.sigil-workspace/`.
For very large workspaces, the DuckDB backend is the answer (already
handles 1 M+ entities; see PostHog audit).

## Backend integration

Both backends already exist. They learn one new entry point:

- `Index::load_workspace(root: &Path) -> Result<Index>` — reads
  `.sigil-workspace/` instead of `.sigil/`. Otherwise identical.
- `Backend::load(root)` checks for `.sigil-workspace/` first. If present,
  builds against that. Falls back to `.sigil/`.

The DuckDB backend's existing `populate()` already reads JSONL with
`read_json(..., columns = ...)`. We point it at `.sigil-workspace/`
JSONL — no other change.

## Schema impact

No new fields on `Entity` or `Reference`. The existing schema already
suffices:

- `Entity.file` carries the workspace-prefixed path
- `Reference.callee_id` already carries `<file>::<symbol>` and can hold
  the workspace-prefixed file
- Cross-repo refs get `Reference.confidence = Some(0.6 | 0.4)` per the
  new tier

The `manifest.json` shape is new but versioned via the existing
`Stamp`/`schema_version` pattern from PR #34.

## Phasing

Each phase ships independently. Each is its own PR with TDD coverage.

### Phase 1 — workspace index command

- `sigil workspace index <root>` subcommand
- Walks child repos via `workspace::scan`
- Merges per-child JSONL into `.sigil-workspace/` with file-path rewrite
- Stamps a `manifest.json`
- New unit tests: 2-child merge, file-path prefix, stamp creation

Acceptance: `cat .sigil-workspace/entities.jsonl | jq '.file' | sort -u`
shows entries prefixed with each child's name.

### Phase 2 — workspace-aware backend load

- `Index::load_workspace`
- `Backend::load` prefers workspace over per-repo when both exist
- Query commands (`callers`, `callees`, `where`, `heritage`, `context`,
  `search`, `symbols`, `children`, `explore`, `outline`, `map`, `blast`,
  `duplicates`, `communities`) all work transparently against the
  workspace index without flag changes
- Integration test: cross-repo `callers` returns refs from multiple
  repos

Acceptance: with two indexed repos under `~/work/org/` and `sigil index`
having been run in each, `sigil --root ~/work/org workspace index` then
`sigil --root ~/work/org callers SomeSymbol` returns hits from both
children.

### Phase 3 — cross-repo resolution at index time

- Extend `resolve_externals` (already in `src/workspace.rs`) to write its
  results into the workspace `refs.jsonl` instead of stdout
- New confidence tier 0.4 (or 0.6 with `package-deps` evidence)
- Integration test: a real `external:<modpath>` in one repo gets a
  workspace-tier ref in the merged view

Acceptance: `sigil --root <root> callers <leaf>` returns the cross-repo
caller, marked with `confidence = 0.4` (or 0.6).

### Phase 4 — incremental re-merge

- Stamp-based child-change detection
- `--full` flag forces full re-merge
- Default re-merges only changed children + re-runs cross-repo + rank

Acceptance: touching one file in `repo-a`, re-indexing `repo-a`, then
`sigil workspace index <root>` re-merges only `repo-a`'s slice
(verifiable via timing or per-child stamp inspection).

### Phase 5 — DuckDB workspace backend

- Auto-engage at the same 5 MB threshold against
  `.sigil-workspace/entities.jsonl`
- Schema-version stamp covers both per-repo and workspace DBs

Acceptance: 10-repo workspace (~500 MB JSONL) opens against DuckDB and
`callers` returns < 100 ms.

### Phase 6 — `sigil workspace install` git hook

- Pre-commit / post-commit hook on the workspace root that runs
  `sigil workspace index` after any child repo's hook fires
- Optional — Phases 1–5 work without this

## Open questions

1. **Workspace manifest shape**. Today we discover children by walking
   for `.git/`. Some orgs use `nx.json` / `pnpm-workspace.yaml` /
   `Cargo.toml [workspace]` to declare members. Should `sigil workspace
   index` honour those when present? **Tentative answer**: yes, with
   a fallback to `workspace::scan`. Phase 4 follow-up.

2. **Package-deps as a hard constraint**. Phase 3 makes 0.6 vs 0.4 a
   confidence call. Should we ever REQUIRE a `package-deps` edge before
   emitting a cross-repo binding? **Tentative answer**: no — sigil's
   philosophy is "surface what's there, let consumers filter on
   confidence." A 0.4 ref is opt-in for any consumer that wants
   strictness.

3. **What about repos with no `.sigil/`?** Two options: auto-build via
   `build_index` (slow on first run), or skip and warn (faster but
   surprising). **Tentative answer**: auto-build, since users running
   `sigil workspace index` clearly want the workspace.

4. **Symbol-name collisions across repos**. `User` exists in both
   repo-a and repo-b. Current `(file, name)` key already disambiguates,
   but `sigil where User` will return both. Is this a feature or a bug?
   **Tentative answer**: feature. The result rows already carry the
   file path, so the user sees which repo each `User` lives in.

5. **CLI override**. If both `.sigil/` (at the workspace root, because
   the user happened to run `sigil index` there) AND `.sigil-workspace/`
   exist, which wins? **Tentative answer**: workspace wins. Per-repo at
   the workspace root makes no semantic sense (it would only index the
   workspace itself, not children).

6. **What about repowise / graphify parity**. Repowise has a notion of
   "wiki" that includes cross-repo. Not a goal to match feature-for-
   feature, but if there are obvious gaps the workspace index should
   close them. **Tentative answer**: defer until Phase 1–3 ship and we
   have real user feedback.

## Existing surface this builds on

- `src/workspace.rs::scan` — child-repo discovery (PR #14, "wiki-substrate")
- `src/workspace.rs::resolve_externals` — cross-repo MVP query (PR #35,
  issue #30)
- `src/package_deps.rs` — manifest dep edges (per repo, will roll up)
- `src/cross_repo_cochange.rs` — cross-repo cochange already shipped;
  pattern to mirror for workspace index
- `src/index.rs::build_index` — single-repo pipeline; the workspace
  index calls this for any child without an up-to-date `.sigil/`
- `src/rank.rs` — PageRank + blast-radius, takes `&[Entity], &[Reference]`
  — needs no change to run on the merged input
- `src/query/index.rs` — in-memory backend, schema-stable
- `src/query/duckdb_backend.rs` — DuckDB backend, schema-stable (post
  PR #34)
- `CHANGELOG.md` entries for workspace-related shipped work

## Estimated size

- Phase 1: ~200 LOC + tests
- Phase 2: ~100 LOC + tests
- Phase 3: ~150 LOC + tests (reuses `resolve_externals`)
- Phase 4: ~100 LOC + tests
- Phase 5: ~50 LOC (mostly path swap)
- Phase 6: ~80 LOC (install hook, optional)

Total: ~700 LOC for Phases 1–5. Comparable to a single typical
manifest-resolver shipped in PR #35.
