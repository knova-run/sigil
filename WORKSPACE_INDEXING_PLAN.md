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

**Storage policy**: per-repo `.sigil/` remains the source of truth for
each member's own entities + refs + rank. The workspace directory
stores ONLY the cross-repo delta: membership, stamps, and refs that
didn't exist in any per-repo index. No duplication of per-repo data.

DuckDB is the one exception — when it auto-engages (≥5 MB merged
JSONL) it materialises a unified table for query speed. But the
materialised DuckDB file is a derived cache, rebuildable on the fly
from members.json + per-repo + workspace deltas.

```
<workspace-root>/
├── repo-a/
│   └── .sigil/              # unchanged per-repo SoT — owns this repo's data
│       ├── entities.jsonl
│       ├── refs.jsonl
│       ├── rank.json
│       └── cache.json
├── repo-b/
│   └── .sigil/
└── .sigil-workspace/        # NEW — workspace-level delta only
    ├── members.json         # AUTHORITATIVE membership list (add/remove writes here)
    ├── manifest.json        # per-member stamp (mtime + size of each .sigil/), schema version
    ├── cross_repo_refs.jsonl  # cross-repo Reference rows produced at index time
    ├── workspace_rank.json    # PageRank over the union (per-repo rank.json stays as is)
    └── index.duckdb           # optional — materialised union of per-repo JSONL + cross_repo_refs
```

**Why deltas-only and not a full merged copy:**

- **Storage**: 50-repo workspace at 50 MB each = 2.5 GB per-repo. A
  full merge would double that to ≈5 GB. Deltas keep workspace
  overhead tiny (a few MB of cross-repo refs).
- **Incremental**: child's `sigil index` updates only its own
  `.sigil/`. The workspace doesn't have to re-emit any per-repo data;
  it just re-stamps and re-runs the cross-repo resolver. No per-member
  slice maintenance, no eviction logic on `workspace remove` beyond
  forgetting the stamp.
- **Single source of truth per fact**: a ref defined in repo-a appears
  in exactly one file (`repo-a/.sigil/refs.jsonl`). A ref that crosses
  repos lives in `.sigil-workspace/cross_repo_refs.jsonl`. No risk of
  stale duplicates after a partial rebuild.
- **In-memory backend reads N+1 files** at workspace load time — fast
  for any realistic N. File-path prefixing (`<member.name>/<rel>`) is
  applied at LOAD time, not stored on disk.
- **DuckDB materialises** the union for query speed when it
  auto-engages. The materialised table contains the prefixed paths;
  the schema-version stamp from PR #34 already handles invalidation.

`.sigil-workspace/members.json` + `manifest.json` + `cross_repo_refs.jsonl`
+ `workspace_rank.json` total ≈ a few KB to a few MB. Committable;
sized to fit in a git diff.

`.sigil-workspace/index.duckdb` is **derived**, gitignored, rebuilt
on stamp mismatch.

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
   │ 3. Refresh workspace stamps + cross-repo deltas:     │
   │    - Stamp each member's .sigil/{entities,refs}.jsonl│
   │      (size + mtime) into manifest.json               │
   │    - DO NOT copy per-repo data into the workspace —  │
   │      it stays as-is in each member's .sigil/         │
   │    - Re-run cross-repo resolution iff any stamp      │
   │      changed (Phase 3 logic)                         │
   │    - Drop manifest entries for members no longer in  │
   │      members.json (removed-since-last-index)         │
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
   │ 5. PageRank + blast-radius across the union          │
   │    (load N per-repo + cross-repo deltas into one     │
   │    in-memory Index, run src/rank.rs over it,         │
   │    write workspace_rank.json). Per-repo rank.json    │
   │    stays as-is.                                      │
   └────────────────────┬─────────────────────────────────┘
                        │
                        ▼
   ┌──────────────────────────────────────────────────────┐
   │ 6. (optional) materialise .sigil-workspace/index.duckdb │
   │    by running read_json over each member's .sigil/   │
   │    + cross_repo_refs.jsonl, UNION ALL, then          │
   │    rewriting the `file` column with the member       │
   │    prefix. Auto-engaged at the 5 MB total threshold. │
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
  Reads `members.json`. For each member, ensures its `.sigil/` is fresh
  (auto-builds via `build_index(member)` if missing/stale), then stamps
  its `entities.jsonl` + `refs.jsonl` size/mtime into
  `manifest.json`. Does NOT copy per-repo data into the workspace —
  the per-repo JSONL stays the source of truth. Phase 1 leaves
  `cross_repo_refs.jsonl` empty (Phase 3 fills it).

New unit tests:
- `workspace_add_creates_members_json_with_canonical_path`
- `workspace_add_is_idempotent`
- `workspace_add_with_alias_overrides_name`
- `workspace_add_collision_appends_numeric_suffix`
- `workspace_remove_drops_member`
- `workspace_remove_idempotent_when_member_absent`
- `workspace_list_prints_jsonl`
- `workspace_index_stamps_each_member_jsonl`
- `workspace_index_drops_stamp_for_removed_member`
- `workspace_index_errors_when_no_members`
- `workspace_index_auto_builds_member_missing_sigil`

Acceptance:
```
sigil workspace add ~/code/repo-a --root ~/work/org
sigil workspace add ~/code/repo-b --root ~/work/org
sigil workspace list --root ~/work/org
sigil workspace index --root ~/work/org
jq '.members' ~/work/org/.sigil-workspace/manifest.json
# → both members appear with non-zero entities_len + refs_len stamps
ls -la ~/work/org/.sigil-workspace/
# → members.json + manifest.json + (empty until Phase 3) cross_repo_refs.jsonl
sigil workspace remove repo-b --root ~/work/org
sigil workspace index --root ~/work/org
jq '.members | length' ~/work/org/.sigil-workspace/manifest.json
# → 1 (repo-b's stamp gone)
```

### Phase 2 — workspace-aware backend load (union over N + 1 files)

- `Index::load_workspace(root)` — reads `members.json`, then for each
  member calls `read_jsonl::<Entity>` and `read_jsonl::<Reference>` on
  the member's `.sigil/`. Applies the `<member.name>/<rel-path>` file
  prefix as it loads each row (no copy to disk). Appends
  `cross_repo_refs.jsonl` rows last. Returns a single `Index`.
- `Backend::load(root)` checks for `.sigil-workspace/members.json`
  first; if present, builds against the workspace. Falls back to
  per-repo `.sigil/`.
- Query commands (`callers`, `callees`, `where`, `heritage`, `context`,
  `search`, `symbols`, `children`, `explore`, `outline`, `map`, `blast`,
  `duplicates`, `communities`) all work transparently against the
  workspace index without flag changes.
- Integration test: cross-repo `callers` returns refs from multiple
  repos via the union load.

Acceptance: with two indexed repos under `~/work/org/`, `sigil
workspace add` for each, then `sigil --root ~/work/org workspace index`,
`sigil --root ~/work/org callers SomeSymbol` returns hits from both
members — file paths in the output have the `repo-a/` / `repo-b/`
prefix.

### Phase 3 — cross-repo resolution at index time

- Extend `resolve_externals` (already in `src/workspace.rs`) to write
  its results into `.sigil-workspace/cross_repo_refs.jsonl` — a stable
  on-disk delta, distinct from any per-repo `refs.jsonl`
- New confidence tier 0.4 (or 0.6 with `package-deps` evidence)
- Integration test: a real `external:<modpath>` in one member gets a
  cross-repo ref row appended to `cross_repo_refs.jsonl`, and `Backend::
  load_workspace` surfaces it in `callers`

Acceptance: `sigil --root <root> callers <leaf>` returns the cross-repo
caller, marked with `confidence = 0.4` (or 0.6). The ref lives in
`.sigil-workspace/cross_repo_refs.jsonl` and nowhere else.

### Phase 4 — incremental cross-repo + rank refresh

Since per-repo data is not duplicated at the workspace level, the only
work the workspace index actually does on each invocation is:

1. Refresh `manifest.json` stamps for each member's
   `.sigil/entities.jsonl` + `refs.jsonl`.
2. Re-run cross-repo resolution iff any member's stamp changed
   (`Phase 3` logic).
3. Re-run PageRank iff any stamp changed or membership changed
   (writes `workspace_rank.json`).

`--full` flag forces all three even when nothing changed. Default
short-circuits when every member's stamp matches and members.json is
unchanged.

Acceptance:
- Touching one file in `repo-a` + `sigil index` in `repo-a`, then
  `sigil workspace index --root <ws>`, re-runs cross-repo + rank only
  (no per-repo data churn at the workspace).
- `sigil workspace add <new>` + `sigil workspace index` stamps the new
  member and runs cross-repo + rank.
- `sigil workspace remove <r>` + `sigil workspace index` drops `r`'s
  stamp and re-runs cross-repo + rank.
- No-op invocations (`workspace index` twice in a row) finish in well
  under a second on a 10-member workspace.

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

- Phase 1: ~280 LOC + tests (add / remove / list / index + members.json
  + manifest stamps — no per-repo data copy)
- Phase 2: ~150 LOC + tests (union-load: read N per-repo + cross-repo
  deltas, prefix file paths in memory, return one Index)
- Phase 3: ~150 LOC + tests (cross-repo resolution writes
  `cross_repo_refs.jsonl`; reuses `resolve_externals`)
- Phase 4: ~80 LOC + tests (stamp-based skip, no per-member slice
  maintenance because there's no per-member slice in the first place)
- Phase 5: ~100 LOC (DuckDB workspace mode: read_json over each
  member's .sigil/ + cross_repo_refs, UNION with member-name prefix,
  schema-version-stamp invalidation)
- Phase 6: ~100 LOC (`--from-manifest` bulk-add + optional git hook)

Total: ~760 LOC for Phases 1–5. Workspace-level on-disk JSONL stays
small (members.json + manifest.json + cross_repo_refs.jsonl, a few KB
to a few MB).
