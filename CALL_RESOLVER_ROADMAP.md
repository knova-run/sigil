# Call resolver — status

Status doc for sigil's call-graph resolver, pitched against the repowise
behaviour that informed it. Originally a roadmap; most of it shipped via
PR #23 (call-graph accuracy + heritage across 13 languages) and PR #24
(Kotlin/Swift/Scala/PHP language support). Remaining gaps are tracked
as GitHub issues; see [Outstanding](#outstanding) below.

## Confidence tiers

`Reference.confidence` on a call ref, post-index:

| Confidence | Where it comes from |
|---|---|
| `0.95` | same-file tier-1 (verified bare-identifier); or member-call Strategy 3 (`self`/`this` receiver) |
| `0.93` | member-call Strategy 2 same-file (receiver names a class defined in this file) |
| `0.88` | member-call Strategy 2 imported (receiver names a globally-unique same-language class with this method) |
| `0.85` | tier-2b fallback — bare name appears as a callable in an imported file |
| `0.80` | per-parser tier-2 import-alias resolution |
| `0.70` | one-hop barrel-follow (JS/TS/Python) |
| `0.50` | global-unique name match (language-gated, last resort) |
| `None` | unresolved |

CLI: `sigil index --no-tier3` opts out of all tier-3 passes (including
member-call resolution, tier-2b, barrel-follow, manifest resolvers).

## What's shipped

Member-call resolution (`receiver.method()`), all strategies in
`src/index.rs::resolve_member_call`:
- Strategy 3 (`self`/`this`) — 0.95
- Strategy 2 same-file (known class) — 0.93
- Strategy 2 imported (global-unique class+method) — 0.88

Tier-2b imported-file fallback: `resolve_tier2b_imported_fallback` in
`src/index.rs` — 0.85 when a bare name resolves uniquely in an imported file.

Manifest-aware import resolvers (each parses the manifest at index root
and feeds `resolve_module_path` / language-specific tier-3 passes):
- **TS/JS** — `tsconfig.json paths` (`load_tsconfig_paths`, `apply_tsconfig_paths`)
- **Go** — multi-`go.mod` with vendor skip (`GoModules`, `resolve_go_import`)
- **PHP** — `composer.json autoload.psr-4` + `autoload-dev.psr-4`
- **Rust** — `Cargo.toml [workspace] members` + crate roots
- **Swift** — `Package.swift` `.target(path:)` declarations
- **C/C++** — `compile_commands.json` `-I/-isystem` (loaded from index root or `build/`)
- **Ruby** — Rails `config/application.rb` autoload + Zeitwerk conventions
- **C#** — `.csproj` / `.sln` / `<GlobalUsings/>` / NuGet (`src/index.rs:1396+`)

`external:<modpath>` sentinel entities for unresolved imports
(`src/index.rs:2573+`), so the edge is still surfaced for downstream
consumers (communities, dead-code, package-deps).

Granular `callee_id` on `Reference` (`<file>::<Parent>::<leaf>` form),
populated at every resolver site.

Confidence scale aligned with repowise's 0.95/0.93/0.88/0.85/0.80/0.70/0.50.

## Outstanding

Tracked in GitHub issues — pick from these to extend coverage:

- **#27 — Member-call Strategy 1 (module-alias receiver).** ~30 LOC.
  Resolve `alias.method()` where `alias` was bound by tier-2 import,
  at 0.88. Currently this case is partially covered by per-parser tier-2
  (uneven across languages, lower confidence than warranted).

- **#28 — Kotlin Gradle `settings.gradle(.kts)` resolver.** ~220 LOC.
  Today's JVM FQN resolution (`resolve_jvm_fqn_imports`,
  `src/index.rs:2199`) is path-based; non-standard `srcDirs(...)` and
  multi-module include layouts miss.

- **#29 — Scala sbt + Mill resolver.** ~195 LOC. Sister to #28; same
  shared JVM resolver entry, different manifest formats (`build.sbt`,
  `build.sc`).

- **#30 — `sigil workspace resolve` cross-repo subcommand.** New
  initiative (not a repowise port). Re-bind `external:<modpath>` nodes
  to entities in sibling sigil indexes, constrained by `sigil
  package-deps` edges. New ~0.4 confidence tier.

## Decided against (or shipped enough)

- **`file::__module__` synthetic caller** for module-level calls — low
  value in isolation; sigil emits `caller: null` for these, which
  downstream consumers handle.
- **Tier-2 for C# `using F = Ns.T;` aliased form** — blocked on
  tree-sitter-c-sharp grammar work. Live with this gap.
- **Luau resolver** — sigil doesn't parse Luau.

## Reference: repowise source

External checkout at `/Users/gaurav/Downloads/repowise/` (not under sigil's tree). Useful when porting:

- `packages/core/src/repowise/core/ingestion/call_resolver.py` — the
  three-tier resolver. `_resolve_member_call` at lines 247–313 is what
  Strategy 1 (#27) still needs.
- `packages/core/src/repowise/core/ingestion/resolvers/` — per-language
  manifest resolvers. `kotlin.py` (#28), `scala.py` (#29).

## TDD pattern for porting

Each port should follow `tests/tier3_resolver_integration.rs`:

1. One integration test per behaviour (not per implementation step).
2. Real fixtures staged in temp dirs via `fresh_dir` / `write` helpers.
3. Drive through `sigil index --stdout`; parse refs from stderr; assert
   on `Reference.confidence` + `Reference.name` (+ `callee_id` for
   member calls).
4. RED → GREEN per behaviour.
5. Update `CLAUDE.md` `Reference.confidence` section when adding a new tier.
