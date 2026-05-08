# sigil evals

The minimum honest benchmark: for a fixed set of "agent-shaped" queries, how
many tokens does it take to answer via **raw tools** (git, grep, file reads)
vs **sigil commands**?

This directory holds the scripts, inputs, and historical results. The
methodology is deliberately simple at this stage — no LLM in the loop, no
hand-labeled ground truth, no SWE-bench. Just a reproducible
token-accounting pass on a real repo (sigil itself) whose numbers we can
publish without hand-waving.

All published numbers use BPE-accurate counts via OpenAI's `o200k_base`
tokenizer (closest Rust-available match to Claude's on code). Build sigil
with `--features tokenizer` to enable it. The default build still has a
`bytes / 4` proxy for zero-dep usage, but everything in this directory
and everything quoted in the README is BPE.

## Layout

```
evals/
  README.md              ← this file: methodology + how to run
  run.sh                 ← capture a new result snapshot
  results/               ← one JSON per (sigil_version, refspec)
```

## What's measured today

`sigil benchmark --format json` runs three canonical queries:

| Query | Control (raw) | Treatment (sigil) |
|---|---|---|
| **PR review** | `git diff --stat --patch <refspec>` | `sigil review <refspec>` |
| **Context for `<sym>`** | read every file that references `<sym>` (bounded 100) | `sigil context <sym>` |
| **Cold-start orientation** | cat 20 random source files | `sigil map --tokens 2000` |

For each, we capture the output of both approaches, count tokens with
`o200k_base`, and publish the ratio. The median across queries is the
headline number.

## What's *not* measured yet

- **Agent success rate** — no model in the loop. A win on tokens means
  little if the agent can't reach an answer. Covered by E1/E2/E4 in §8
  of the plan.
- **Cross-repo variance** — we only run on sigil itself. Different
  codebases will produce different numbers; especially, token reduction
  grows with corpus size (graphify's 71.5× on Karpathy-repos vs ~5×
  on a single lib). Covered by E1 batching in §8.
- **Hand-labeled navigation ground truth** — comparing sigil's caller
  set vs grep's would need a human oracle per symbol. Deferred.

## Why `o200k_base`

OpenAI's `o200k_base` (the GPT-4o/o3 tokenizer) is the closest Rust-
available match to Claude's on code — the token-per-byte rate differs
by under 5% in our spot checks. Using it keeps the published numbers
honest (safe to cite dollar-cost math) without requiring a network
round-trip for every run.

Available via `tiktoken-rs` behind the `tokenizer` cargo feature;
build once with `cargo install sigil --features tokenizer` and every
`sigil benchmark` call can use `--tokenizer o200k_base`. Sigil also
supports `cl100k_base` (GPT-3.5/4) and `p50k_base` (legacy) for
cross-model comparison.

The zero-dep `proxy` (bytes / 4) path remains available for internal
comparisons but we never publish proxy numbers — they over-estimate
ratios by 15–30% depending on query shape.

## How to capture a snapshot

```bash
# One-time: build with the tokenizer feature
cargo install --path . --features tokenizer

# Make sure the index is fresh and rank is populated
sigil index

# Capture with the current HEAD range
./evals/run.sh

# Or with an explicit refspec
./evals/run.sh HEAD~5..HEAD
```

The script writes `evals/results/<sigil-version>-<refspec>-o200k.json`
and prints the median ratio to stderr. Commit the JSON alongside the
code change that produced it — each result is a dated artifact.

## Cross-repo benchmark

The sigil-self number is one data point. Real adoption decisions need
multiple points across corpus sizes, languages, and coupling densities.
`evals/cross_repo.sh` runs the benchmark across a curated OSS set.

```bash
# Defaults: uses evals/corpus.tsv, persists results to
# evals/results/cross-repo-<date>/.
./evals/cross_repo.sh

# Custom corpus:
./evals/cross_repo.sh path/to/my-corpus.tsv

# Persist clones across runs (skips re-cloning on repeat):
CORPUS_DIR=/tmp/sigil-corpus ./evals/cross_repo.sh
```

The script clones each repo (shallow, depth 200), runs `sigil index`,
and records a benchmark JSON per repo. At the end it emits a
`README.md` summary table with per-repo medians and per-query detail.

**Expected runtime**: ~1 minute per repo on cold clone. Under 5 minutes
for the default 3-repo corpus on a decent connection.

### Adding repos

Append a tab-separated row to `evals/corpus.tsv`:

```
<slug>	<git-url>	<ref>	<refspec>
```

Keep refs pinned (tag or SHA, not `main`) so results are reproducible
on re-run. Test corpora belong in their own TSV, not the main one.

## Historical results

All rows use `o200k_base` (GPT-4o/o3). Add a new row per (sigil version,
refspec) snapshot; commit the JSON alongside.

| Date | sigil version | refspec | Median ratio | PR review | Context | Orientation |
|---|---|---|---:|---:|---:|---:|
| 2026-04-20 | 0.2.4 | HEAD~3..HEAD | **35.00×** | 35.00× | 196.87× | 16.06× |

Raw JSON: [`results/0.2.4-HEAD-3..HEAD-o200k.json`](results/0.2.4-HEAD-3..HEAD-o200k.json)

## Backend selection (`SIGIL_BACKEND`)

Queries that the router covers (`sigil callers` / `callees` / `symbols`
/ `children`) pick a backend automatically:

- JSONL total < 50 MB → in-memory `Index` (fast, zero deps).
- JSONL ≥ 50 MB → DuckDB (columnar, handles Chromium-scale).

Override via the `SIGIL_BACKEND` environment variable:

```bash
SIGIL_BACKEND=memory sigil callers Entity   # force in-memory
SIGIL_BACKEND=db     sigil callers Entity   # force DuckDB (needs --features db)
```

Any other value is a hard error — no silent fallbacks, to keep
reproducibility explicit. The analytical commands (`map`, `context`,
`review`, `blast`, `benchmark`) stay on the in-memory path
unconditionally until the corresponding DuckDB methods land.

## Reproducibility

```bash
git clone https://github.com/knova-run/sigil
cd sigil
cargo install --path . --features tokenizer
sigil index
./evals/run.sh HEAD~3..HEAD
# Compare against evals/results/0.2.4-HEAD-3..HEAD-o200k.json
```

Numbers should match within ±5% of the published result on the same git
ref. Larger deviations typically mean the repo has evolved; re-run
against a specific commit SHA to reproduce historical numbers.
