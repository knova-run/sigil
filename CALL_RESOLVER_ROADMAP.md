# Call resolver roadmap

Plan for closing the gap between sigil's call-graph resolver and repowise's.
Written for future agent sessions picking up this work without conversation
context. Pair with `tests/heritage_integration.rs` and
`tests/tier3_resolver_integration.rs` (existing TDD fixtures).

---

## Current state (as of feat/call-graph-accuracy, post-commit 21e784a)

Confidence tiers populated on `Reference.confidence`:

| Tier | Confidence | What | Where it runs |
|---|---|---|---|
| 1 | `1.0` | same-file bare-identifier call (verified in `index.rs`) | per-parser, validated post-pass |
| 2 | `0.8` | file-local import-alias resolution + 2-edge form `<path>/<rest>` | per-parser `resolve_<lang>_imports_tier2` |
| 3-barrel | `0.7` | one-hop barrel-follow through `index.{ts,js}` / `__init__.py` | `index.rs::resolve_barrel_follow` (JS/TS + Python only) |
| 3-global | `0.5` | global-unique name match, language-gated | `index.rs::resolve_tier3` |
| — | `None` | unresolved | default |

Tier-2 shipped for: Go, Java, Kotlin, PHP, Python, JS, TS, Scala, Rust.
Tier-2 deferred for: C# (`using F = Ns.T;` aliased form — tree-sitter-c-sharp
grammar work).

CLI: `sigil index --no-tier3` opts out of tier-3 passes.

---

## Reference: repowise source

All paths under `/Users/gaurav/Downloads/repowise/`. Read these when porting
specific behavior. (External checkout — not under sigil's tree.)

- `packages/core/src/repowise/core/ingestion/call_resolver.py` — the 3-tier
  resolver. Critical functions:
  - `_resolve_free_call` (lines 187-245) — tier-1/2/3 for `foo()` style
  - `_resolve_member_call` (lines 247-313) — receiver-aware for `x.foo()` (NOT YET PORTED)
  - `_follow_barrel_exports` (lines 98-110) — one-hop barrel heuristic
  - `_build_indices` (lines 112-152) — same-file/global/method index builds
- `packages/core/src/repowise/core/ingestion/resolvers/` — per-language
  import-path → file-path resolvers (the `external:` sentinel comes from here).
  See per-language table below.
- `packages/core/src/repowise/core/ingestion/resolvers/context.py` —
  `ResolverContext`, lazy per-language index slots, `add_external_node`.
- `docs/architecture/ARCHITECTURE.md` — edge taxonomy + confidence narrative.

---

## Gap analysis (verified by reading source, not inferred)

### Big gaps — measurable impact on call-graph completeness

1. **Member-call resolution (`receiver.method()`)** — *not started.*
   Sigil leaves all member calls at `None` or tier-2 `0.8`. Repowise has
   four strategies in `_resolve_member_call` totaling ~70 LOC:
   - **Strategy 1 / 1b** (0.88): receiver is a module alias → look in target file
   - **Strategy 2** (0.93 same-file / 0.88 imported): receiver is a known class name
   - **Strategy 3** (0.95): `self`/`this` → walk caller's parent class
   - **Strategy 4** (0.50): globally unique class with that name

2. **Tier-2b fallback** — *not started.*
   Repowise's `_resolve_free_call` at 0.85: when no specific binding matches,
   scan every imported file for a symbol with that name. We don't do this.

3. **Symbol-ID granularity** — *architectural difference.*
   Repowise emits `(caller_id, callee_id, confidence)` with IDs like
   `path/to/file.py::User::save`. We emit `Reference.name` as a string and
   `caller` as a name. Downstream consumers (e.g. heritage CLI, blast
   radius) re-do symbol lookup. Probably acceptable for now; revisit only
   if we add tools that need stable symbol IDs.

### Medium gaps — import-path resolution depth

Sigil's barrel-follow currently handles JS/TS + Python with inline
relative-path probing only. Verified repowise resolvers (per `resolvers/`
LOC + manifest parsing):

| Lang | Repowise | Manifests parsed | Sigil today | Port size |
|---|---|---|---|---|
| Python | thin (~30 LOC) | none — filesystem heuristics | matches (we have .py + /__init__.py) | done |
| TS/JS | substantial (~130) | `tsconfig.json paths`, workspaces | partial — no tsconfig paths, no workspaces | ~50 LOC |
| Go | substantial (~80) | multi-`go.mod` index, vendor skip | nothing | ~80 LOC |
| PHP | substantial (~105) | `composer.json autoload.psr-4` | nothing | ~80 LOC |
| Rust | substantial (~180) | `Cargo.toml [workspace] members` + crate roots | nothing | ~180 LOC |
| Swift | substantial (~120) | `Package.swift .target()` | nothing | ~120 LOC |
| Ruby | substantial (~130) | Rails `config/application.rb` autoload | nothing | ~130 LOC |
| Kotlin | substantial (~220) | `settings.gradle(.kts)` + per-module srcDirs | nothing | ~220 LOC |
| Scala | substantial (~195) | `build.sbt` + Mill `build.sc` | nothing | ~195 LOC |
| C/C++ | substantial (~110) | `compile_commands.json` `-I/-isystem` | nothing | ~110 LOC |
| C# | huge (~620) | `.csproj`+`.sln`+globalUsings+NuGet | nothing | ~620 LOC |
| Luau | substantial (~120) | none (Rojo deferred) | n/a — sigil doesn't parse Luau | skip |

### Minor gaps

- **Confidence scale**: repowise uses 0.95/0.93/0.90/0.88/0.85/0.50; we use
  1.0/0.8/0.7/0.5. The 1.0 vs 0.95 distinction is the most material —
  repowise leaves headroom for AST-level uncertainty even on same-file
  matches. Defer unless downstream consumers need the finer scale.
- **`external:` sentinel nodes**: repowise emits unresolved imports as
  `external:<modpath>` graph nodes so the edge is still surfaced. We
  silently drop unresolved imports. Worth adding when we tackle
  cross-repo workspace resolution.
- **`file::__module__` synthetic caller**: repowise tags module-level
  calls (outside any function) with a synthetic `__module__` symbol.
  We emit `caller: null`. Probably not worth porting in isolation.

---

## Prioritized work items (by impact / LOC)

### P0 — biggest payoff per line of code

**1. `_resolve_member_call` Strategy 3 (`self`/`this`)** — *highest priority.*

- *Why:* In OO codebases (Python, Java, Kotlin, Ruby, C#, Swift), most
  intra-class calls go through `self.foo()` / `this.foo()`. These are
  100% unambiguous (the binding is local to the class). Current sigil
  leaves them at `None`.
- *Estimated edges recovered:* For Python/Java/Kotlin/Ruby/C# codebases,
  probably **doubles** the resolved-edge count.
- *Code shape:* Build per-file class-method index
  `(file, class_name, method_name) → method_entity`. For each call ref
  whose name starts with `self.` / `this.`, look up the caller's parent
  class (from `caller` field's `Parent::method` form), then look up
  `(file, parent_class, method)` in the index. Emit at confidence 0.95.
- *Lives in:* New pass in `src/index.rs`, sibling to `resolve_tier3`.
  Call it `resolve_self_this`. Or fold into `resolve_tier3`.
- *TDD fixtures:* `tests/tier3_resolver_integration.rs` style. Python:
  `class Foo:\n  def a(self): self.b()\n  def b(self): pass\n`
- *Repowise source:* `call_resolver.py:289-302`

**2. `_resolve_member_call` Strategy 2 (known class receiver)** — *natural follow-on.*

- *Why:* Static-method calls like `User.create()` or `StringUtils.join()`
  resolve cleanly when the receiver is a class name in the import set.
  Builds on the same class-method index as Strategy 3.
- *Code shape:* For call refs with name `Head.method`, check if `Head` is
  a class entity. If same-file → 0.93. If imported (resolves via tier-2
  import) → 0.88.
- *Repowise source:* `call_resolver.py:273-288`
- *Test:* class in `a.py` with `def m(self):`, caller in `b.py` doing
  `from a import Foo; Foo.m()`.

**3. Tier-2b fallback (free-call, imported-file scan)** — *small but easy.*

- *Why:* Closes a real gap for `from utils import *` style (and other
  unbound-name resolutions). Modest accuracy bump.
- *Code shape:* In `resolve_tier3`, after global-unique check fails:
  if the bare name appears as a callable in any *imported* file (not
  same-file, not import-aliased), bind to the first match at 0.85.
- *Repowise source:* `call_resolver.py:228-234`
- *~15 LOC.*

### P1 — language-specific manifest resolvers

In descending order of likely real-world corpora hits:

**4. TS `tsconfig.json paths` resolver** — *high impact for TS monorepos.*

- *Why:* Real TS projects use path mappings (`"@/utils": ["./src/utils"]`).
  Without this, sigil's barrel-follow misses every aliased import in a
  TS monorepo.
- *Code shape:* Lazy-load `tsconfig.json` at index root. Build
  `paths → fs-path` map. In `resolve_module_path`'s JS/TS branch, try
  longest-prefix path match before falling through to relative probing.
- *Repowise source:* `resolvers/typescript.py` + the external
  `TsconfigResolver` (referenced in `__init__.py` as
  `from .ingestion.tsconfig_resolver import TsconfigResolver`).
- *~50 LOC.*

**5. Go `go.mod` multi-module resolution** — *clean spec, high impact for Go.*

- *Why:* Go imports are canonical (`github.com/x/y/sub`). Without
  reading `go.mod`, sigil can't know that `github.com/x/y` corresponds
  to the current repo's `internal/` or `pkg/` directory.
- *Code shape:* Walk repo for `go.mod`s (multi-module support), build
  `{canonical_prefix → fs_root}` map, longest-prefix match. Then locate
  the imported sub-path under that root.
- *Repowise source:* `resolvers/go.py` (~80 LOC, very clean).
- *Also need to handle:* `vendor/` skip.

**6. PHP `composer.json` PSR-4 autoload** — *clean spec, well-defined.*

- *Why:* All real PHP projects use PSR-4. Without it, namespace imports
  like `use App\Foo` can't be resolved to a file.
- *Code shape:* Parse `composer.json autoload.psr-4` + `autoload-dev.psr-4`,
  longest-prefix match, fall through to suffix-tolerant fallback.
- *Repowise source:* `resolvers/php_composer.py` (~80 LOC).

### P2 — heavier language resolvers (defer unless prioritized by corpus)

7. **Kotlin Gradle resolver** (~220 LOC) — needed for serious Kotlin/Android.
8. **Scala build-tool resolver** (~195 LOC) — sbt + Mill project graphs.
9. **Rust workspace resolver** (~180 LOC) — `Cargo.toml [workspace]`.
10. **Swift SPM resolver** (~120 LOC) — `Package.swift` targets.
11. **C/C++ compile_commands.json resolver** (~110 LOC) — header
    resolution via build database.
12. **Ruby Rails autoload** (~130 LOC) — `config/application.rb`.

### P3 — only if you have C# corpora

13. **C# `.csproj`/`.sln`/GlobalUsings resolver** (~620 LOC) — the
    heaviest port. Don't tackle unless C# is a primary target.

### P4 — cross-repo (separate initiative)

14. **`sigil workspace resolve` subcommand** — repowise doesn't have
    this. Cross-repo symbol resolution constrained by `sigil package-deps`
    edges. New tier ~0.4 confidence. See conversation transcript at
    `/Users/gaurav/.claude/projects/-Users-gaurav-Downloads-SuprSend-sigil/`
    for design sketch.

### P5 — schema / completeness

15. **`external:` sentinel nodes** for unresolved imports — needed
    before cross-repo work in P4.
16. **Symbol-ID granular `callee_id`** — only if downstream consumers
    need stable IDs (heritage CLI, blast radius, etc.).
17. **Confidence-scale realignment** to repowise's 0.95/0.93/0.90/0.88/
    0.85/0.50 — defer unless interop tests demand it.

---

## TDD pattern for porting (reuse the shape that worked)

Each port should follow the pattern in `tests/tier3_resolver_integration.rs`:

1. **One integration test per behavior** (not per implementation step).
2. **Real fixtures staged in temp dirs** via the `fresh_dir` / `write`
   helpers — no mocking.
3. **Drive through `sigil index --stdout`**, parse refs from stderr,
   assert on `Reference.confidence` + `Reference.name`.
4. **RED → GREEN** per behavior. Don't write all tests upfront.
5. **Update CLAUDE.md** `Reference.confidence` section as new tiers ship.

Reference helpers already exist in `tests/tier3_resolver_integration.rs`:
- `fresh_dir(tag)` — unique temp dir per test
- `write(dir, rel, contents)` — write fixture file
- `run_index_with_refs(dir, extra_args)` — invoke sigil, parse refs

---

## Picking up where we left off

If you're a future session and just want to start swinging:

- **Easiest first commit:** P0 item 3 (tier-2b fallback) — 15 LOC, 1 test.
- **Highest-impact first commit:** P0 item 1 (`self`/`this` resolution)
  — ~30 LOC + class-method index, 2-3 tests.
- **Most user-facing first commit:** P1 item 4 (TS tsconfig paths) — most
  TS users will notice the difference immediately.

Read `call_resolver.py` lines 247-313 before starting member-call work.
Read the relevant `resolvers/<lang>.py` before starting a manifest-aware
import resolver.

---

## Open questions / decisions deferred

- Should member-call resolution emit additional edges (like tier-2's
  two-edge pattern) or only upgrade existing edges? Currently tier-2
  emits both raw + resolved; tier-3 global-unique only upgrades. For
  member calls (Strategy 2-4) we'd probably emit additional resolved
  edges for analytic clarity.
- What's the right confidence floor where we *stop* emitting low-quality
  edges? Repowise emits down to 0.50. Below that the noise outweighs the
  signal. Currently sigil matches — no edges emitted under 0.50.
- For cross-repo (P4), what's the package-deps→resolve constraint
  shape? Probably: A's call to `X` only resolves to B's `X` if B is
  reachable from A via `sigil package-deps`. Design TBD.
