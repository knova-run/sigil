# sigil

**Deterministic structural code intelligence for AI coding agents — and humans.**

sigil cuts the orientation tax AI coding agents pay on every new repo. Instead of grepping through a codebase line by line, agents ask sigil: _"who calls this?"_, _"what's in this file?"_, _"what changed in this PR?"_ — and get back structured answers from a parsed AST index, not text matches.

No LLM in the code path. No embeddings. No cloud. Just tree-sitter + BLAKE3 + PageRank.

```
Median: 35× fewer tokens per agent query on sigil's own source.
Peak:   252× on "focused context for one symbol".
```

Measured with the GPT-4o/o3 BPE tokenizer. Reproduce with `sigil benchmark` on your own repo.

---

## What it does

**Structural diff — the original power tool, with or without agents:**
- `sigil diff HEAD~1` — per-entity change list (struct / fn / class), classified breaking vs logic vs formatting via three BLAKE3 hashes. Line-level inline diffs with code context.
- `sigil diff main..HEAD --markdown` — pastes straight into a PR. Reviewers skim 20 entity-level bullets instead of wading through 800 lines.
- `sigil diff --files old.py new.py` — compares any two files without git. Works on all 11 code languages + JSON / YAML / TOML (e.g., `"port": 8080 → 8443` detected as a structural change, not just "line 14 changed").
- `sigil diff main..HEAD --json` — pipe into `jq` for CI gates.

**Impact & navigation — blast radius is the second headline:**
- `sigil blast <symbol>` — direct callers, direct files, transitive reach. Before touching a function, see how many files it breaks.
- `sigil callers <symbol>` — exact reference sites from the parsed AST. Not every string match grep catches.
- `sigil callees <caller>` — what a symbol depends on.
- `sigil symbols <file>` / `sigil children <file> <parent>` / `sigil search <q>` — precise AST lookups, not regex guesses.
- `sigil duplicates` — clone report (free — sigil already hashes entity bodies).

**For AI agents — one-shot primitives that fit a context window:**
- `sigil where <symbol>` — single-shot definition locator. Returns file + line + class + signature + overloads in one call. Replaces the grep-narrow-read_file chain.
- `sigil context <symbol>` — signature + callers + callees + related types + inheritance overrides, in ~500 tokens.
- `sigil map` — ranked codebase digest. Cold-start orientation in one tool call.
- `sigil outline [--path DIR]` — hierarchical top-level tree of classes + fns grouped by file.
- `sigil review A..B` — PR review: `diff` + blast radius + co-change misses. Replaces `git diff` for review.

On empty results sigil emits a `Did you mean: X, Y, Z?` hint on stderr so agents don't abandon the tool on the first wrong guess. First query on a fresh clone auto-runs `sigil index` — zero-config onboarding.

**For scripts & CI:**
- Every command supports `--json`; output is minified by default (pass `--pretty` for indented).
- `sigil query "SELECT ..."` for ad-hoc SQL against the materialized index.
- `sigil callers X --group-by file` collapses per-call-site output to a `{file: count}` map when you only need distribution.

### `git sigil` — the git-native alias

Agents that know `git diff` / `git log` discover `git sigil diff` / `git sigil map` naturally. Git auto-wires any `git-<name>` executable on `PATH` as a `git <name>` subcommand — no extra config, no aliases to maintain.

**Setup** — pick one. All three use only the installed `sigil` binary; none require a local clone of the repo.

```bash
# 1. Symlink the sigil binary (simplest). Works on macOS / Linux.
sudo ln -s "$(command -v sigil)" /usr/local/bin/git-sigil

# 2. No-sudo variant — install into ~/.local/bin (ensure it's on PATH).
mkdir -p ~/.local/bin
ln -s "$(command -v sigil)" ~/.local/bin/git-sigil

# 3. Inline shim (for filesystems where symlinks are awkward).
sudo tee /usr/local/bin/git-sigil > /dev/null <<'SHIM'
#!/bin/sh
exec sigil "$@"
SHIM
sudo chmod +x /usr/local/bin/git-sigil
```

Windows (PowerShell): create `git-sigil.cmd` anywhere on `PATH`:

```powershell
# adjust destination to any folder already on PATH
Set-Content -Path "$env:USERPROFILE\bin\git-sigil.cmd" -Value '@sigil %*'
```

Verify:

```bash
git sigil --version
git sigil diff HEAD~1
git sigil map --tokens 2000
git sigil review main..HEAD --markdown
```

Every `sigil <cmd>` works as `git sigil <cmd>`.

---

## Install

Pre-built archives for every supported platform ship on the [Releases page](https://github.com/knova-run/sigil/releases/latest). No Rust toolchain required.

**npm / npx** (any platform with Node ≥ 14):

```bash
npx sigil --help            # one-shot
npm install -g sigil        # persistent
```

**macOS / Linux** (one-liner):

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/knova-run/sigil/releases/latest/download/sigil-installer.sh | sh
```

**Windows** (PowerShell):

```powershell
irm https://github.com/knova-run/sigil/releases/latest/download/sigil-installer.ps1 | iex
```

**Manual** — grab the right archive for your platform from [Releases](https://github.com/knova-run/sigil/releases/latest), untar it, and drop the `sigil` binary anywhere on `PATH`:

```bash
# Example: aarch64 macOS
curl -LO https://github.com/knova-run/sigil/releases/latest/download/sigil-aarch64-apple-darwin.tar.gz
tar -xzf sigil-aarch64-apple-darwin.tar.gz
sudo mv sigil-aarch64-apple-darwin/sigil /usr/local/bin/
```

Available archives: `sigil-aarch64-apple-darwin.tar.gz`, `sigil-x86_64-apple-darwin.tar.gz`, `sigil-aarch64-unknown-linux-gnu.tar.gz`, `sigil-x86_64-unknown-linux-gnu.tar.gz`, `sigil-x86_64-pc-windows-msvc.zip`.

Single binary, ~20 MB. Every command ships in it: `index` / `diff` / `map` / `context` / `where` / `outline` / `review` / `blast` / `query` — plus the DuckDB backend for monorepo scale and a BPE-accurate tokenizer for `sigil benchmark`. No opt-in features, no partial builds.

### Building from source (optional)

```bash
git clone https://github.com/knova-run/sigil && cd sigil
cargo build --release
```

The default build produces the full binary (all 11 languages + DuckDB + tokenizer). Requires a C++17 toolchain for DuckDB (Xcode CLT on macOS, `build-essential` on Debian/Ubuntu, MSVC on Windows) — DuckDB sources are bundled. The compiled binary lands at `target/release/sigil`.

### Python bindings

```bash
pip install sigil-diff
```

```python
import sigil

result = sigil.diff_json(old_json_str, new_json_str)      # two JSON strings
result = sigil.diff_files("old.py", "new.py")             # any two files on disk
result = sigil.diff_refs(".", "HEAD~1", "HEAD")           # two git refs
entities = sigil.index_json(json_str)                     # parse a JSON blob into entities
```

See [python/README.md](python/README.md) for full API.

---

## 5-minute tour

Clone this repo as the demo corpus:

```bash
git clone https://github.com/knova-run/sigil && cd sigil
```

### 1. Index

```bash
sigil index
```

~2 seconds on sigil itself. Creates `.sigil/entities.jsonl`, `.sigil/refs.jsonl`, `.sigil/rank.json`. Incremental on re-runs — only touched files re-parse.

### 2. Cold-start orientation

```bash
sigil map --tokens 2000
```

Top files by PageRank over the import graph, top symbols per file ranked by blast radius. The artifact to hand an agent when it first enters your repo.

```
# Sigil Map
100 files, 2985 entities, 12963 refs · sigil 0.3.2

## Subsystems (7)
- **src/parser** (#20) — 12 file(s): src/parser/helpers.rs, src/parser/format.rs, ...
- **src/install** (#42) — 8 file(s): src/install/claude.rs, src/install/codex.rs, ...
- **src/query** (#10) — 6 file(s): src/query/index.rs, src/query/mod.rs, ...
...

## Top files by impact

### src/entity.rs — rank 0.0798 (rust, subsystem #10)
- struct **Entity** [public] — blast 15f/45c/192t
  `pub struct Entity`
- struct **Reference** [public] — blast 9f/24c/101t
...
```

### 3. Focused context on one symbol

```bash
sigil context Entity --budget 1000
```

```
# `Entity`

**struct** in `src/entity.rs`:7-35 · public · blast 15f/45c/192t

## Signature
`pub struct Entity`

## Callers (4)
- `is_public` _type_annotation_ `src/classifier.rs:116`
- `match_classify_enrich` _type_annotation_ `src/diff.rs:220`
- `EntityDiff` _type_annotation_ `src/diff_json.rs:36`
- _+40 more truncated by budget_

## Related types (4)
- `Entity` → `String` _type_annotation_ `src/entity.rs:8`
- `Entity` → `Option` _type_annotation_ `src/entity.rs:14`
- _+16 more_
```

~350 tokens — the minimum-viable context for editing `Entity`, or for answering "what is this thing?"

### 4. Structural diff

```bash
sigil diff HEAD~1                           # terminal view with colors
sigil diff main..HEAD --markdown            # paste into a PR
sigil diff HEAD~1 --json --pretty           # feed into jq / a script
sigil diff --files old_config.yaml new_config.yaml   # no git required
sigil diff HEAD~1 --summary                 # one-line "5 breaking, 3 logic, 2 formatting"
sigil diff HEAD~1 --group                   # cluster related entity changes
sigil diff HEAD~1 --lines --context 5       # show 5 lines of code context
```

Three BLAKE3 hashes per entity (`struct_hash`, `body_hash`, `sig_hash`) classify every change:

- **breaking** — `sig_hash` changed (public API surface moved)
- **logic** — `body_hash` changed but signature stable
- **formatting** — whitespace / comment only; `body_hash` unchanged

Sample markdown output on a real commit:

```markdown
## src/entity.rs
- 🔴 **breaking** struct `Entity` (L7-L35) — field added
    + pub visibility: Option<Visibility>,
- 🟡 **logic** fn `is_public` (L112-L118) — body refactored
- ⚪ **formatting** fn `name` (L45-L47)
```

Renames, moves, and refactors matched across the commit so "deleted `foo` + added `bar` with the same body_hash" becomes one renamed row, not two.

### 5. PR review (diff + blast + co-change)

```bash
sigil review HEAD~3..HEAD
```

The agent-shaped wrapper around `diff`: structural changes ranked by blast radius, plus co-change misses ("this commit touched `a.rs` — git history suggests `b.rs` usually moves with it"). Committable as a review artifact.

### 6. Navigation queries

```bash
sigil where get_default                # single-shot: file + line + class + signature
sigil callers Entity                   # exact reference sites
sigil callers Entity --group-by file   # {file: count} summary
sigil callees build_index              # what a function depends on
sigil symbols src/entity.rs            # all entities in a file
sigil symbols src/entity.rs --depth 1  # top-level outline only (95% smaller)
sigil outline --path src/parser/       # hierarchical view of a subtree
sigil search parse                     # substring symbol search
sigil blast Entity --depth 5           # impact summary
```

Wrong-name guesses are recoverable: `sigil where resolve_default` → stderr prints `Did you mean: get_default, lookup_default, resolve_color_default?` so the agent retries with the suggested name instead of falling back to grep.

### 7. Run the benchmark on your repo

```bash
sigil benchmark --refspec HEAD~3..HEAD
# BPE-accurate counts:
sigil benchmark --refspec HEAD~3..HEAD --tokenizer o200k_base
```

Prints a per-query table: bytes grep would produce vs bytes sigil produces, plus the median reduction ratio.

---

## Install into your AI agent

Each installer writes a directive block into the host agent's system context — a question→command flowchart ("where is X defined?" → `sigil where X`) plus a worked one-shot example. The phrasing is the same prompt shape that produced deterministic sigil-first paths in our evals (Sonnet treatment on the click-library find-method task: 2 turns, 5.5k tokens, byte-identical across N=3 seeds, 2.22× cheaper than grep-only control).

```bash
sigil claude install     # CLAUDE.md + .claude/settings.json PreToolUse hook
sigil cursor install     # .cursor/rules/sigil.mdc (alwaysApply: true)
sigil codex install      # AGENTS.md + .codex/hooks.json Bash hook
sigil gemini install     # GEMINI.md + .gemini/settings.json BeforeTool hook
sigil opencode install   # AGENTS.md + .opencode/plugins/sigil.js
sigil aider install      # AGENTS.md block
sigil copilot install    # ~/.copilot/skills/sigil/SKILL.md
sigil hook install       # git post-commit + post-checkout auto-rebuild
```

Each has a matching `uninstall`. Every installer is idempotent (rerunning with same content is a no-op), preserves user content outside sigil's marker block, and leaves sibling user hooks / rules / plugins untouched.

### Upgrading after `sigil update` / `cargo install sigil`

Just re-run `sigil <tool> install`. The block is scoped by sentinels — `<!-- sigil:begin -->` … `<!-- sigil:end -->` in Markdown files; a `# sigil-hint` trailer inside the hook command in JSON files — and `upsert_marker_block` / `upsert_*_hook` swap the old block out for the new one in place:

- **Content outside the sentinels is preserved** — your `# My project` header, `## Build` notes, sibling `Bash`-matcher hooks, unrelated settings, etc. all stay untouched across the upgrade.
- **Content inside the sentinels is owned by sigil** — if you hand-edited the block, those edits will be replaced on re-install. Put custom phrasing *outside* the markers if you want it preserved.
- **Output tells you what happened**: `Created` = new file, `Updated` = block swapped, `Unchanged` = already current.
- **If you deleted the sentinels by hand**, sigil will *append* a fresh block at the end rather than find-and-replace — safe, but you'll end up with duplicate content unless you clean the stray text.

Concrete end-to-end demo: a CLAUDE.md carrying an old one-line sigil block, plus user content before and after it, plus a custom `Bash` matcher hook in `.claude/settings.json`. After `sigil claude install`: the block is replaced with the current flowchart, the old `sigil-hint` hook is replaced with the current HINT_LINE, and every other user setting / hook / paragraph survives byte-for-byte. `uninstall` does the mirror — strips only sigil's sentinel-scoped territory.

Net upgrade path for users on 0.3.x moving to 0.4.x: one `sigil <tool> install`, their CLAUDE.md jumps from the old capability list to the new structural-questions flowchart + file-system routing table + `--names-only` hint + worked example, and their PreToolUse hook picks up the new didactic HINT_LINE.

---

## Benchmarks

### Multi-language test (DuckDB backend auto-engaged at the 5 MB threshold)

One OSS repo per language, 3 query shapes each, 3-run median wall-clock. Sigil vs `git grep`. Full writeup in [evals/results/multilang-with-db-2026-04-20.md](evals/results/multilang-with-db-2026-04-20.md).

| Repo | Lang | Entities | Init | Best sigil win |
|---|---|---:|---:|---|
| cobra | Go | 1.6k | 235 ms | 15.9× more compact, 1.5× faster |
| ripgrep | Rust | 5.6k | 1.66 s | 9.2× more compact, ≈ tied on time |
| zod | TypeScript | 30.8k | 9.9 s | **69.5× more compact**, 1.8× faster |
| fastapi | Python | 118.6k | 2.83 s | 16.7× more compact, **6× faster** |

- **Compactness**: sigil consistently 5–70× smaller than grep output because the parsed reference table skips docstrings, comments, string literals, and type annotations that grep matches.
- **Speed**: small repos, sigil is ~1.5× faster; large repos (with DuckDB auto-engage), **sigil beats grep by 2–6×**. The sweet spot is anything with ≥5 MB of `.sigil/*.jsonl`.
- **Semantic gap**: `git grep` returns **0 lines** on "what's in this file?" queries across all 4 languages — regex can't match Rust multi-line impls, Python indented methods, TS `export const foo = ...`, or Go receiver methods. sigil's AST-based extraction handles each trivially.

### Self-benchmark (sigil on sigil)

| Query | grep tokens | sigil tokens | Ratio |
|---|---:|---:|---:|
| PR review (3 commits) | 195,003 | 5,572 | **35×** |
| Context for `Entity` | 91,937 | 467 | **196×** |
| Cold-start orientation | 44,733 | 2,786 | **16×** |

Median: **35×**. BPE-accurate counts via `o200k_base`. Raw JSON at [evals/results/0.2.4-HEAD-3..HEAD-o200k.json](evals/results/0.2.4-HEAD-3..HEAD-o200k.json).

---

## How it works

```
                  ┌──────────────────────────────────────────┐
                  │  source files (.rs .py .ts .go …)        │
                  └──────────────────┬───────────────────────┘
                                     │ tree-sitter (11 languages)
                                     ▼
                  ┌──────────────────────────────────────────┐
                  │  Entity  — struct/fn/class with 3 BLAKE3 │
                  │  Reference — call / import / type_annot  │
                  └──────┬──────────────────────┬────────────┘
                         │                      │
              ┌──────────▼──┐           ┌──────▼────────────┐
              │ entities.jsonl│         │ refs.jsonl        │
              └──────┬────────┘         └──────┬────────────┘
                     │                         │
              ┌──────▼───────────┐             │
              │ PageRank + blast │◄────────────┘
              │ rank.json        │
              └──────┬───────────┘
                     │
   ┌─────────────────┼─────────────────────────────┐
   ▼                 ▼                             ▼
 in-memory        DuckDB-backed               sigil diff
 HashMap Index    (auto-engages ≥5 MB)        (structural match + classify)
 (small repos)    (monorepo scale)
```

1. **tree-sitter parser** extracts entities (functions, structs, classes, types, imports) with line ranges. 11 languages ship by default; language grammars can be opted out of at build time for a slimmer binary.
2. **BLAKE3 hashes** per entity — `struct_hash` (raw), `body_hash` (normalized, ignores whitespace), `sig_hash` (signature only). Powers classify: formatting-only vs logic-change vs API-change.
3. **Reference table** — call / import / type_annotation / instantiation / definition rows, linking caller → target.
4. **PageRank** over the file import graph ranks which files are load-bearing. **Blast radius** per entity = BFS over the reverse-reference graph, capped at depth 3.
5. **Two backends** behind a single router:
   - **In-memory** `HashMap<String, Vec<usize>>` lookups. Sub-20 ms queries, zero dependencies.
   - **DuckDB** persistent store, columnar + vectorized. Auto-engages above 5 MB of JSONL. Handles monorepo scale without re-parsing on every invocation.

The two backends serve identical APIs; the router picks based on index size or `SIGIL_BACKEND` env var. Users never think about it.

**On-disk** (per repo):
```
.sigil/
  entities.jsonl       ← one entity per line; source of truth, committable
  refs.jsonl           ← one reference per line
  rank.json            ← PageRank + blast radius
  cache.json           ← per-file BLAKE3 hashes for incremental re-indexing
  SIGIL_MAP.md         ← optional — `sigil map --write` artifact for agents
  index.duckdb         ← derived, gitignored, built lazily on first SQL query
```

---

## Supported languages

Tree-sitter grammars ship as cargo features. Default build includes all 11:

| Language | Extensions |
|---|---|
| Python | `.py` `.pyi` `.pyw` |
| Rust | `.rs` |
| JavaScript | `.js` `.mjs` `.cjs` `.jsx` |
| TypeScript | `.ts` `.mts` `.cts` `.tsx` |
| Go | `.go` |
| Java | `.java` |
| C / C++ | `.c` `.h` `.cpp` `.cc` `.cxx` `.hpp` `.hxx` |
| Ruby | `.rb` `.rake` `.gemspec` |
| C# | `.cs` |
| Markdown | `.md` `.markdown` |

Plus four sigil-native parsers for data formats: **JSON**, **YAML**, **TOML**, and **Markdown**. Structural diff works on all four — `"port": 8080 → 8443` is detected as an entity change (not "line 14 changed"), YAML / TOML key moves are matched parent-aware, Markdown headings / code blocks / tables / lists are entity-extracted for diffing too.

---

## Command reference

### Structural diff

The original workhorse. Works without `.sigil/` when you pass `--files`; with an index it also lights up caller-aware impact on breaking changes.

| Flag | Effect |
|---|---|
| `<refspec>` | `HEAD~1`, `main..HEAD`, `abc123..def456` |
| `--files <OLD> <NEW>` | Compare two files directly, no git required |
| `--markdown` | GitHub-flavored markdown (PR-ready); `--no-emoji` for ASCII glyphs |
| `--json [--pretty]` | Structured output for scripts / CI |
| `--summary` | One-line `N breaking / M logic / K formatting` |
| `--group` | Cluster related entity changes |
| `--lines` | Show line numbers next to entity names |
| `--context N` / `--no-context` | Lines of code context around each change (default 3) |
| `--no-callers` | Skip caller analysis for breaking changes (faster on huge diffs) |
| `--no-color` | Disable ANSI color (for logs / files) |
| `-r, --root <ROOT>` | Project root (default `.`) |

Exit code is always 0 on success; non-zero only on fatal errors. Rename / move detection is automatic — matched via `body_hash` equality across delete + add pairs.

### Agent-facing (narrated, budget-aware)

| Command | What it does |
|---|---|
| `sigil where <symbol> [--include-tests] [--format markdown\|json] [--pretty]` | Single-shot definition locator. Returns one row per defining `(file, parent, kind)` with signature preview + overload count. Test files excluded by default. |
| `sigil outline [--path DIR] [--format markdown\|json]` | Hierarchical top-level tree of classes + functions grouped by file. Complements `sigil map` (rank-ordered) with a plain structural view. |
| `sigil map [--tokens N] [--focus PATH] [--exclude-tests] [--write]` | Ranked codebase digest. Pack N tokens of highest-impact orientation into one markdown artifact. `--write` tees to `.sigil/SIGIL_MAP.md`. |
| `sigil context <symbol> [--budget N] [--format agent\|markdown\|json]` | Focused bundle for one symbol: signature + callers + callees + related types + inheritance overrides. |
| `sigil review <refspec> [--markdown\|--json]` | PR review: structural diff + blast radius + co-change misses. |
| `sigil blast <symbol> [--depth N]` | Impact summary: direct callers, files, transitive reach. |
| `sigil benchmark [--refspec R] [--symbol S] [--tokenizer o200k_base\|cl100k_base\|p50k_base\|proxy] [--format markdown\|json]` | Publishes a median token-reduction number for your repo. |

### Script-facing (raw, unbounded, JSON-friendly)

Script-facing commands default to unbounded results and minified `--json` output. Pass `--pretty` for indented JSON.

| Command | What it does |
|---|---|
| `sigil search <q> [--scope symbol\|file\|all] [--kind K]` | Substring search over symbol names. Defaults to symbol-scope (pass `--scope all` to widen). Rows include file, line, kind, parent, and signature preview. Overload dedupe collapses repeated `(file, name, kind)` hits into one row with `overloads: N`. |
| `sigil symbols <file> [--depth N] [--with-hashes]` | All entities in a file. `--depth 1` keeps only top-level items (classes, top-level fns) — ~95% smaller payload for outline-style orientation. |
| `sigil children <file> <parent>` | Entities under a class / module. |
| `sigil callers <symbol> [--kind K] [--group-by file\|caller\|name\|kind]` | All references targeting a symbol. `--group-by` collapses to a `{key: count}` map when you only need aggregate distribution. |
| `sigil callees <caller> [--kind K] [--group-by file\|name\|kind]` | What a symbol calls. Same `--group-by` support. |
| `sigil explore [--path PATH]` | Directory overview with file counts by language. |
| `sigil duplicates [--min-lines N]` | Clone report across the codebase (groups by `body_hash`). |
| `sigil cochange [--commits N]` | Mine git history for file-pair co-change weights (`.sigil/cochange.json`). |

### Admin & data pipeline

| Command | What it does |
|---|---|
| `sigil index [--full] [--no-rank]` | Build / refresh the `.sigil/` index. Incremental by default. `--full` forces re-parse; `--no-rank` skips PageRank + blast radius. Runs automatically on the first query in an un-indexed repo (disable with `SIGIL_NO_AUTO_INDEX=1`). |
| `sigil query "SQL"` | Ad-hoc SQL against the materialized DuckDB index. Tables: `entities`, `refs`, plus `rank` / `blast` views. |
| `sigil update` | Self-update via axoupdater (release-binary installs). |

### Integrations

| Command | What it writes |
|---|---|
| `sigil claude install` | `CLAUDE.md` block + `.claude/settings.json` PreToolUse hook |
| `sigil cursor install` | `.cursor/rules/sigil.mdc` (alwaysApply: true) |
| `sigil codex install` | `AGENTS.md` + `.codex/hooks.json` |
| `sigil gemini install` | `GEMINI.md` + `.gemini/settings.json` BeforeTool |
| `sigil opencode install` | `AGENTS.md` + `.opencode/plugins/sigil.js` |
| `sigil aider install` | `AGENTS.md` block |
| `sigil copilot install` | `~/.copilot/skills/sigil/SKILL.md` |
| `sigil hook install` | `.git/hooks/post-commit` + `post-checkout` auto-rebuild |

Every integration has `sigil <name> uninstall`. All are idempotent and content-preserving.

---

## Backend selection

The router picks per query:

1. `SIGIL_BACKEND=memory` → force in-memory.
2. `SIGIL_BACKEND=db` → force DuckDB.
3. Otherwise, auto-engage DuckDB when total `.sigil/*.jsonl` size ≥ `SIGIL_AUTO_ENGAGE_THRESHOLD_MB` (default 5 MB).
4. Fall back to in-memory.

Unknown `SIGIL_BACKEND` values are a hard error — no silent fallbacks. Reproducibility > convenience.

---

## CI / CD example

```yaml
# .github/workflows/review.yml
- name: sigil structural diff
  run: |
    curl -LsSf https://github.com/knova-run/sigil/releases/latest/download/sigil-installer.sh | sh
    sigil index
    sigil review origin/main..HEAD --markdown > review.md

- name: Comment on PR
  uses: actions/github-script@v7
  with:
    script: |
      const fs = require('fs');
      const body = fs.readFileSync('review.md', 'utf8');
      github.rest.issues.createComment({
        issue_number: context.issue.number,
        owner: context.repo.owner,
        repo: context.repo.repo,
        body,
      });

- name: Block breaking changes without label
  run: |
    if sigil diff origin/main..HEAD --json | jq -e '.summary.has_breaking'; then
      gh pr view ${{ github.event.number }} --json labels | \
        jq -e '.labels[] | select(.name == "breaking-change")' || \
        (echo "breaking changes require the 'breaking-change' label"; exit 1)
    fi
```

---

## Honest caveats

- **sigil needs `sigil index` first.** ~200 ms on tiny repos; ~3 s on fastapi-size (2,500 files); ~10 s on TypeScript-heavy codebases (zod). One-time cost per session; `sigil hook install` amortizes via git hooks.
- **Output is precise, not exhaustive by default.** `sigil map --tokens 4000` hits its budget and truncates; `sigil context` hits a depth cap. The script-facing commands (`callers`, `symbols`, `children`) are unbounded — use those when you need every row.
- **No semantic inference.** sigil tells you who calls what and what changed structurally. It doesn't tell you "this function implements the observer pattern" or "this has a race condition." Those need an LLM — sigil feeds one, it doesn't replace one.
- **Tree-sitter parsing isn't 100%.** Some language edge cases (Rust macros, Python dynamic imports, TS complex generics) don't extract cleanly. The 4 data parsers (JSON/YAML/TOML/Markdown) are sigil-native and handle edge cases that tree-sitter grammars don't.
- **Small-repo performance:** the DuckDB backend only engages above 5 MB of JSONL index (tunable via `SIGIL_AUTO_ENGAGE_THRESHOLD_MB`). Below that, sigil stays on the in-memory index, so there's no DuckDB cost on small repos.

---

## FAQ

**Q: Do I need to commit `.sigil/` to git?**
Depends. `.sigil/entities.jsonl` + `refs.jsonl` + `rank.json` + `SIGIL_MAP.md` are committable (human-readable, diffable, small on small repos). `.sigil/index.duckdb` is derived and `.gitignore`'d by default. Committing the JSONL lets every teammate / CI agent read the map without running `sigil index` first. Not committing them keeps the tree cleaner.

**Q: How does sigil compare to ripgrep?**
Different tools. ripgrep is line-oriented text search; sigil is structural AST search. Sigil beats grep on (a) output compactness — 5–70× fewer bytes because no noise from docstrings / strings / comments, and (b) semantic queries like "what's defined in this file?" that grep can't express. Grep beats sigil on one-shot queries against unindexed repos. Detailed numbers in [evals/results/multilang-with-db-2026-04-20.md](evals/results/multilang-with-db-2026-04-20.md).

**Q: How does sigil compare to LSP / language servers?**
LSPs are per-language, resident processes with deep semantic understanding (types, generic resolution, incremental state). sigil is cross-language, stateless, deterministic. Complementary, not competitive.

**Q: Why BLAKE3 for hashing?**
Faster than SHA-256, faster than xxhash3 at most sizes. 16-hex-char truncation is sigil's storage form — enough to distinguish entities within any plausible repo size.

**Q: What happens if `sigil index` can't parse a file?**
Skipped silently (with `-v` flag: printed to stderr). sigil never errors out on parse failures — one broken file doesn't block the other 2,000.

**Q: Can I run sigil without the `.sigil/` directory?**
Yes — `sigil diff --files old.py new.py` compares two files directly without an index. `sigil diff <refspec>` (against git) also works without an index; caller-aware blast enrichment on breaking changes gets skipped until you run `sigil index`. For agent commands (`map`, `context`, `blast`, `review`) the index is required.

**Q: How is `sigil diff` different from `git diff`?**
`git diff` is line-oriented text: a renamed function becomes "delete + add" noise; whitespace-only edits look identical to logic changes; you can't tell a breaking-API change from a body refactor without reading every hunk. `sigil diff` matches entities by body hash (so renames collapse into one row), classifies every change by three BLAKE3 hashes (breaking / logic / formatting), and extracts entity-level markdown that pastes straight into PRs. Use `git diff` to see lines; use `sigil diff` to review the change.

**Q: What does `sigil diff --files` support?**
All 11 tree-sitter languages + JSON + YAML + TOML + Markdown. Pass any two paths; sigil picks the parser from the extension. Works offline, no git, no index.

---

## License

MIT. See [LICENSE](LICENSE).
