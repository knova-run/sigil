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

Workspace membership is **explicit**, not auto-discovered. The user
opts each repo in with `add`, opts out with `remove`. `workspace index`
re-merges over only the declared members.

```
sigil workspace add <repo-path> [--root <workspace-dir>]
    # Register a repo as a workspace member. Path is resolved + canonicalised.
    # If the repo has no .sigil/, queues it for indexing on the next
    # `workspace index` run. Idempotent — adding the same repo twice is a no-op.

sigil workspace remove <repo-name-or-path> [--root <workspace-dir>]
    # De-register a repo. Drops it from the merged view on the next
    # `workspace index` (or immediately if --rebuild is passed).
    # Per-repo .sigil/ at the removed repo is left untouched.

sigil workspace list [--root <workspace-dir>]
    # Print the current membership (one JSONL row per member).

sigil workspace index [--root <workspace-dir>] [--full]
    # Build / refresh .sigil-workspace/ over the registered members.
    # --full forces re-merge of every member; default re-merges only the
    # ones whose .sigil/ changed since the last index.

sigil workspace scan <root>
    # UNCHANGED — still walks for child .git/ dirs. Now positioned as a
    # *discovery helper* for callers who want to bulk-add: e.g.
    #   sigil workspace scan ~/org | jq -r .path | xargs -I{} sigil workspace add {}
    # Indexing no longer calls scan automatically.
```

All query commands already take `--root` / `-r`. If `--root` points at a
directory containing a `.sigil-workspace/` directory, the command reads
the workspace index. If it points at a directory with a `.sigil/`, it
reads the per-repo index. No new flag.

Behind the scenes the dispatch is identical to today's `db`-feature
auto-engage: `Backend::load(root)` already picks among in-memory vs
DuckDB; we extend it to pick workspace vs per-repo first.

### Why explicit membership

Walk-based discovery is wrong for real orgs:

- Mixed-purpose parent dirs (`~/Downloads/`, `~/code/`) contain repos
  the user never wants in the workspace.
- Vendor / submodule directories under a monorepo are git repos but not
  workspace members.
- Add/remove gives a clean editing surface and a stable manifest that's
  reviewable, scriptable, and committable.
- `workspace scan` is retained as a discovery helper for bulk-add.

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
    ├── members.json         # AUTHORITATIVE membership list (add/remove writes here)
    ├── manifest.json        # per-child stamp (mtime + size), schema version
    ├── entities.jsonl       # all members' entities, file-path prefixed
    ├── refs.jsonl           # all members' refs + cross-repo additions
    ├── rank.json            # PageRank computed across the merged graph
    └── index.duckdb         # optional, auto-engaged at the same 5 MB threshold
```

`.sigil-workspace/manifest.json` + `entities.jsonl` + `refs.jsonl` +
`rank.json` + `index.duckdb` are **derived** — regenerable from
`members.json` + each child's `.sigil/`. Safe to delete and rebuild.

`.sigil-workspace/members.json` is **authoritative** — it's the only
file the user cares about. Shape:

```json
{
  "version": 1,
  "members": [
    {
      "name": "repo-a",
      "path": "/absolute/path/to/repo-a",
      "added_at": "2026-05-13T13:42:11Z"
    },
    {
      "name": "shared-lib",
      "path": "/absolute/path/to/shared-lib",
      "added_at": "2026-05-13T13:43:05Z"
    }
  ]
}
```

`name` defaults to the path's basename; collisions trigger a numeric
suffix (`repo-a`, `repo-a-2`) and surface a warning. The user can
override with `sigil workspace add <path> --as <name>`.

`.sigil-workspace/` (including `members.json`) is **committable** —
unlike per-repo `.sigil/` which is auto-regenerated from source. The
membership list IS the workspace definition and should be reviewable
in PRs. The derived artifacts under it are gitignored by the
workspace-install hook.

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
   │ 1. Read .sigil-workspace/members.json (declared via  │
   │    `sigil workspace add` / `remove`). Error out with │
   │    a hint if it's empty.                             │
   └────────────────────┬─────────────────────────────────┘
                        │
        ┌───────────────┴─────────────────┐
        │ 2. For each member:             │
        │    - Read its .sigil/           │
        │    - Or call build_index(child) │
        │      if .sigil/ missing/stale   │
        │      (delegate to existing      │
        │      per-repo pipeline)         │
        └───────────────┬─────────────────┘
                        │
                        ▼
   ┌──────────────────────────────────────────────────────┐
   │ 3. Merge per-member entities + refs:                 │
   │    - Prefix `file` with member's `name/`             │
   │    - Append to workspace entities.jsonl / refs.jsonl │
   │    - Stamp each member (size + mtime) into manifest  │
   │    - Drop any stale slice for a member no longer     │
   │      in members.json (removed-since-last-index)      │
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

### Phase 1 — membership + workspace index command

Ships four subcommands together — they're meaningless individually:

- `sigil workspace add <repo-path> [--as <name>] [--root <ws>]`
  Resolves + canonicalises the path, asserts it has a `.git/`, then
  upserts an entry in `.sigil-workspace/members.json`. Creates the
  workspace dir on first call. Default `name` is the path's basename;
  `--as` overrides; collisions get a numeric suffix.

- `sigil workspace remove <name-or-path> [--root <ws>]`
  Drops the entry from `members.json`. Per-repo `.sigil/` at the
  removed repo is untouched. The next `workspace index` run will
  evict the member's slice from the merged JSONL.

- `sigil workspace list [--root <ws>] [--json]`
  Prints membership. Default human-readable; `--json` emits one row
  per member.

- `sigil workspace index [--root <ws>] [--full]`
  Reads `members.json`, merges per-member JSONL into
  `.sigil-workspace/entities.jsonl` / `refs.jsonl` with file-path
  rewrite (`<member.name>/<rel-path>`), stamps each member into
  `manifest.json`.

New unit tests:
- `workspace_add_creates_members_json_with_canonical_path`
- `workspace_add_is_idempotent`
- `workspace_add_with_alias_overrides_name`
- `workspace_add_collision_appends_numeric_suffix`
- `workspace_remove_drops_member`
- `workspace_remove_idempotent_when_member_absent`
- `workspace_list_prints_jsonl`
- `workspace_index_merges_two_member_jsonl_with_prefix`
- `workspace_index_evicts_slice_for_removed_member`
- `workspace_index_errors_when_no_members`

Acceptance:
```
sigil workspace add ~/code/repo-a --root ~/work/org
sigil workspace add ~/code/repo-b --root ~/work/org
sigil workspace list --root ~/work/org
sigil workspace index --root ~/work/org
jq '.file' ~/work/org/.sigil-workspace/entities.jsonl | sort -u
# → entries start with repo-a/ and repo-b/
sigil workspace remove repo-b --root ~/work/org
sigil workspace index --root ~/work/org
jq '.file' ~/work/org/.sigil-workspace/entities.jsonl | sort -u
# → only repo-a/ entries remain
```

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

Three change axes the workspace index reacts to:

1. **Member added** (new entry in `members.json`) → emit its slice for
   the first time.
2. **Member removed** (entry gone from `members.json`) → drop its slice.
3. **Member content changed** (per-repo `.sigil/entities.jsonl` mtime/size
   diverges from the stamp) → re-read and re-emit that slice.

`--full` flag forces re-merge of every member regardless of stamps.
Default skips unchanged members. Cross-repo resolution + PageRank
re-run iff any of the three axes fired.

Acceptance:
- Touching one file in `repo-a` + `sigil index` in `repo-a`, then
  `sigil workspace index --root <ws>`, re-merges only `repo-a`'s slice
  (verifiable via per-member stamp inspection).
- `sigil workspace add <new>` + `sigil workspace index` only re-emits
  the new member's slice.
- `sigil workspace remove <r>` + `sigil workspace index` drops `r`'s
  slice without touching the others.

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

1. **Bulk-import from external manifests**. The authoritative source is
   `sigil workspace add`, but some orgs already declare members in
   `nx.json` / `pnpm-workspace.yaml` / `Cargo.toml [workspace] members`.
   Should we offer `sigil workspace add --from-manifest <file>` that
   bulk-adds from those? **Tentative answer**: yes, but as a Phase 6
   convenience — never as the default discovery path. The user always
   sees the diff before it lands in `members.json`.

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

- Phase 1: ~350 LOC + tests (add / remove / list / index + members.json
  + manifest plumbing)
- Phase 2: ~100 LOC + tests (Backend::load workspace branch)
- Phase 3: ~150 LOC + tests (cross-repo resolution at index time;
  reuses `resolve_externals`)
- Phase 4: ~120 LOC + tests (incremental: add / remove / content axes)
- Phase 5: ~50 LOC (mostly path swap for DuckDB)
- Phase 6: ~100 LOC (`--from-manifest` bulk-add + optional git hook)

Total: ~770 LOC for Phases 1–5. Phase 1 is heavier than the original
estimate because the membership CLI (`add` / `remove` / `list`) ships
together — they're meaningless without each other.
