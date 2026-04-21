#!/usr/bin/env bash
# E4: SWE-bench-Lite-shaped evals. Currently covers:
#
#   001  pallets/click   @ 04ef3a6   "find Parameter.get_default"
#   002  django/django   @ 42e8cf4   "#32347 / PR 13933 — ModelChoiceField
#                                     invalid_choice doesn't substitute %(value)s"
#   003  django/django   @ 42e8cf4   synthetic bug report shaped like
#                                     FileField.to_python (tests --parent re-use
#                                     on a different class than 002)
#   004  django/django   @ 42e8cf4   enumerate top-level classes under
#                                     django/db/migrations/operations/ (tests
#                                     sigil_outline vs find+grep)
#
# Tasks 001/002 are real upstream bugs graded against the actual fix commit.
# 003/004 are synthetic but reuse the pinned Django checkout so no re-clone
# is required; they exercise eval dimensions 001/002 don't cover.
#
# Usage:
#   export ANTHROPIC_API_KEY=sk-ant-...
#   bash evals/runner/run-e4-click-spike.sh
#
# Cost estimate with 4 tasks × N=3 Sonnet + N=1 Haiku:
#   ~$4–8 total (Django tree is ~350k LOC vs click's 2.3k; 004 is outline-
#   shaped so the treatment path should be cheap).

set -euo pipefail

if [[ -z "${ANTHROPIC_API_KEY:-}" ]]; then
  echo "ERROR: export ANTHROPIC_API_KEY=... first" >&2
  exit 1
fi

cd "$(dirname "$0")/../.."

DATE=$(date -u +%Y-%m-%d)

echo "==================================="
echo "1/3: Haiku smoke (2 runs, ~\$0.20)"
echo "==================================="
python3 evals/runner/run.py \
  --task-set E4_swebench_like --sweep --seeds 1 \
  --model claude-haiku-4-5-20251001 --max-turns 20 --workers 12

echo
echo "==================================="
echo "2/3: Sonnet N=3 (6 runs, ~\$1-2)"
echo "==================================="
python3 evals/runner/run.py \
  --task-set E4_swebench_like --sweep --seeds 3 \
  --model claude-sonnet-4-6 --max-turns 30 --workers 12

echo
echo "==================================="
echo "3/3: Grade + per-arm token summary"
echo "==================================="
python3 evals/runner/grade.py "evals/results/$DATE/sonnet-4-6/E4"
echo
python3 - <<PY
import json, glob, statistics

for label, path in [
    ("Haiku N=1", "evals/results/$DATE/haiku-4-5/E4/*.json"),
    ("Sonnet N=3", "evals/results/$DATE/sonnet-4-6/E4/*.json"),
]:
    rows = [json.load(open(f)) for f in glob.glob(path) if 'summary' not in f]
    if not rows: continue
    print(f'\n{label}:')
    print(f'  {"arm":10} {"tokens_in":>10} {"tokens_out":>10} {"turns":>6}')
    for arm in ['control', 'treatment']:
        xs = [r for r in rows if r['arm']==arm]
        if not xs: continue
        med_in = statistics.median(r['tokens_in'] for r in xs)
        med_out = statistics.median(r['tokens_out'] for r in xs)
        med_turns = statistics.median(r['turns'] for r in xs)
        print(f'  {arm:10} {med_in:>10,.0f} {med_out:>10,.0f} {med_turns:>6.0f}')
    c = [r for r in rows if r['arm']=='control']
    t = [r for r in rows if r['arm']=='treatment']
    if c and t:
        ci = statistics.median(r['tokens_in'] for r in c)
        ti = statistics.median(r['tokens_in'] for r in t)
        ratio = ci/ti if ti else 0
        print(f'  control/treatment input ratio: {ratio:.2f}x ({"sigil wins" if ratio>1 else "control wins"})')
    tot_in = sum(r['tokens_in'] for r in rows)
    tot_out = sum(r['tokens_out'] for r in rows)
    cost = tot_in * 3/1e6 + tot_out*15/1e6 if 'sonnet' in label.lower() else tot_in*1/1e6 + tot_out*5/1e6
    print(f'  total cost this arm-set: ~\${cost:.2f}')
PY

echo
echo "Done. Results under: evals/results/$DATE/{haiku-4-5,sonnet-4-6}/E4/"
