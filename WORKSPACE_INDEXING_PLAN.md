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
- Auto-discovery of workspace members. Membership is explicit and
  user-managed via `add` / `remove`. `workspace scan` is retained as a
  discovery HELPER for users who want to bulk-add, but it never writes
  to `members.json` automatically.

## CLI surface

Workspace membership is **explicit**, not auto-discovered. The user
opts each repo in with `add`, opts out with `remove`. `workspace index`
re-merges over only the declared members.

```
sigil workspace init [<dir>]
    # Bootstrap a workspace. Creates <dir>/.sigil-workspace/members.json
    # (empty members list, schema version 1). <dir> defaults to cwd.
    # Errors if .sigil-workspace/ already exists (idempotent re-init via
    # --force). Required before any add/remove/index call against <dir>.

sigil workspace add <repo-path> [--as <name>] [--root <workspace-dir>] [--disabled] [--description <text>]
    # Register a repo as a workspace member. Path is resolved with
    # `.` / `..` / `~` expansion and canonicalised to an absolute path.
    # Symlinks in the path are PRESERVED (we don't resolve through them —
    # users sometimes pin a stable symlink and rotate the target).
    # Members can live ANYWHERE on disk; they are not required to be
    # siblings of the workspace dir.
    # If the repo has no .sigil/, queues it for indexing on the next
    # `workspace index` run. Idempotent — adding the same canonical path
    # twice is a no-op (existing entry preserved, no fields overwritten).
    # --disabled adds the member in the disabled state (skipped at index
    # time until `workspace enable` flips it).

sigil workspace remove <repo-name-or-path> [--root <workspace-dir>]
    # De-register a repo. Drops the entry from members.json.
    # Per-repo .sigil/ at the removed repo is left untouched.

sigil workspace enable <repo-name-or-path> [--root <workspace-dir>]
sigil workspace disable <repo-name-or-path> [--root <workspace-dir>]
    # Flip the `disabled` flag on an existing member. Disabled members
    # are kept in members.json (so the entry survives across audits) but
    # are skipped by `workspace index`, union-load, and cross-repo
    # resolution. Lets users temporarily quarantine a stale repo without
    # losing their `--as` alias and description.

sigil workspace list [--root <workspace-dir>] [--json]
    # Print the current membership. Default human-readable; --json emits
    # one JSONL row per member, including the `disabled` flag.

sigil workspace index [--root <workspace-dir>] [--full]
    # Build / refresh .sigil-workspace/ over the registered members.
    # --full forces re-merge of every enabled member; default re-merges
    # only the ones whose .sigil/ changed since the last index.
    # Missing member paths (the repo dir was deleted / moved out from
    # under us) trigger a stderr warning and are SKIPPED for this run.
    # members.json is NEVER mutated by `index` — only by add/remove/
    # enable/disable. This means a transiently-unmounted repo recovers
    # silently when its path returns; a permanently-gone repo stays in
    # the manifest until the user runs `workspace remove` explicitly.

sigil workspace scan <root>
    # UNCHANGED — still walks for child .git/ dirs. Now positioned as a
    # *discovery helper* for callers who want to bulk-add: e.g.
    #   sigil workspace scan ~/org | jq -r .path | xargs -I{} sigil workspace add {}
    # Indexing no longer calls scan automatically.
```

### `--root` resolution (strict)

`--root` is the workspace selector for every workspace subcommand AND
for every query command (`callers`, `where`, `map`, etc.). The rules:

- `--root <dir>` is honored verbatim. If `<dir>/.sigil-workspace/`
  exists, workspace mode engages. Else if `<dir>/.sigil/` exists,
  per-repo mode engages. Else error.
- `--root .` (or omitted) defaults to **cwd's `.sigil/`** — i.e. the
  per-repo index for the repo the user is standing in. Never walks up
  looking for a parent workspace.
- A workspace requires an EXPLICIT `--root <workspace-dir>`. There is
  no implicit-discovery mode. Rationale: users frequently `cd` into a
  member repo to run a query; if `--root .` walked up to find the
  workspace, every per-repo query inside a workspace member would
  silently widen to the whole workspace. That's surprising and costly.

If both `.sigil/` and `.sigil-workspace/` exist at the same `--root`
(the user ran `sigil index` at the workspace root for some reason),
the workspace wins. Per-repo at the workspace root makes no semantic
sense — it would only index `.sigil-workspace/` files, not any member.

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
      "added_at": "2026-05-13T13:42:11Z",
      "description": "API service",
      "disabled": false
    },
    {
      "name": "shared-lib",
      "path": "/absolute/path/to/shared-lib",
      "added_at": "2026-05-13T13:43:05Z",
      "disabled": false
    }
  ]
}
```

Member entry schema:

- `name` — defaults to the path's basename; collisions trigger a
  numeric suffix (`repo-a`, `repo-a-2`) and surface a warning. The user
  can override with `sigil workspace add <path> --as <name>`.
- `path` — absolute, with `.` / `..` / `~` expanded. Symlinks are
  preserved (we do NOT resolve through them — see `add` semantics
  above). The natural key for deduplication and `remove <path>` lookup
  is `(name, path)` after canonicalisation.
- `added_at` — RFC 3339 UTC timestamp set by `add`. Untouched by any
  other subcommand (preserves audit trail).
- `description` — optional free-form text. Defaults to absent. Useful
  for orgs that share a workspace definition across teams.
- `disabled` — bool, defaults to `false`. Flipped by `workspace
  enable / disable`. Omitted from JSON when false to keep diffs small.

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

| Tier      | What                                                          |
|-----------|---------------------------------------------------------------|
| 0.95–0.85 | unchanged — same-file / known-class / receiver-aware paths    |
| 0.80      | tier-2 import-alias resolution                                |
| 0.70      | tier-3 barrel + manifest resolvers                            |
| **0.60**  | **cross-repo, unique match, direct `package-deps` edge (NEW)**|
| 0.50      | tier-3 global-unique                                          |
| **0.40**  | **cross-repo, unique match, no dep-graph evidence (NEW)**     |
| **0.30**  | **cross-repo, ambiguous (multiple matches in one or more providers) (NEW)** |
| None      | unresolved                                                    |

### Emission policy (permissive, ambiguity-demoting)

For each `external:<modpath>` sentinel in a focus repo, the resolver
scans every other enabled member for matching callable definitions.
What we emit depends on the match count:

- **Single match** (exactly one provider, exactly one matching file):
  - 0.6 if `package-deps` declares the focus → provider edge directly
    (see "Package-deps evidence" below)
  - 0.4 otherwise
- **Multiple matches** (either: one provider with several matching
  files, or several providers each with their own match): emit ALL
  matches at **0.3**, one tier below the corresponding single-match
  confidence. The query layer decides what to do with them; the index
  doesn't filter ambiguous candidates out.
- **Cap**: at most 10 emissions per sentinel. If more candidates
  exist, drop the excess deterministically by sort key `(provider_repo,
  provider_file)` and append a debug warning. Prevents pathological
  blow-up on common names (`run`, `init`, `main`).

### Package-deps evidence — direct edge required

The 0.6 tier requires a **direct** dependency edge in
`sigil package-deps` from the focus repo to the provider repo. That is:

- The focus repo's `package.json` / `go.mod` / `Cargo.toml` /
  `composer.json` / `requirements.txt` etc. declares a package that
  resolves to the provider repo (by name, by module path, or by
  workspace alias).
- **Transitive edges do NOT count.** Even if focus → middle → provider
  exists in the dep graph, the focus must declare provider directly to
  earn 0.6.
- **No `package-deps` integration in either repo** → everything falls
  back to 0.4 / 0.3 by definition. Sigil's `package_deps.rs` runs
  per-repo, so the workspace just unions those edges; nothing new to
  parse here.

Rationale: 0.6 should be a high-confidence "this binding is real, the
focus genuinely depends on the provider." Transitive ties through a
shim crate are noisier — we'd rather under-promise and let the user
opt in by adding the missing direct dep declaration.

Why three tiers, not two: the workspace might or might not have
`package-deps` edges, and even with them the matches might be
ambiguous. The three tiers separate three different uncertainty
sources: dep-evidence (0.6 vs 0.4), match-ambiguity (0.4 vs 0.3),
language uncertainty (already encoded in the higher tiers).

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

Ships seven subcommands together — they form one cohesive UX surface
(membership editing + index refresh):

- `sigil workspace init [<dir>] [--force]`
  Creates `<dir>/.sigil-workspace/members.json` with an empty members
  list at schema version 1. `<dir>` defaults to cwd. Errors if
  `.sigil-workspace/` already exists unless `--force` is passed (which
  preserves `members.json` if non-empty — never silently destructive).

- `sigil workspace add <repo-path> [--as <name>] [--description <text>] [--disabled] [--root <ws>]`
  Expands `.` / `..` / `~` and canonicalises the path to absolute form
  WITHOUT resolving through symlinks. Asserts the path exists and is a
  directory containing `.git/`. Upserts an entry in `members.json`.
  Default `name` = path basename; `--as` overrides; collisions get a
  numeric suffix (`repo-a`, `repo-a-2`) with a warning. Re-adding the
  same canonical path is a no-op (returns the existing entry; never
  overwrites `description` / `added_at` / `disabled`). `--root` is
  REQUIRED unless cwd already contains a `.sigil-workspace/`.

- `sigil workspace remove <name-or-path> [--root <ws>]`
  Drops the entry from `members.json`. Per-repo `.sigil/` at the
  removed repo is untouched. Lookup is by name OR by canonical path
  (so `remove ~/code/foo` and `remove foo` both work, regardless of
  which form the user typed at `add` time). Idempotent — removing an
  absent member emits a warning and exits 0.

- `sigil workspace enable <name-or-path> [--root <ws>]`
- `sigil workspace disable <name-or-path> [--root <ws>]`
  Flip `members[].disabled`. Disabled members survive in `members.json`
  but are skipped by `index`, union-load, and cross-repo resolution.

- `sigil workspace list [--root <ws>] [--json]`
  Prints membership. Default human-readable table; `--json` emits one
  JSONL row per member with all fields (incl. `disabled`).

- `sigil workspace index [--root <ws>] [--full]`
  Reads `members.json`. For each ENABLED member:
  - If `member.path` doesn't exist on disk → stderr warning, SKIP this
    member for the run, do NOT mutate `members.json`. The user is the
    only one who can remove members.
  - If `member.path/.sigil/` is missing or stale → auto-build via
    `build_index(member.path)`.
  - Stamp its `entities.jsonl` + `refs.jsonl` size/mtime into
    `.sigil-workspace/manifest.json`.
  Does NOT copy per-repo data into the workspace — the per-repo JSONL
  stays the source of truth. Phase 1 leaves `cross_repo_refs.jsonl`
  empty (Phase 3 fills it). Errors out (exit 2) if `members.json`
  doesn't exist (user must `workspace init` first) or contains zero
  ENABLED members.

New unit tests:
- `workspace_init_creates_empty_members_json`
- `workspace_init_errors_when_already_initialized`
- `workspace_add_creates_members_json_with_canonical_path`
- `workspace_add_expands_home_dir`
- `workspace_add_preserves_symlinks_in_path`
- `workspace_add_is_idempotent`
- `workspace_add_idempotent_preserves_description`
- `workspace_add_with_alias_overrides_name`
- `workspace_add_collision_appends_numeric_suffix`
- `workspace_add_with_disabled_flag`
- `workspace_remove_drops_member`
- `workspace_remove_by_path_works`
- `workspace_remove_warns_when_member_absent`
- `workspace_enable_disable_round_trip`
- `workspace_list_prints_table`
- `workspace_list_json_includes_disabled_flag`
- `workspace_index_stamps_each_member_jsonl`
- `workspace_index_skips_disabled_member`
- `workspace_index_warns_and_skips_missing_path`
- `workspace_index_does_not_mutate_members_json`
- `workspace_index_drops_stamp_for_removed_member`
- `workspace_index_errors_when_uninitialized`
- `workspace_index_errors_when_no_enabled_members`
- `workspace_index_auto_builds_member_missing_sigil`

Acceptance:
```
sigil workspace init ~/work/org
sigil workspace add ~/code/repo-a --root ~/work/org
sigil workspace add ~/code/repo-b --root ~/work/org --description "auth lib"
sigil workspace list --root ~/work/org --json
sigil workspace index --root ~/work/org
jq '.members' ~/work/org/.sigil-workspace/manifest.json
# → both members appear with non-zero entities_len + refs_len stamps
ls -la ~/work/org/.sigil-workspace/
# → members.json + manifest.json + (empty until Phase 3) cross_repo_refs.jsonl
sigil workspace disable repo-b --root ~/work/org
sigil workspace index --root ~/work/org
jq '.members | length' ~/work/org/.sigil-workspace/manifest.json
# → 1 (repo-b skipped while disabled; entry still in members.json)
sigil workspace remove repo-b --root ~/work/org
jq '.members | length' ~/work/org/.sigil-workspace/members.json
# → 1 (repo-b's entry gone for good)
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

## Decided design points (locked by /grill-me interview)

1. **Member location**: members can live anywhere on disk; not required
   to be siblings of `.sigil-workspace/`. `members.json` stores absolute
   paths with `.` / `..` / `~` expanded; symlinks preserved.
2. **`--root` resolution**: strict. Workspace mode requires explicit
   `--root <workspace-dir>` (or cwd containing `.sigil-workspace/`); we
   never walk up from a member to find a workspace. See the "--root
   resolution" section.
3. **Missing member at index time**: warn + skip; never mutate
   `members.json`. The user is the sole authority on membership.
4. **Member schema**: `name` / `path` / `added_at` / `description?` /
   `disabled` (default false). `disabled` is omitted from JSON when
   false to keep diffs small.
5. **Disable / enable**: separate subcommands (not a flag on `add`).
   Disabled members survive in `members.json` but are skipped at index
   time.
6. **Match emission**: permissive. Single match → 0.6 (w/ direct
   package-deps edge) or 0.4. Multiple matches in one or more providers
   → 0.3 each. Cap of 10 emissions per sentinel.
7. **Package-deps evidence (0.6 tier)**: direct edge required.
   Transitive deps don't count. No `package-deps` data → falls through
   to 0.4 / 0.3.

## Open questions

1. **Bulk-import from external manifests**. Some orgs already declare
   members in `nx.json` / `pnpm-workspace.yaml` / `Cargo.toml
   [workspace] members`. Phase 6 will offer `sigil workspace add
   --from-manifest <file>` as a convenience — never as the default
   discovery path. The user always sees the diff before it lands in
   `members.json`. Format for the diff preview is TBD (likely a unified
   diff of `members.json` printed to stderr with `--dry-run` always
   implied unless `--apply` is passed).

2. **Cross-repo dead-code / communities / heritage behavior**. Most of
   these "just work" against the union-loaded index: `heritage.rs`
   already scans every entity (verified), `dead_code.rs` counts callers
   (now cross-repo), `communities.rs` runs Leiden over the merged
   graph. Open question is whether `dead-code` needs a separate
   confidence threshold for "no callers found, but maybe an external
   consumer outside the workspace calls it" — currently treats the
   workspace as the closed world. Revisit after Phase 3.

3. **Concurrency / locking**. Two simultaneous `workspace index`
   invocations would race on `manifest.json` + `cross_repo_refs.jsonl`.
   Today's per-repo `sigil index` has no lock either; we just don't
   advertise concurrency. Defer until users hit it; trivially fixable
   with an advisory file lock at `.sigil-workspace/.lock`.

4. **Repowise / graphify parity**. Not a goal to match feature-for-
   feature. Defer until Phases 1–5 ship and we have user feedback.

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
