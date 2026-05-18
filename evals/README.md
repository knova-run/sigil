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
  semantic_eval.py       ← semantic-search retrieval eval (NDCG@10 / Recall@k)
```

## Semantic-search retrieval eval

`semantic_eval.py` measures **retrieval quality** rather than token cost: given
a natural-language query, how high does each retriever rank the gold entity?
Drives the `feat/semantic-search` workstream (Spike 1 → 4).

Query/gold pairs are bootstrapped from docstrings already in
`.sigil/entities.jsonl` (first sentence of the docstring → the entity it
documents — same CodeSearchNet methodology semble uses). Single-repo today
(sigil itself); multi-repo corpus to follow.

```bash
# One-time env setup (creates evals/.venv with tiktoken for BPE counts)
python3 -m venv evals/.venv && evals/.venv/bin/pip install tiktoken

# Run the baseline (sigil search + grep)
evals/.venv/bin/python evals/semantic_eval.py \
    --max-pairs 200 \
    --retrievers sigil_search,grep \
    --out evals/results/semantic-baseline-<date>.json

# Add new retrievers by registering them in semantic_eval.py:
#   @register("my_retriever")
#   def my_retriever(query: str, root: Path, k: int) -> List[Hit]: ...
```

**Metrics:**
- `ndcg@10` — Normalized Discounted Cumulative Gain at rank 10 (binary
  relevance, single gold doc → equivalent to MRR@10).
- `r@k` — Recall at k: fraction of queries where the gold entity appears in
  the top-k.
- `tok@10` — Median tokens (o200k_base BPE) of the stitched top-10 result.
- `ms` — Median wall-clock per query.

**Baseline + Spike 1 + doc-masked results (sigil-on-sigil, 200 docstring queries, 2026-05-18):**

| Retriever | NDCG@10 | R@1 | R@10 | Tokens@10 | Median ms |
|---|---:|---:|---:|---:|---:|
| `sigil_search` (multi-probe distinctive terms) | 0.129 | 0.045 | 0.250 | 264 | 421 |
| `sigil_search_verbatim` (raw prose) | 0.000 | 0.000 | 0.000 | 0 | 226 |
| `grep` (multi-term regex) | 0.020 | 0.010 | 0.030 | 300 | 1377 |
| `sigil_semantic_bm25` (Spike 1, full text) | **0.905** | **0.780** | **0.995** | 276 | 52 |
| `sigil_semantic_bm25_no_doc` (doc-masked) | **0.370** | 0.220 | 0.525 | 278 | 48 |

The full-text BM25 result (0.905) is **inflated by self-referential overlap**:
queries are first sentences of docstrings, and the gold entity's own `doc`
field is part of the indexed text. The `--no-doc` retriever (`sigil semantic
<q> --no-doc`) excludes `doc` from the indexed text so the retriever sees
only `name + qualified_name + sig` per entity — that's the **honest lower
bound** on what BM25 buys us when docstring overlap is removed.

**Honest reading:**
- Doc-masked BM25 (0.370) is **~2.9× better than `sigil_search` (0.129)** on
  the same queries, and ~8× faster.
- Bringing docstrings back into the index lifts NDCG@10 from 0.370 → 0.905.
  That lift is **real in production** when the agent queries a documented
  codebase — it isn't pure measurement bias; it's the value of indexing
  doctrings. The bias issue is only that our eval queries are *literally
  first sentences of docs*, which over-rewards doc-overlap.
- True production performance for docstring-style natural-language queries
  is between 0.370 (worst case: query terms appear nowhere in entity text)
  and 0.905 (best case: query terms heavily overlap with indexed doc).
- Semble's reported 0.854 is on a different (broader) corpus; not
  directly comparable without their query set.
- Doc-masked still has 79/200 queries missing the top-20 — that's the
  headroom Spike 2 (static embeddings) and Spike 4 (rerank signals) are
  competing for.

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
