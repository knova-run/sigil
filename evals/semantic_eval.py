#!/usr/bin/env python3
"""
sigil semantic-search eval harness.

Bootstraps (query, gold_entity) pairs from docstrings already extracted into
.sigil/entities.jsonl, runs a set of pluggable retrievers against them, and
reports NDCG@10 / Recall@k / median token-cost@k=10 / median latency.

The harness is retriever-agnostic. Today it ships two baselines (`sigil_search`,
`grep`). Future spikes plug new retrievers into the RETRIEVERS registry — no
other code in this file needs to change.

Usage:
    python3 evals/semantic_eval.py
    python3 evals/semantic_eval.py --retrievers sigil_search,grep --max-pairs 200
    python3 evals/semantic_eval.py --out evals/results/semantic-baseline.json

Requires: a current .sigil/ index at <root> (run `sigil index` first).
Optional: `pip install tiktoken` for o200k_base BPE token counts; falls
back to bytes/4 when unavailable.
"""

import argparse
import json
import math
import re
import subprocess
import sys
import time
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Callable, Dict, List, Optional


SOLID_KINDS = {"function", "method", "class", "struct", "impl", "enum", "interface"}


@dataclass
class GoldPair:
    query: str
    file: str
    name: str
    kind: str
    line_start: int
    line_end: int


@dataclass
class Hit:
    file: str
    name: Optional[str] = None
    line_start: Optional[int] = None
    line_end: Optional[int] = None
    snippet: str = ""


@dataclass
class QueryResult:
    query: str
    gold: GoldPair
    hits: List[Hit]
    elapsed_ms: float


@dataclass
class RetrieverReport:
    retriever: str
    n_queries: int
    ndcg_at_10: float
    recall_at_1: float
    recall_at_5: float
    recall_at_10: float
    recall_at_20: float
    median_tokens_at_10: float
    median_elapsed_ms: float
    rank_distribution: Dict[str, int]


# ---------------------------------------------------------------------------
# Gold-pair extraction
# ---------------------------------------------------------------------------

_SENT_BREAK = re.compile(r"(?<=[.!?])\s|\n\n")
_WHITESPACE = re.compile(r"\s+")


def extract_gold_pairs(
    entities_path: Path,
    max_pairs: Optional[int] = None,
    min_doc: int = 30,
    max_doc: int = 600,
) -> List[GoldPair]:
    pairs: List[GoldPair] = []
    with entities_path.open() as f:
        for line in f:
            e = json.loads(line)
            if e.get("kind") not in SOLID_KINDS:
                continue
            doc = (e.get("doc") or "").strip()
            if not (min_doc <= len(doc) <= max_doc):
                continue
            query = _first_sentence(doc)
            if len(query) < 20:
                continue
            pairs.append(
                GoldPair(
                    query=query,
                    file=e["file"],
                    name=e["name"],
                    kind=e["kind"],
                    line_start=e["line_start"],
                    line_end=e["line_end"],
                )
            )
            if max_pairs and len(pairs) >= max_pairs:
                break
    return pairs


def _first_sentence(doc: str) -> str:
    first = _SENT_BREAK.split(doc.strip(), maxsplit=1)[0]
    return _WHITESPACE.sub(" ", first).strip()[:140]


# ---------------------------------------------------------------------------
# Retriever registry
# ---------------------------------------------------------------------------

Retriever = Callable[[str, Path, int], List[Hit]]
RETRIEVERS: Dict[str, Retriever] = {}


def register(name: str):
    def deco(fn: Retriever) -> Retriever:
        RETRIEVERS[name] = fn
        return fn
    return deco


def _parse_sigil_search_json(stdout: str) -> List[Hit]:
    try:
        data = json.loads(stdout)
    except json.JSONDecodeError:
        return []
    hits: List[Hit] = []
    for r in data:
        hits.append(
            Hit(
                file=r.get("file", ""),
                name=r.get("name"),
                line_start=r.get("line") or r.get("line_start"),
                line_end=r.get("line_end"),
                snippet=r.get("sig") or r.get("name") or "",
            )
        )
    return hits


def _sigil_search(args: List[str], root: Path) -> List[Hit]:
    try:
        proc = subprocess.run(
            ["sigil", "search", *args, "--json"],
            cwd=root,
            capture_output=True,
            text=True,
            timeout=15,
        )
    except (subprocess.TimeoutExpired, FileNotFoundError):
        return []
    if proc.returncode != 0 or not proc.stdout.strip():
        return []
    return _parse_sigil_search_json(proc.stdout)


# Baseline: agent-shaped sigil search. `sigil search` does substring matching
# (not FTS5 tokenized despite the help text — multi-word queries miss
# everything). Agents work around this by picking the single longest
# distinctive token and probing with that. We mirror that pattern: try the
# top-N distinctive terms one at a time, union the results in order.
@register("sigil_search")
def retriever_sigil_search(query: str, root: Path, k: int) -> List[Hit]:
    terms = _grep_terms(query, n=3)
    if not terms:
        return []
    seen: set = set()
    merged: List[Hit] = []
    per_term_limit = max(k // max(len(terms), 1), 3)
    for term in terms:
        for h in _sigil_search([term, "--limit", str(per_term_limit)], root):
            key = (h.file, h.name, h.line_start)
            if key in seen:
                continue
            seen.add(key)
            merged.append(h)
            if len(merged) >= k:
                return merged
    return merged


# Naive variant: feed the verbatim natural-language query. Mostly useful as
# a "what happens if an agent doesn't preprocess" sanity-check baseline.
@register("sigil_search_verbatim")
def retriever_sigil_search_verbatim(query: str, root: Path, k: int) -> List[Hit]:
    return _sigil_search([query, "--limit", str(k)], root)


def _sigil_semantic(query: str, root: Path, k: int, extra: List[str]) -> List[Hit]:
    try:
        proc = subprocess.run(
            ["sigil", "semantic", query, "--json", "--limit", str(k), *extra],
            cwd=root,
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (subprocess.TimeoutExpired, FileNotFoundError):
        return []
    if proc.returncode != 0 or not proc.stdout.strip():
        return []
    return _parse_sigil_search_json(proc.stdout)


# Spike 1: BM25 retrieval over `name + qualified_name + sig + doc`.
@register("sigil_semantic_bm25")
def retriever_sigil_semantic_bm25(query: str, root: Path, k: int) -> List[Hit]:
    return _sigil_semantic(query, root, k, [])


# Doc-masked variant: BM25 over `name + qualified_name + sig` only. Used to
# de-bias the docstring-bootstrap eval — queries are first sentences of
# docstrings, so including the gold entity's own doc in the indexed text
# inflates scores via self-referential overlap. Masking measures whether
# symbol-shape signal alone carries the win.
@register("sigil_semantic_bm25_no_doc")
def retriever_sigil_semantic_bm25_no_doc(query: str, root: Path, k: int) -> List[Hit]:
    return _sigil_semantic(query, root, k, ["--no-doc"])


# Spike 2: Model2Vec static-embedding retrieval. potion-code-16M (256-dim
# static vectors, mean-pooled, L2-normalised). Each entity is encoded as
# `name + sig + doc`; query is encoded once; ranks by cosine similarity.
# Today rebuilds the embedding index in-memory on every query — expensive
# at scale, but fine for measurement.
@register("sigil_semantic_m2v")
def retriever_sigil_semantic_m2v(query: str, root: Path, k: int) -> List[Hit]:
    return _sigil_semantic(query, root, k, ["--m2v"])


# Spike 2 + doc-masked: Model2Vec over `name + sig` only. Honest comparison
# against `sigil_semantic_bm25_no_doc` — measures whether embeddings beat
# lexical BM25 when both lose the docstring-overlap signal.
@register("sigil_semantic_m2v_no_doc")
def retriever_sigil_semantic_m2v_no_doc(query: str, root: Path, k: int) -> List[Hit]:
    return _sigil_semantic(query, root, k, ["--m2v", "--no-doc"])


# Spike 4: BM25 + code-aware rerank signals (test-file / vendored /
# definition-kind / file-rank). Pulls 3*k candidates from BM25 and
# reranks before truncating.
@register("sigil_semantic_bm25_rerank")
def retriever_sigil_semantic_bm25_rerank(query: str, root: Path, k: int) -> List[Hit]:
    return _sigil_semantic(query, root, k, ["--rerank"])


# Spike 4 + Model2Vec: same rerank signals over the m2v candidate set.
@register("sigil_semantic_m2v_rerank")
def retriever_sigil_semantic_m2v_rerank(query: str, root: Path, k: int) -> List[Hit]:
    return _sigil_semantic(query, root, k, ["--m2v", "--rerank"])


# --- semble baseline: import the upstream library directly. Indexes the
# `--root` repo once per process (we cache the SembleIndex globally so
# 200 queries don't pay the indexing cost 200 times) and runs in three
# modes for fair sub-comparison: HYBRID (full pipeline incl. rerank),
# BM25 only, SEMANTIC (embeddings) only.

_SEMBLE_CACHE: Dict[str, object] = {}


def _semble_index(root: Path):
    key = str(root.resolve())
    if key not in _SEMBLE_CACHE:
        from semble import SembleIndex
        _SEMBLE_CACHE[key] = SembleIndex.from_path(root)
    return _SEMBLE_CACHE[key]


def _semble_search(query: str, root: Path, k: int, mode: str) -> List[Hit]:
    try:
        idx = _semble_index(root)
        from semble import SearchMode
        results = idx.search(query, top_k=k, mode=SearchMode(mode))
    except Exception as e:
        print(f"semble error ({mode}): {e}", file=sys.stderr)
        return []
    hits: List[Hit] = []
    for r in results:
        c = r.chunk
        hits.append(
            Hit(
                file=c.file_path,
                line_start=c.start_line,
                line_end=c.end_line,
                snippet=c.content[:200] if c.content else "",
            )
        )
    return hits


@register("semble_hybrid")
def retriever_semble_hybrid(query: str, root: Path, k: int) -> List[Hit]:
    return _semble_search(query, root, k, "hybrid")


@register("semble_bm25")
def retriever_semble_bm25(query: str, root: Path, k: int) -> List[Hit]:
    return _semble_search(query, root, k, "bm25")


@register("semble_semantic")
def retriever_semble_semantic(query: str, root: Path, k: int) -> List[Hit]:
    return _semble_search(query, root, k, "semantic")


# Built-in baseline: classic grep over source files.
_STOPWORDS = set(
    """
the of to and a in is it for on are with as i its his they be at one have this
from or had by but what some we can out other were all there when up use your
how said an each she which do their time if will way about many then them write
would like so these her long make thing see him two has look more day could go
come did number sound no most people my over know than first water been call
who oil now find down made may part also into such only no does etc may
returns return given list dict set string str text return value object
""".split()
)


def _grep_terms(query: str, n: int = 3) -> List[str]:
    toks = re.findall(r"[A-Za-z_][A-Za-z0-9_]+", query.lower())
    distinctive = [t for t in toks if t not in _STOPWORDS and len(t) >= 4]
    distinctive.sort(key=len, reverse=True)
    return distinctive[:n] or toks[:n]


_SOURCE_INCLUDES = [
    "--include=*.rs",
    "--include=*.py",
    "--include=*.ts",
    "--include=*.tsx",
    "--include=*.js",
    "--include=*.jsx",
    "--include=*.go",
    "--include=*.java",
    "--include=*.kt",
    "--include=*.rb",
    "--include=*.php",
    "--include=*.cs",
    "--include=*.cpp",
    "--include=*.c",
    "--include=*.h",
    "--include=*.hpp",
    "--include=*.swift",
    "--include=*.scala",
]
_SOURCE_EXCLUDES = [
    "--exclude-dir=.sigil",
    "--exclude-dir=.git",
    "--exclude-dir=node_modules",
    "--exclude-dir=target",
    "--exclude-dir=dist",
    "--exclude-dir=build",
]


@register("grep")
def retriever_grep(query: str, root: Path, k: int) -> List[Hit]:
    terms = _grep_terms(query)
    if not terms:
        return []
    pattern = "|".join(re.escape(t) for t in terms)
    try:
        proc = subprocess.run(
            ["grep", "-rnE", *_SOURCE_INCLUDES, *_SOURCE_EXCLUDES, pattern, "."],
            cwd=root,
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (subprocess.TimeoutExpired, FileNotFoundError):
        return []
    # grep exit codes: 0 = match, 1 = no match, 2+ = error.
    if proc.returncode > 1:
        return []
    hits: List[Hit] = []
    for line in proc.stdout.splitlines()[:k]:
        parts = line.split(":", 2)
        if len(parts) < 3:
            continue
        path, lineno_s, content = parts
        if path.startswith("./"):
            path = path[2:]
        line_no: Optional[int] = int(lineno_s) if lineno_s.isdigit() else None
        hits.append(Hit(file=path, line_start=line_no, snippet=content.strip()))
    return hits


# ---------------------------------------------------------------------------
# Metrics
# ---------------------------------------------------------------------------


def rank_of_gold(hits: List[Hit], gold: GoldPair) -> Optional[int]:
    for i, h in enumerate(hits, start=1):
        if _hit_matches(h, gold):
            return i
    return None


def _hit_matches(h: Hit, g: GoldPair) -> bool:
    if not h.file:
        return False
    if _norm(h.file) != _norm(g.file):
        return False
    if h.name and g.name:
        # Symbol-aware retrievers report the entity name directly.
        if h.name == g.name or g.name == _bare_leaf(h.name):
            return True
        # Some retrievers report the qualifier (Class.method).
        if g.name in h.name.split("."):
            return True
    # Range overlap: hit covers (or is covered by) the gold's line range.
    # `[a,b]` overlaps `[c,d]` iff a <= d and b >= c. Hits without an
    # end (grep) use start == end. This catches both:
    #   - line-pinpoint hits whose line is inside the gold function
    #   - chunk hits (semble) whose range contains the gold function
    if h.line_start is not None:
        hs, he = h.line_start, h.line_end if h.line_end is not None else h.line_start
        if hs <= g.line_end and he >= g.line_start:
            return True
    return False


def _norm(p: str) -> str:
    return Path(p).as_posix().lstrip("./")


def _bare_leaf(qualified: str) -> str:
    return re.split(r"[.:/]", qualified)[-1]


def ndcg_at_k(rank: Optional[int], k: int = 10) -> float:
    if rank is None or rank > k:
        return 0.0
    # Binary relevance, single gold doc → IDCG = 1.
    return 1.0 / math.log2(rank + 1)


# ---------------------------------------------------------------------------
# Token counting (BPE via tiktoken if available; bytes/4 fallback)
# ---------------------------------------------------------------------------

try:
    import tiktoken  # type: ignore

    _ENC = tiktoken.get_encoding("o200k_base")

    def count_tokens(text: str) -> int:
        return len(_ENC.encode(text, disallowed_special=()))

    TOKENIZER = "o200k_base"
except ImportError:

    def count_tokens(text: str) -> int:
        return max(1, len(text) // 4)

    TOKENIZER = "bytes/4 (install tiktoken for BPE)"


def stitched_token_cost(hits: List[Hit], k: int = 10) -> int:
    parts: List[str] = []
    for h in hits[:k]:
        head = f"{h.file}:{h.line_start or '?'}"
        if h.name:
            head += f" {h.name}"
        parts.append(head)
        if h.snippet:
            parts.append(h.snippet)
    return count_tokens("\n".join(parts))


# ---------------------------------------------------------------------------
# Runner
# ---------------------------------------------------------------------------


def run_eval(
    retriever_name: str,
    pairs: List[GoldPair],
    root: Path,
    k: int = 20,
    verbose: bool = False,
) -> RetrieverReport:
    fn = RETRIEVERS[retriever_name]
    results: List[QueryResult] = []
    for p in pairs:
        t0 = time.perf_counter()
        hits = fn(p.query, root, k)
        elapsed = (time.perf_counter() - t0) * 1000.0
        results.append(QueryResult(query=p.query, gold=p, hits=hits, elapsed_ms=elapsed))
        if verbose:
            r = rank_of_gold(hits, p)
            print(f"  [{retriever_name}] rank={r!s:>4} {p.name} ({p.file})", file=sys.stderr)
    return _summarise(retriever_name, results)


def _summarise(name: str, results: List[QueryResult]) -> RetrieverReport:
    n = len(results)
    ranks = [rank_of_gold(r.hits, r.gold) for r in results]

    def recall_at(k: int) -> float:
        return sum(1 for r in ranks if r is not None and r <= k) / n if n else 0.0

    ndcgs = [ndcg_at_k(r, 10) for r in ranks]
    tokens = sorted(stitched_token_cost(r.hits, 10) for r in results)
    elapsed = sorted(r.elapsed_ms for r in results)

    def median(xs):
        return xs[len(xs) // 2] if xs else 0.0

    buckets = {"1": 0, "2-5": 0, "6-10": 0, "11-20": 0, ">20 or miss": 0}
    for r in ranks:
        if r is None or r > 20:
            buckets[">20 or miss"] += 1
        elif r == 1:
            buckets["1"] += 1
        elif r <= 5:
            buckets["2-5"] += 1
        elif r <= 10:
            buckets["6-10"] += 1
        else:
            buckets["11-20"] += 1

    return RetrieverReport(
        retriever=name,
        n_queries=n,
        ndcg_at_10=sum(ndcgs) / n if n else 0.0,
        recall_at_1=recall_at(1),
        recall_at_5=recall_at(5),
        recall_at_10=recall_at(10),
        recall_at_20=recall_at(20),
        median_tokens_at_10=median(tokens),
        median_elapsed_ms=median(elapsed),
        rank_distribution=buckets,
    )


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def _print_summary(reports: List[RetrieverReport]) -> None:
    headers = ["retriever", "ndcg@10", "r@1", "r@5", "r@10", "r@20", "tok@10", "ms"]
    print(f"{headers[0]:<22}" + "".join(f"{h:>10}" for h in headers[1:]))
    print("-" * (22 + 10 * (len(headers) - 1)))
    for r in reports:
        print(
            f"{r.retriever:<22}"
            f"{r.ndcg_at_10:>10.3f}"
            f"{r.recall_at_1:>10.3f}"
            f"{r.recall_at_5:>10.3f}"
            f"{r.recall_at_10:>10.3f}"
            f"{r.recall_at_20:>10.3f}"
            f"{r.median_tokens_at_10:>10.0f}"
            f"{r.median_elapsed_ms:>10.1f}"
        )
    print()
    print("rank distribution (out of n queries):")
    for r in reports:
        dist = r.rank_distribution
        print(
            f"  {r.retriever:<22} "
            f"rank=1:{dist['1']:>4}  2-5:{dist['2-5']:>4}  "
            f"6-10:{dist['6-10']:>4}  11-20:{dist['11-20']:>4}  "
            f">20/miss:{dist['>20 or miss']:>4}"
        )
    print(f"\ntokenizer: {TOKENIZER}")


def main() -> None:
    ap = argparse.ArgumentParser(description="sigil semantic-search eval harness")
    ap.add_argument("--root", type=Path, default=Path("."))
    ap.add_argument(
        "--entities",
        type=Path,
        default=None,
        help="path to .sigil/entities.jsonl (default: <root>/.sigil/entities.jsonl)",
    )
    ap.add_argument(
        "--retrievers",
        default="sigil_search,grep",
        help=f"comma-separated retriever names; registered: {sorted(RETRIEVERS)}",
    )
    ap.add_argument("--max-pairs", type=int, default=200)
    ap.add_argument("--k", type=int, default=20)
    ap.add_argument("--out", type=Path, default=None)
    ap.add_argument("--verbose", action="store_true")
    args = ap.parse_args()

    entities = args.entities or (args.root / ".sigil" / "entities.jsonl")
    if not entities.exists():
        print(f"error: {entities} not found. Run `sigil index` first.", file=sys.stderr)
        sys.exit(2)

    pairs = extract_gold_pairs(entities, max_pairs=args.max_pairs)
    print(f"loaded {len(pairs)} (query, gold) pairs from {entities}", file=sys.stderr)
    if not pairs:
        sys.exit(2)

    reports: List[RetrieverReport] = []
    for name in (n.strip() for n in args.retrievers.split(",")):
        if name not in RETRIEVERS:
            print(
                f"error: unknown retriever {name!r}; have: {sorted(RETRIEVERS)}",
                file=sys.stderr,
            )
            sys.exit(2)
        print(f"running retriever: {name}", file=sys.stderr)
        reports.append(run_eval(name, pairs, args.root, k=args.k, verbose=args.verbose))

    payload = {
        "schema": "sigil-semantic-eval/v1",
        "root": str(args.root.resolve()),
        "n_pairs": len(pairs),
        "k": args.k,
        "tokenizer": TOKENIZER,
        "reports": [asdict(r) for r in reports],
    }
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps(payload, indent=2))
        print(f"wrote {args.out}", file=sys.stderr)
    print()
    _print_summary(reports)


if __name__ == "__main__":
    main()
