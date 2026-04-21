---
name: sigil
description: "Use sigil for structural code intelligence — find where a symbol is defined, who calls it, what it calls, list the names in a file, diff a PR structurally, see what breaks if you rename it. ALWAYS use this skill when: the user asks 'where is X defined' or 'who calls X' or 'what does X call' or 'what's in file F' or 'how does X fit in the codebase' or 'what would break if I change X' or 'show me the diff' or 'review this PR' or 'find duplicates' or 'what does this codebase look like', when exploring an unfamiliar repo, when you're about to chain `grep` + `read_file` to answer a structural question, or when a task matches the SWE-bench-Lite phase-1 shape of 'find the method that does X'. Prefer sigil over grep/read_file for any question about relationships (callers, callees, inheritance, rank, blast) or cross-file structural lookups. Do NOT use for pure file enumeration (use `ls`), language-specific syntactic patterns (`grep` for Rust `^pub mod`), or raw text inside a known file."
---

# sigil — Structural Code Intelligence for Agents

sigil gives you entity-level understanding of code: what exists, how it relates, what changed. It replaces multi-step grep+read_file chains with a **single call** that returns a structured answer — file, line, class, signature, overrides, callers — ready to use.

## One-shot command cheat-sheet

Questions that sigil answers in **one call**, ordered by frequency. Each row is a complete flow: the question, the command, why it's one-shot.

| Question (what the user asks) | One-shot command | Why one-shot |
|---|---|---|
| "where is `X` defined?" | `sigil where X` | Returns file + line + class + signature + override siblings in one row, rank-sorted (framework-level definitions first). Tail-segment match: `get_default` finds both `Parameter.get_default` and `Option.get_default`. Default cap: 10 rows. |
| "where is `X` defined **on class `C`**?" | `sigil where X --parent C` | Same shape, filtered to one class. Use when the bare-name query returned >10 hits or hits the wrong class. |
| "where is `X` defined **in file F** or subtree?" | `sigil where X --file F` | Filters hits whose path contains `F` (substring, not glob). Stack with `--parent`. |
| "who calls `X`?" | `sigil callers X --json` | Structured caller list with file + caller-fn + line, filtered by kind. Add `--group-by file` when you want `{file: count}` distribution only. |
| "what does `X` call?" | `sigil callees X --json` | Same shape in reverse. `--group-by name` for a target-count summary. |
| "list the classes/fns in `F`" | `sigil symbols F --depth 1 --names-only` | Flat JSON array of top-level names, ~300 bytes. Drops imports, nested methods, variables. |
| "full entities in `F` (with sigs + line ranges)" | `sigil symbols F --depth 1 --json` | Top-level entities with sig + kind + line + parent. |
| "how does `X` fit into the codebase?" | `sigil context X --format agent` | Bundle: signature + callers + callees + related types + inheritance overrides. Budget-capped. |
| "locate `X` **and** show me its body in one call" | `sigil context X --with-body --format agent` | Same bundle plus the raw source lines from `line_start..=line_end`. Saves the follow-up `read_file` in "locate then inspect" flows. |
| "what's in this directory structurally?" | `sigil outline --path DIR` | Hierarchical tree of classes + top-level fns grouped by file. |
| "what would break if I rename `X`?" | `sigil blast X --format agent` | Direct callers + files + transitive reach (depth 3). |
| "structural diff of this change" | `sigil diff A..B --markdown` | Entity-level change list classified as breaking / logic / formatting. |
| "review this PR" | `sigil review A..B --markdown` | `diff` + blast radius + co-change misses, rank-ordered. |
| "diff two files without git" | `sigil diff --files OLD NEW` | Any two paths, no index required for the compare itself. |
| "find duplicated function bodies" | `sigil duplicates` | Groups by BLAKE3 body hash; nothing else matches this. |
| "cold-start orientation" | `sigil map --tokens 2000` | Ranked digest in your token budget. Run this **first** in a new repo. |
| "any symbol matching 'foo'" | `sigil search foo --json` | Substring over names with sig preview per row; overloads collapsed. |

## Validated one-shot examples (measured)

These aren't hypothetical — they're the command/payload shapes benchmarked against control arms using only grep + read_file. Numbers are Sonnet medians on real codebases.

**Example 1 — "find the method on class `Parameter` that resolves default values when a callable is passed"** (pallets/click)

```bash
sigil where get_default
```

Response (384 bytes):
```
get_default
  Parameter.get_default  src/click/core.py:2249-2251  (method, 3 overloads)
    def get_default(self, ctx: Context, call: bool = True) -> Any
  Option.get_default     src/click/core.py:2891-2905  (method)
    def get_default(self, ctx: Context, call: bool = True) -> Any
```

Measured: one tool call, 2 turns total. **Control arm (grep-only): 6 turns, 12,269 tokens.** sigil: 2 turns, 5,521 tokens. **2.22× cheaper**, deterministic across seeds.

**Example 1b — many hits → narrow with `--parent` / `--file`** (common ambiguous-name pattern)

Many names in large codebases collide (`to_python` in Django has ~40 definitions across forms/models). `sigil where` caps at 10 rank-ordered rows and prints a one-line "narrow" hint on stderr:

```
sigil where to_python
# stderr: sigil: 38 definitions matched, showing top 10 by rank.
#         Narrow with `--parent CLASS`, `--file PATH_SUBSTR`, or rerun with --limit 0.

sigil where to_python --parent ModelChoiceField
# → exactly 1 row: django/forms/models.py:1321, ChoiceField.to_python signature
```

When the bug report names a class (e.g. "ModelChoiceField invalid_choice"), go straight to `--parent ModelChoiceField`. When it names a file path, use `--file`. For compound filters that flags don't express (e.g. "all `to_python` methods with blast > 5"), drop to `sigil query "SELECT file, parent, line_start FROM entities WHERE name = 'to_python' AND parent LIKE '%ChoiceField%'"`.

**Example 2 — "who calls `parse_file` in this codebase?"**

```bash
sigil callers parse_file --kind call --json
```

Returns 128 call-site references across 12 files — including qualified forms like `crate::parser::treesitter::parse_file`. Measured: **control burned 80k tokens across 16 grep-narrow turns; sigil 10k tokens, 2 turns. 14.8× cheaper for Haiku, 3.23× for Sonnet.**

**Example 3 — "list every top-level struct in `src/entity.rs`"**

```bash
sigil symbols src/entity.rs --depth 1 --names-only
# → ["Entity","BlastRadius","Reference"]
```

50 bytes instead of 900 bytes of full entity records. If you need signatures or line ranges, drop `--names-only`.

**Example 4 — "what would break if I rename `process_event`?"**

```bash
sigil blast process_event --format agent
```

Direct callers + files + transitive reach (depth 3) + top callers by file rank — one call. Replaces "grep for name; read every caller; recursively chase each caller's callers."

## When NOT to use sigil

sigil is structural. For these question shapes, simpler tools win:

| Question shape | Use instead | Why |
|---|---|---|
| "which files exist under dir D?" | `ls` / `find` / `bash` | Pure file enumeration; `sigil outline` returns classes+fns, not raw file lists. |
| "text content X inside known file F" | `read_file` / `grep` | sigil indexes symbols, not string contents. |
| "lines matching regex in the repo" | `grep` | Text search beats AST search on raw text. |
| language-specific syntactic pattern | `grep` | e.g. Rust `^pub mod` — simpler to regex. |
| sigil returned empty AND no "Did you mean?" on stderr | `grep` | Confirm the name really doesn't exist textually. |

**Empty sigil results are data, not failure.** On an empty response sigil prints `Did you mean: X, Y, Z?` to stderr when the queried name is close to known entities. Retry with a suggestion *before* falling back to grep.

## Consuming sigil output in code

Every script-facing command (`symbols`, `children`, `callers`, `callees`, `search`) defaults to **minified JSON** with `--json`. Add `--pretty` only for human inspection.

Agent-facing commands (`map`, `context`, `review`, `blast`) accept `--format agent` for a compact token-tuned JSON. Use `--format markdown` when the *user* needs to read it.

Entity JSON fields (0.4.0+):
- `file`, `name`, `kind`, `line_start`, `line_end`
- `parent` (skip when null), `sig` (when present), `meta` (when non-empty)
- `visibility` (skip when "private" — the default)
- `blast_radius: {direct_callers, direct_files, transitive_callers}` (skip when all zero)
- Hash columns (`struct_hash`, `body_hash`, `sig_hash`) appear only with `--with-hashes`

Reference JSON: `{file, caller?, name, kind, line}`. Field is `kind` (not `ref_kind`) from 0.4.0; older JSONL with `ref_kind` is read via a serde alias.

## Zero-config onboarding

First query in a repo without `.sigil/` auto-runs `sigil index` and emits one stderr line — `sigil: no index at .../.sigil — running sigil index once`. That's not an error; it's a heads-up. Set `SIGIL_NO_AUTO_INDEX=1` to disable for bulk scripts.

Once the index exists, `sigil index` reruns are incremental — only touched files re-parse.

## Full agent loop — a worked flow

"Help me understand this codebase and then refactor `handle_payment`":

```bash
# 1. Orient
sigil map --tokens 3000 --format json

# 2. Find the symbol
sigil where handle_payment
# → {file: src/checkout.rs, line: 142, sig: "fn handle_payment(...)"}

# 3. Understand its role
sigil context handle_payment --format agent
# → signature + callers + callees + related types + inheritance overrides

# 4. Quantify impact before editing
sigil blast handle_payment --format agent
# → direct_callers: 14, direct_files: 6, transitive_callers: 23

# 5. Edit the code.

# 6. Verify no unintended fan-out
sigil diff HEAD --json
# → look for unexpected MODIFIED/BREAKING entries
```

Six commands, six structured answers. Every step avoids a grep+read_file chain.

## Where to look for more

The commands above cover ~90% of agent use. For less-common scenarios, load the relevant reference file only when the task hits its scope:

- **Detailed `sigil diff` flags, classifications, exit codes, `--files` offline mode** → `references/diff.md`
- **Navigation primitives** (`search` scopes, `symbols --with-hashes`, `children`, call-graph `--kind` / `--group-by` filters, `explore`) → `references/navigation.md`
- **Advanced** (`sigil duplicates`, ad-hoc SQL via `sigil query`, `sigil benchmark`, `sigil cochange`, `sigil index` flags, install/hook commands) → `references/advanced.md`

Each reference file is self-contained — read only when needed. Don't preload them.

## Tips

- **All commands accept `-r <path>` / `--root <path>`** — run against a directory that isn't `$PWD`.
- **`sigil callers` matches qualified-tail names** — `sigil callers parse_file` finds refs stored as `crate::parser::treesitter::parse_file`, not just bare `parse_file`.
- **`sigil context` / `sigil blast` accept qualified names** like `file.rs::name`, `Parent::name`, `file.rs::Parent::name` when the bare name is ambiguous.
- **Shipped binaries are single-build** — `cargo install sigil` includes all 11 languages + DuckDB + tokenizer. No `--features` flags needed.
- **`.sigil/entities.jsonl` + `refs.jsonl` + `rank.json` are committable** (human-readable, diffable). `.sigil/index.duckdb` is derived and gitignored.
