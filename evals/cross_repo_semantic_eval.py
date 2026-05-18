#!/usr/bin/env python3
"""
Cross-repo semantic-search eval — same harness as `semantic_eval.py`, but
run across the curated corpus in `evals/corpus.tsv` instead of just
sigil-on-sigil.

Why: the single-repo eval showed sigil's BM25 retriever beating semble
~2x on NDCG@10, but that result has structural bias — sigil indexes
per-entity, semble indexes per-chunk, and our eval gold is "find the
specific function," which suits per-entity retrieval. This driver tests
whether the gap holds across repos of different sizes and languages
(Rust, Python, TypeScript, Go).

Procedure per repo:
  1. Clone (depth 200) to $CORPUS_DIR/<slug>/ if not already present.
  2. Checkout the pinned `ref` from corpus.tsv.
  3. Build `sigil index --full`.
  4. Extract docstring → entity gold pairs.
  5. Run each retriever against those pairs.
  6. Aggregate global metrics across repos (one row per retriever).

Usage:
    evals/.venv/bin/python evals/cross_repo_semantic_eval.py \\
        --max-pairs-per-repo 50 \\
        --retrievers sigil_semantic_bm25,sigil_semantic_m2v,semble_bm25,semble_hybrid \\
        --out evals/results/semantic-cross-repo-<date>.json
"""

import argparse
import json
import os
import statistics
import subprocess
import sys
from dataclasses import asdict
from pathlib import Path
from typing import Dict, List, Optional, Tuple

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "evals"))

from semantic_eval import (  # noqa: E402
    GoldPair,
    QueryResult,
    RetrieverReport,
    RETRIEVERS,
    TOKENIZER,
    extract_gold_pairs,
    rank_of_gold,
    ndcg_at_k,
    stitched_token_cost,
    run_eval,
)


def parse_corpus(path: Path) -> List[Tuple[str, str, str]]:
    """Yield (slug, url, ref) for every non-comment row in corpus.tsv."""
    rows: List[Tuple[str, str, str]] = []
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split("\t")
        if len(parts) < 3:
            continue
        slug, url, ref = parts[0], parts[1], parts[2]
        rows.append((slug, url, ref))
    return rows


def prepare_repo(slug: str, url: str, ref: str, corpus_dir: Path) -> Path:
    repo = corpus_dir / slug
    if not (repo / ".git").exists():
        print(f"  cloning {url} @ {ref} → {repo}", file=sys.stderr)
        # `--branch <ref>` works for both branches and tags and keeps the
        # clone shallow at the right point. Avoids the "shallow clone
        # doesn't include the tag" failure mode.
        r = subprocess.run(
            ["git", "clone", "--depth", "200", "--branch", ref, "--single-branch", url, str(repo)],
            capture_output=True,
            text=True,
        )
        if r.returncode != 0:
            raise RuntimeError(
                f"git clone --branch {ref} failed for {url}: {r.stderr.strip()}"
            )
    # Verify the ref is what we expect; if not (e.g. existing clone left
    # at a different ref), fetch + checkout.
    head = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=repo, capture_output=True, text=True
    ).stdout.strip()
    target = subprocess.run(
        ["git", "rev-parse", ref], cwd=repo, capture_output=True, text=True
    ).stdout.strip()
    if head != target and target:
        subprocess.run(
            ["git", "checkout", "--quiet", ref],
            cwd=repo,
            check=True,
            capture_output=True,
        )
    return repo


def build_index(repo: Path) -> None:
    sigil_path = repo / ".sigil" / "entities.jsonl"
    if sigil_path.exists():
        return
    print(f"  indexing {repo.name}", file=sys.stderr)
    r = subprocess.run(
        ["sigil", "index", "--root", str(repo), "--full"],
        capture_output=True,
        text=True,
    )
    if r.returncode != 0:
        raise RuntimeError(f"sigil index failed for {repo}: {r.stderr}")


def per_query_results(
    retriever_name: str,
    pairs: List[GoldPair],
    repo: Path,
    k: int,
) -> List[QueryResult]:
    """Run a retriever and return raw per-query data (rank/tokens/elapsed)."""
    fn = RETRIEVERS[retriever_name]
    results: List[QueryResult] = []
    import time
    for p in pairs:
        t0 = time.perf_counter()
        hits = fn(p.query, repo, k)
        elapsed = (time.perf_counter() - t0) * 1000.0
        results.append(QueryResult(query=p.query, gold=p, hits=hits, elapsed_ms=elapsed))
    return results


def summarise_global(
    retriever_name: str,
    results: List[QueryResult],
) -> RetrieverReport:
    """Re-implementation of semantic_eval._summarise that's callable from here."""
    n = len(results)
    ranks = [rank_of_gold(r.hits, r.gold) for r in results]

    def recall_at(k: int) -> float:
        return sum(1 for r in ranks if r is not None and r <= k) / n if n else 0.0

    ndcgs = [ndcg_at_k(r, 10) for r in ranks]
    tokens = sorted(stitched_token_cost(r.hits, 10) for r in results)
    elapsed = sorted(r.elapsed_ms for r in results)
    median = lambda xs: xs[len(xs) // 2] if xs else 0.0

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
        retriever=retriever_name,
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


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--corpus", type=Path, default=ROOT / "evals" / "corpus.tsv")
    ap.add_argument(
        "--corpus-dir",
        type=Path,
        default=Path(os.environ.get("CORPUS_DIR", "/tmp/sigil-eval-corpus")),
    )
    ap.add_argument("--max-pairs-per-repo", type=int, default=50)
    ap.add_argument(
        "--retrievers",
        default="sigil_semantic_bm25,sigil_semantic_m2v,semble_bm25,semble_semantic,semble_hybrid",
    )
    ap.add_argument("--k", type=int, default=20)
    ap.add_argument("--out", type=Path, default=None)
    args = ap.parse_args()

    args.corpus_dir.mkdir(parents=True, exist_ok=True)

    rows = parse_corpus(args.corpus)
    retriever_names = [r.strip() for r in args.retrievers.split(",")]
    for name in retriever_names:
        if name not in RETRIEVERS:
            print(f"unknown retriever: {name}; have: {sorted(RETRIEVERS)}", file=sys.stderr)
            sys.exit(2)

    per_repo_reports: Dict[str, Dict[str, RetrieverReport]] = {}
    # Aggregator: retriever_name → list of QueryResults across all repos
    global_results: Dict[str, List[QueryResult]] = {n: [] for n in retriever_names}

    for slug, url, ref in rows:
        print(f"\n=== {slug} ({ref}) ===", file=sys.stderr)
        try:
            repo = prepare_repo(slug, url, ref, args.corpus_dir)
            build_index(repo)
        except Exception as e:
            print(f"  skip {slug}: {e}", file=sys.stderr)
            continue
        entities = repo / ".sigil" / "entities.jsonl"
        pairs = extract_gold_pairs(entities, max_pairs=args.max_pairs_per_repo)
        if not pairs:
            print(f"  no doc-bearing entities in {slug}", file=sys.stderr)
            continue
        print(f"  {len(pairs)} gold pairs", file=sys.stderr)

        per_repo_reports[slug] = {}
        for name in retriever_names:
            print(f"    {name}…", file=sys.stderr)
            results = per_query_results(name, pairs, repo, args.k)
            per_repo_reports[slug][name] = summarise_global(name, results)
            global_results[name].extend(results)

    # Build aggregate (cross-repo) reports
    global_reports = {
        name: summarise_global(name, results)
        for name, results in global_results.items()
    }

    if args.out:
        payload = {
            "schema": "sigil-semantic-eval-cross-repo/v1",
            "corpus": str(args.corpus),
            "max_pairs_per_repo": args.max_pairs_per_repo,
            "tokenizer": TOKENIZER,
            "per_repo": {
                slug: {name: asdict(r) for name, r in repo_reports.items()}
                for slug, repo_reports in per_repo_reports.items()
            },
            "global": {name: asdict(r) for name, r in global_reports.items()},
        }
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps(payload, indent=2))
        print(f"\nwrote {args.out}", file=sys.stderr)

    # Markdown summary to stdout
    print()
    print(f"## Cross-repo semantic eval  (tokenizer: {TOKENIZER})\n")
    for slug, repo_reports in per_repo_reports.items():
        any_r = next(iter(repo_reports.values()))
        print(f"### {slug} — {any_r.n_queries} queries\n")
        _print_table(repo_reports)
        print()
    print("### Global (cross-repo aggregate)\n")
    _print_table(global_reports)


def _print_table(reports: Dict[str, RetrieverReport]) -> None:
    print("| Retriever | NDCG@10 | R@1 | R@5 | R@10 | R@20 | Tok@10 | Median ms |")
    print("|---|---:|---:|---:|---:|---:|---:|---:|")
    for name, r in reports.items():
        print(
            f"| `{name}` | {r.ndcg_at_10:.3f} | {r.recall_at_1:.3f} | "
            f"{r.recall_at_5:.3f} | {r.recall_at_10:.3f} | {r.recall_at_20:.3f} | "
            f"{r.median_tokens_at_10:.0f} | {r.median_elapsed_ms:.1f} |"
        )


if __name__ == "__main__":
    main()
