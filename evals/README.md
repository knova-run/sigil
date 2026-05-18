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

**Baseline + Spike 1 + Spike 2 results (sigil-on-sigil, 200 docstring queries, 2026-05-18):**

| Retriever | NDCG@10 | R@1 | R@10 | Tokens@10 | Median ms |
|---|---:|---:|---:|---:|---:|
| `sigil_search` (multi-probe distinctive terms) | 0.129 | 0.045 | 0.250 | 264 | 438 |
| `grep` (multi-term regex) | 0.020 | 0.010 | 0.030 | 300 | 1373 |
| `sigil_semantic_bm25` (Spike 1, full text) | **0.905** | **0.780** | **0.995** | 276 | **53** |
| `sigil_semantic_bm25_no_doc` (doc-masked) | 0.372 | 0.225 | 0.525 | 279 | 49 |
| `sigil_semantic_m2v` (Spike 2, full text) | 0.856 | 0.745 | 0.960 | 274 | 197 |
| `sigil_semantic_m2v_no_doc` (doc-masked) | **0.404** | 0.240 | 0.585 | 265 | 156 |

The full-text BM25 result (0.905) is **inflated by self-referential overlap**:
queries are first sentences of docstrings, and the gold entity's own `doc`
field is part of the indexed text. The `--no-doc` retriever (`sigil semantic
<q> --no-doc`) excludes `doc` from the indexed text so the retriever sees
only `name + qualified_name + sig` per entity — that's the **honest lower
bound** on what BM25 buys us when docstring overlap is removed.

### Cross-repo eval — 4 repos × 4 languages × 50 queries each (2026-05-18)

To check whether the sigil-on-sigil wins above generalise, ran the same
harness against a 4-repo corpus pinned at stable releases:
**ripgrep 14.1.0** (Rust), **httpx 0.27.0** (Python),
**mdbook v0.4.40** (Rust), **cobra v1.8.0** (Go). 50 docstring-bootstrap
queries per repo = 200 total queries, **including 4 retrievers from sigil
and 3 retrievers from semble (the upstream library, `pip install semble`)
on the same indexes and same queries**:

```bash
evals/.venv/bin/python evals/cross_repo_semantic_eval.py \
    --max-pairs-per-repo 50 \
    --retrievers sigil_semantic_bm25,sigil_semantic_bm25_no_doc,\
sigil_semantic_m2v,sigil_semantic_m2v_no_doc,\
semble_hybrid,semble_bm25,semble_semantic \
    --out evals/results/semantic-cross-repo-<date>.json
```

**Cross-repo global aggregate (200 queries, all 4 repos pooled):**

| Retriever | NDCG@10 | R@1 | R@10 | Tok@10 | Median ms |
|---|---:|---:|---:|---:|---:|
| `sigil_semantic_bm25` (Spike 1, full) | 0.951 | 0.875 | **1.000** | 240 | 31 |
| `sigil_semantic_bm25_rerank` (Spike 1+4) | **0.959** | **0.895** | **1.000** | 243 | 31 |
| `sigil_semantic_m2v` (Spike 2, full, persisted) | 0.915 | 0.820 | 0.990 | 243 | 61 |
| `sigil_semantic_fuse` (Spike 3, RRF of Spike 1+2) | 0.942 | 0.860 | **1.000** | 243 | 66 |
| `sigil_semantic_fuse_rerank` (Spike 3+4) | 0.946 | 0.875 | 0.995 | 244 | 66 |
| `semble_bm25` | 0.850 | 0.740 | 0.950 | 571 | **0.1** |
| `semble_hybrid` (semble's default) | 0.643 | 0.535 | 0.750 | 577 | 2.3 |
| `semble_semantic` | 0.642 | 0.490 | 0.810 | 575 | 0.2 |
| `sigil_semantic_m2v_no_doc` | 0.613 | 0.440 | 0.795 | 242 | 89 |
| `sigil_semantic_bm25_no_doc` | 0.472 | 0.325 | 0.645 | 257 | 29 |

**Cross-repo findings:**

1. **The wins hold across all 4 repos and 4 languages.** Per-repo
   `sigil_semantic_bm25` NDCG@10 ranges 0.943–0.983; `semble_bm25` 0.802–0.894.
   Pattern is consistent — sigil BM25 wins on every single repo by a
   comparable margin. Not a sigil-on-sigil artifact.

2. **Sigil BM25 beats semble's best mode by ~12% relative**, sigil m2v
   beats semble HYBRID by ~42% relative.

3. **Semble HYBRID is worse than semble BM25 alone on every repo.**
   Their RRF-fused default consistently underperforms pure BM25 here —
   this is a known risk that Spike 3 needs to design around, not just
   adopt by default.

4. **Doc-masked m2v decisively beats doc-masked BM25** (0.613 vs 0.472,
   +30% relative). Embeddings buy a real lift on the "no lexical
   overlap" hard cases — most visible on cobra (Go) where idiomatic
   identifiers and godoc make name+sig text already informative
   (0.805 vs 0.720).

5. **Token efficiency persists**: sigil ~240 vs semble ~575 tokens at k=10
   — 2.4× more efficient at the same recall, across all repos.

6. **Latency caveat**: semble wins on per-query latency (0.1–2.3 ms vs
   sigil's 30–60 ms). The remaining gap is dominated by CLI startup +
   model load on a fresh process; semble runs in a long-lived process
   (MCP server / Python REPL) and amortises both. For agentic CLI use,
   60 ms is unnoticeable; if sigil exposes a long-lived MCP retriever
   later, the gap closes further. After persistence (this commit),
   first-query latency is ~2 s on sigil-on-sigil (full corpus encode);
   subsequent queries load `.sigil/embeddings.bin` and only encode the
   query (~µs) before scoring.

### Per-repo NDCG@10 breakdown

| Retriever | ripgrep | httpx | mdbook | cobra |
|---|---:|---:|---:|---:|
| `sigil_semantic_bm25` | 0.946 | 0.983 | 0.943 | 0.948 |
| `sigil_semantic_m2v` | 0.885 | 0.904 | 0.940 | 0.929 |
| `semble_bm25` | 0.802 | 0.874 | 0.830 | 0.894 |
| `semble_hybrid` | 0.581 | 0.542 | 0.667 | 0.784 |
| `semble_semantic` | 0.652 | 0.478 | 0.686 | 0.753 |

### Sigil-on-sigil single-repo eval (legacy, kept for trace)

The original single-repo eval below is preserved because the bias
discussion (self-referential docstring queries on the same repo) drove
the addition of the doc-masked retriever and the cross-repo extension
above.

**Honest reading (sigil-on-sigil):**
- **BM25 outscores Model2Vec on full-text mode (0.905 vs 0.856).** Lexical
  overlap on docstring-shaped queries beats semantic embedding when the
  agent's query happens to share words with the indexed doc — and on this
  bootstrap corpus, it usually does.
- **Model2Vec wins on doc-masked mode (0.404 vs 0.372).** When the
  lexical overlap signal is removed, embeddings buy back ~8.6% relative
  NDCG@10. m2v also has fewer top-20 misses (69 vs 79).
- **Where m2v helps is the long tail.** Look at rank distribution: m2v_no_doc
  has 69 misses, bm25_no_doc has 79. Embeddings catch ~10 queries where
  the name/sig lacks any token overlap with the prose query — the
  "agent asks about a concept the codebase calls something different"
  case lexical retrieval can't bridge.
- **m2v is currently 3-4× slower** (197 ms vs 53 ms) because every query
  re-encodes the whole corpus. With persisted embeddings, query time
  drops to single-digit ms (encode query once + dot product against
  precomputed matrix). Persistence is a follow-up if we keep m2v.
- **Doc-overlap is real production signal**, not just measurement bias —
  agents querying a documented codebase do benefit from doc match. Our
  eval queries are literally first sentences of docs, which over-rewards
  it; production is between the doc-masked (worst) and full-text (best)
  numbers.
- **The takeaway for ship/revert (task #6):** BM25 alone may already be the
  right answer for sigil. Embeddings give a measurable but modest lift
  on the hard cases. The real win could come from Spike 3 (RRF fusion of
  BM25 + m2v) — combining the lexical and semantic signals so each
  catches what the other misses.

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
