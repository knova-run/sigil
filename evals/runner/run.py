"""
Eval runner v0.0.1 — E2 navigation.

Runs one task × arm × seed, or a sweep over all tasks. Records token
counts + the model's final answer to a JSON result file for later
grading.

Usage:
  python evals/runner/run.py --task evals/tasks/E2_navigation/001-*.yaml \
                             --arm treatment --seed 1 \
                             --model claude-haiku-4-5

  python evals/runner/run.py --sweep --model claude-sonnet-4-6 --seeds 3

The runner does NOT grade — that's grade.py's job. It writes a result
JSON per (task, arm, seed) and moves on.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import random
import re
import shlex
import subprocess
import sys
from pathlib import Path
from typing import Any

import yaml

REPO_ROOT = Path(__file__).resolve().parents[2]
TASKS_ROOT = REPO_ROOT / "evals" / "tasks"
RESULTS_ROOT = REPO_ROOT / "evals" / "results"


def task_repo_root(task: dict) -> Path:
    """Effective CWD for the agent's tools. Defaults to the sigil repo.
    Tasks targeting external codebases (SWE-bench-like) set `repo:
    evals/_workbench/<slug>` to point at a cloned tree."""
    if "repo" in task and task["repo"]:
        p = REPO_ROOT / task["repo"]
        if not p.exists():
            raise RuntimeError(f"task {task['id']} references missing repo: {p}")
        return p.resolve()
    return REPO_ROOT

# Directive blurb for the treatment arm. Intentionally prescriptive
# about when to use each sigil primitive vs grep/read_file — prior
# "capability-describing only" phrasing led agents to default to grep
# habits even with sigil on PATH. When this blurb accompanies the
# native sigil_* tools in the manifest, the agent picks from a
# labeled decision tree rather than improvising.
SIGIL_BLURB = """\
sigil_* tools give pre-computed structural code intelligence:

  "where is X defined?"       → sigil_where(X) [add parent=C / file=F]
  "how does X fit?"           → sigil_context(X) [add with_body=true]
  "who calls X?"              → sigil_callers(X)
  "what does X call?"         → sigil_callees(X)
  "list names in file F"      → sigil_symbol_names(F)
  "tree under directory D"    → sigil_outline(D) [add kind=["class"]]

When the bug report names a literal string, constant, or error message
(e.g. "FILE_INPUT_CONTRADICTION", "invalid_choice"), use sigil_grep —
it's ripgrep plus the enclosing class/method on each hit, so one call
tells you both where the literal is AND which method owns it.
Not a first-resort tool for definition lookups — use sigil_where for
those.

Empty sigil results print `Did you mean: X, Y, Z?` on stderr — retry
with a suggested name before falling back to grep. sigil_where caps
at 10 rank-ordered hits; if the question names a class or file path,
pass parent=CLASS or file=PATH_SUBSTR up front.

WORKED EXAMPLES (pay attention to what the question names vs what you
query — you query the METHOD by name, not the class):

  Q: "Find the method on class `Parameter` that resolves the default
      value when a callable is passed to click.option(default=...)."
  GOOD (1 turn): sigil_where(symbol="get_default")
    → {"definitions":[{"parent":"Parameter","file":"src/click/core.py",
        "line":2249,"sig":"def get_default(...)"}]}
  BAD: sigil_where(symbol="Parameter") — returns the CLASS, not the
       method you want. Then you spend 10+ turns reading 600 lines.

  Q: "When a user submits a bad choice, the error message shows
      `%(value)s` literally instead of substituting — the issue is
      specific to ModelChoiceField."
  GOOD (1 turn): sigil_where(symbol="to_python", parent="ModelChoiceField")
    → exact method, one row.
"""

SYSTEM_PROMPT_BASE = """\
You are helping answer a navigation question about a code repository at
{repo}. You have tools: read_file, grep, glob, bash. Be efficient — aim
for the minimum number of tool calls that gives you a confident answer.

When you have the answer, reply with ONLY valid JSON as the final
assistant message (no prose, no markdown fences, no extra text). The
shape is specified by the question — it may be a JSON array, a JSON
object, or another JSON value. Match the exact shape requested.
"""

BASE_TOOLS = [
    {
        "name": "read_file",
        "description": "Read a file from the repository. Returns up to 200 lines by default; pass `limit` up to 5000 for longer spans. Prefer narrow reads — whole-file reads become context bloat on subsequent turns.",
        "input_schema": {
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Repo-relative path"},
                "offset": {"type": "integer", "description": "Optional 1-based line offset", "default": 1},
                "limit": {"type": "integer", "description": "Max lines to return", "default": 200},
            },
            "required": ["path"],
        },
    },
    {
        "name": "grep",
        "description": "ripgrep over the repo. Returns matching lines with file:line prefix.",
        "input_schema": {
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "glob": {"type": "string", "description": "Optional file glob filter"},
            },
            "required": ["pattern"],
        },
    },
    {
        "name": "glob",
        "description": "List repo files matching a glob pattern.",
        "input_schema": {
            "type": "object",
            "properties": {"pattern": {"type": "string"}},
            "required": ["pattern"],
        },
    },
    {
        "name": "bash",
        "description": "Run a shell command in the repo root. Returns stdout+stderr.",
        "input_schema": {
            "type": "object",
            "properties": {"command": {"type": "string"}},
            "required": ["command"],
        },
    },
]

# Native sigil tools — only exposed to the treatment arm. The agent sees
# them as first-class tools (same tier as read_file/grep) rather than as
# "bash commands to remember." This mirrors the production shape where
# sigil lives behind an MCP / hook integration.
SIGIL_TOOLS = [
    {
        "name": "sigil_grep",
        "description": "Text search (ripgrep semantics) with each hit annotated by the enclosing class/method. Use when the question names a specific literal, constant, or error string and you need to know which method owns the match — one call replaces grep+read_file. Not the right tool for 'where is X defined?' (use sigil_where) or 'what's in file F' (use sigil_symbol_names).",
        "input_schema": {
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regex pattern (or literal string with fixed_strings=true)."},
                "ignore_case": {"type": "boolean", "description": "Case-insensitive (grep -i)"},
                "word": {"type": "boolean", "description": "Whole-word match (grep -w)"},
                "fixed_strings": {"type": "boolean", "description": "Treat pattern as literal, not regex (grep -F)"},
                "file": {"type": "array", "items": {"type": "string"}, "description": "File-path substring filter (repeatable)"},
                "glob": {"type": "array", "items": {"type": "string"}, "description": "Glob patterns for file paths (ripgrep-style)"},
                "class": {"type": "string", "description": "Only hits whose enclosing entity's parent class equals this (or tail-equals). Matches the `--parent` flag on sigil_where."},
                "caller": {"type": "string", "description": "Only hits whose enclosing entity name equals FN."},
                "limit": {"type": "integer", "description": "Max hits. 0 = unlimited. Default 50."},
                "group_by": {"type": "string", "description": "Aggregate counts instead of rows. Values: file, class, entity, kind."},
            },
            "required": ["pattern"],
        },
    },
    {
        "name": "sigil_where",
        "description": "FIRST choice for 'where is X defined?' questions. Returns rows ranked by file-importance desc, capped at 10 by default. Each row: file, line, class (parent), signature, overload count. Tail-segment match: `get_default` finds `Parameter.get_default` and `Option.get_default`. When the bug report names a class or path, pass `parent` or `file` to skip the wide search. When >10 hits, prefer filtering over raising `limit`.",
        "input_schema": {
            "type": "object",
            "properties": {
                "symbol": {"type": "string", "description": "Symbol name. Tail segment matched exactly."},
                "parent": {"type": "string", "description": "Exact match on enclosing class/module (or its tail segment). Use when the question names a specific class like 'ModelChoiceField'."},
                "file": {"type": "string", "description": "Substring match on file path. Use when the question scopes to a subtree like 'django/forms'."},
                "limit": {"type": "integer", "description": "Max rows. 0 = unlimited. Default 10."},
                "include_tests": {"type": "boolean", "description": "Include test-file definitions (default false)", "default": False},
            },
            "required": ["symbol"],
        },
    },
    {
        "name": "sigil_context",
        "description": "Full bundle for a symbol: signature, callers, callees, related types, and inheritance overrides. Use when you need to understand how X fits into the codebase — replaces multiple search+read_file pairs. Pass `with_body=true` to also inline the source body (lines line_start..=line_end), which saves a follow-up read_file in 'locate then read' flows.",
        "input_schema": {
            "type": "object",
            "properties": {
                "symbol": {"type": "string", "description": "Symbol name. Accepts `Parent.name` or `file.rs::Parent::name` forms."},
                "with_body": {"type": "boolean", "description": "Include source body inline (default false)"},
            },
            "required": ["symbol"],
        },
    },
    {
        "name": "sigil_callers",
        "description": "Who calls `name`? Returns file+caller+line for every call-site. Pass `group_by: 'file'` for a {file: count} summary when you only need distribution.",
        "input_schema": {
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "kind": {"type": "string", "description": "Optional: call | import | type_annotation | instantiation"},
                "group_by": {"type": "string", "description": "Optional: file | caller | name | kind"},
            },
            "required": ["name"],
        },
    },
    {
        "name": "sigil_callees",
        "description": "What does `caller` call? Returns file+target+line. Pass `group_by: 'name'` for a count-per-target summary.",
        "input_schema": {
            "type": "object",
            "properties": {
                "caller": {"type": "string"},
                "kind": {"type": "string", "description": "Optional: call | import | type_annotation | instantiation"},
                "group_by": {"type": "string", "description": "Optional: file | name | kind"},
            },
            "required": ["caller"],
        },
    },
    {
        "name": "sigil_symbol_names",
        "description": "Flat JSON array of names of the symbols in ONE file. The right tool for \"list the structs / functions / classes in file F\". Tiny payload (~300 bytes for a mid-sized file).",
        "input_schema": {
            "type": "object",
            "properties": {
                "file": {"type": "string", "description": "Repo-relative file path"},
                "depth": {"type": "integer", "description": "1 = top-level items only (skip nested methods, imports, variables). Usually 1.", "default": 1},
            },
            "required": ["file"],
        },
    },
    {
        "name": "sigil_symbol_details",
        "description": "Full entity records for ONE file: kind, parent class, signature, line range. Use when you need sigs or line ranges per symbol; otherwise prefer sigil_symbol_names.",
        "input_schema": {
            "type": "object",
            "properties": {
                "file": {"type": "string", "description": "Repo-relative file path"},
                "depth": {"type": "integer", "description": "1 = top-level items only; omit for a full entity dump.", "default": 0},
            },
            "required": ["file"],
        },
    },
    {
        "name": "sigil_outline",
        "description": "Hierarchical top-level tree of classes + functions grouped by file across the repo (or under `path`). Answers 'what's in this directory structurally?' without needing multiple sigil_symbols calls. Pass `kind` (e.g. `[\"class\"]`) to restrict the payload — matches `grep -n \"^class \"` exactly but across the structural index.",
        "input_schema": {
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Restrict to files starting with this prefix"},
                "kind": {"type": "array", "items": {"type": "string"}, "description": "Optional: restrict to entities of these kinds (e.g. ['class']). Default: all outline-eligible kinds."},
            },
        },
    },
    {
        "name": "sigil_search",
        "description": "Substring search over symbol names across the whole codebase. Each row includes file, line, kind, parent class, and signature preview. Empty result means `no such symbol` — sigil will suggest close matches on stderr; don't abandon the tool on the first miss.",
        "input_schema": {
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Name substring. Prefer short specific terms."},
                "kind": {"type": "string", "description": "Optional: function | class | method | struct | enum | trait"},
            },
            "required": ["query"],
        },
    },
]


def tools_for_arm(arm: str) -> list:
    """Control gets the base toolkit only. Treatment gets base + sigil
    primitives as first-class tools (not just bash invocations). This
    puts sigil at the same level of abstraction as grep — the agent
    picks between them rather than choosing to shell out."""
    if arm == "treatment":
        return BASE_TOOLS + SIGIL_TOOLS
    return BASE_TOOLS


def arm_env(arm: str) -> dict[str, str]:
    """PATH is the knob that distinguishes arms. Treatment includes sigil's dir."""
    env = os.environ.copy()
    if arm == "control":
        # Strip any directory on PATH that contains a `sigil` binary.
        clean = []
        for d in env.get("PATH", "").split(":"):
            if d and not (Path(d) / "sigil").exists():
                clean.append(d)
        env["PATH"] = ":".join(clean) or "/usr/bin:/bin"
    elif arm == "treatment":
        # Prepend the repo's release build dir if present (so local sigil wins).
        local_bin = REPO_ROOT / "target" / "release"
        if (local_bin / "sigil").exists():
            env["PATH"] = f"{local_bin}:{env.get('PATH', '')}"
    else:
        raise ValueError(f"unknown arm: {arm}")
    return env


def tool_read_file(inp: dict[str, Any], env: dict[str, str], cwd: Path) -> str:
    path = cwd / inp["path"]
    offset = max(1, int(inp.get("offset", 1)))
    # Default capped at 200 lines. Prior default of 2000 let a single read
    # on a 3k-line source file dump 90 KB into context, which then got
    # re-sent on every subsequent turn — Haiku's 453k-token E4 blowup was
    # exactly this. Explicit --limit N overrides up to 5000.
    limit = max(1, min(int(inp.get("limit", 200)), 5000))
    if not path.exists():
        return f"ERROR: {path} not found"
    lines = path.read_text(errors="replace").splitlines()
    sliced = lines[offset - 1 : offset - 1 + limit]
    return "\n".join(f"{i + offset:6d}\t{line}" for i, line in enumerate(sliced))


def tool_grep(inp: dict[str, Any], env: dict[str, str], cwd: Path) -> str:
    cmd = ["rg", "--line-number", "--no-heading", inp["pattern"]]
    if inp.get("glob"):
        cmd += ["--glob", inp["glob"]]
    return run_subprocess(cmd, env, cwd)


def tool_glob(inp: dict[str, Any], env: dict[str, str], cwd: Path) -> str:
    # Use a simple `find`-less approach via python glob for reliability.
    from glob import glob
    hits = sorted(glob(inp["pattern"], root_dir=cwd, recursive=True))
    return "\n".join(hits) if hits else "(no matches)"


def tool_bash(inp: dict[str, Any], env: dict[str, str], cwd: Path) -> str:
    return run_subprocess(["bash", "-c", inp["command"]], env, cwd)


def run_subprocess(cmd: list[str], env: dict[str, str], cwd: Path) -> str:
    try:
        proc = subprocess.run(
            cmd, cwd=cwd, env=env, capture_output=True, text=True, timeout=30
        )
    except subprocess.TimeoutExpired:
        return "ERROR: timeout after 30s"
    out = proc.stdout + proc.stderr
    if len(out) > 20000:
        out = out[:20000] + "\n... [truncated]"
    return out or "(no output)"


def _sigil_cmd(env: dict[str, str], cwd: Path, args: list[str]) -> str:
    """Shell out to the sigil binary in the treatment arm's env and
    return stdout+stderr. Keeps the tool-level abstraction thin — the
    sigil CLI stays the single source of truth; the runner only
    translates tool_use JSON into CLI args."""
    return run_subprocess(["sigil", *args], env, cwd)


def tool_sigil_where(inp: dict[str, Any], env: dict[str, str], cwd: Path) -> str:
    args = ["where", inp["symbol"], "--format", "json"]
    if inp.get("include_tests"):
        args.append("--include-tests")
    if inp.get("parent") is not None:
        args += ["--parent", str(inp["parent"])]
    if inp.get("file"):
        args += ["--file", str(inp["file"])]
    if "limit" in inp and inp["limit"] is not None:
        args += ["--limit", str(int(inp["limit"]))]
    return _sigil_cmd(env, cwd, args)


def tool_sigil_context(inp: dict[str, Any], env: dict[str, str], cwd: Path) -> str:
    args = ["context", inp["symbol"], "--format", "json"]
    if inp.get("with_body"):
        args.append("--with-body")
    return _sigil_cmd(env, cwd, args)


def tool_sigil_callers(inp: dict[str, Any], env: dict[str, str], cwd: Path) -> str:
    args = ["callers", inp["name"]]
    if inp.get("kind"):
        args += ["--kind", inp["kind"]]
    if inp.get("group_by"):
        args += ["--group-by", inp["group_by"]]
    else:
        args.append("--json")
    return _sigil_cmd(env, cwd, args)


def tool_sigil_callees(inp: dict[str, Any], env: dict[str, str], cwd: Path) -> str:
    args = ["callees", inp["caller"]]
    if inp.get("kind"):
        args += ["--kind", inp["kind"]]
    if inp.get("group_by"):
        args += ["--group-by", inp["group_by"]]
    else:
        args.append("--json")
    return _sigil_cmd(env, cwd, args)


def tool_sigil_symbol_names(inp: dict[str, Any], env: dict[str, str], cwd: Path) -> str:
    args = ["symbols", inp["file"], "--json", "--names-only"]
    depth = int(inp.get("depth", 1))
    if depth == 1:
        args += ["--depth", "1"]
    return _sigil_cmd(env, cwd, args)


def tool_sigil_symbol_details(inp: dict[str, Any], env: dict[str, str], cwd: Path) -> str:
    args = ["symbols", inp["file"], "--json"]
    if int(inp.get("depth", 0)) == 1:
        args += ["--depth", "1"]
    return _sigil_cmd(env, cwd, args)


def tool_sigil_outline(inp: dict[str, Any], env: dict[str, str], cwd: Path) -> str:
    args = ["outline", "--format", "json"]
    if inp.get("path"):
        args += ["--path", inp["path"]]
    if inp.get("kind"):
        kinds = inp["kind"] if isinstance(inp["kind"], list) else [inp["kind"]]
        for k in kinds:
            args += ["--kind", str(k)]
    return _sigil_cmd(env, cwd, args)


def tool_sigil_search(inp: dict[str, Any], env: dict[str, str], cwd: Path) -> str:
    args = ["search", inp["query"], "--json"]
    if inp.get("kind"):
        args += ["--kind", inp["kind"]]
    return _sigil_cmd(env, cwd, args)


def tool_sigil_grep(inp: dict[str, Any], env: dict[str, str], cwd: Path) -> str:
    args = ["grep", inp["pattern"], "--format", "json"]
    if inp.get("ignore_case"):
        args.append("-i")
    if inp.get("word"):
        args.append("-w")
    if inp.get("fixed_strings"):
        args.append("-F")
    for f in (inp.get("file") or []):
        args += ["--file", str(f)]
    for g in (inp.get("glob") or []):
        args += ["--glob", str(g)]
    if inp.get("class") is not None:
        args += ["--class", str(inp["class"])]
    if inp.get("caller"):
        args += ["--caller", str(inp["caller"])]
    if "limit" in inp and inp["limit"] is not None:
        args += ["--limit", str(int(inp["limit"]))]
    if inp.get("group_by"):
        args += ["--group-by", str(inp["group_by"])]
    return _sigil_cmd(env, cwd, args)


DISPATCH = {
    "read_file": tool_read_file,
    "grep": tool_grep,
    "glob": tool_glob,
    "bash": tool_bash,
    "sigil_grep": tool_sigil_grep,
    "sigil_where": tool_sigil_where,
    "sigil_context": tool_sigil_context,
    "sigil_callers": tool_sigil_callers,
    "sigil_callees": tool_sigil_callees,
    "sigil_symbol_names": tool_sigil_symbol_names,
    "sigil_symbol_details": tool_sigil_symbol_details,
    "sigil_outline": tool_sigil_outline,
    "sigil_search": tool_sigil_search,
}


def run_one(client, task: dict, arm: str, seed: int, model: str, max_turns: int = 20) -> dict:
    env = arm_env(arm)
    cwd = task_repo_root(task)
    system = SYSTEM_PROMPT_BASE.format(repo=cwd.name)
    if arm == "treatment":
        system += "\n" + SIGIL_BLURB

    messages = [{"role": "user", "content": task["question"]}]
    turns = 0
    tokens_in = 0
    tokens_out = 0
    final_text = None
    trace: list[dict] = []  # compact per-turn record: tool calls + result previews

    tools = tools_for_arm(arm)
    while turns < max_turns:
        resp = client.messages.create(
            model=model,
            max_tokens=4096,
            system=system,
            tools=tools,
            messages=messages,
        )
        turns += 1
        tokens_in += resp.usage.input_tokens
        tokens_out += resp.usage.output_tokens

        # Always capture the latest text block, regardless of stop_reason.
        # Lets us grade runs that hit max_tokens mid-reasoning as well as
        # clean end_turn completions — previously those saved as final_text
        # = None and graded as no_answer even when the answer was visible.
        for block in resp.content:
            if block.type == "text":
                final_text = block.text

        if resp.stop_reason == "end_turn":
            break

        if resp.stop_reason == "tool_use":
            assistant_content = [b.model_dump() for b in resp.content]
            messages.append({"role": "assistant", "content": assistant_content})
            tool_results = []
            for block in resp.content:
                if block.type == "tool_use":
                    fn = DISPATCH.get(block.name)
                    if fn is None:
                        result = f"ERROR: unknown tool {block.name}"
                    else:
                        try:
                            result = fn(block.input, env, cwd)
                        except Exception as e:
                            result = f"ERROR: {type(e).__name__}: {e}"
                    # API rejects tool_result content that is an empty string.
                    if not result:
                        result = "(empty)"
                    trace.append({
                        "turn": turns,
                        "tool": block.name,
                        "input": block.input,
                        "result_len": len(result),
                        "result_preview": result[:400],
                    })
                    tool_results.append({
                        "type": "tool_result",
                        "tool_use_id": block.id,
                        "content": result,
                    })
            if not tool_results:
                # Malformed response: stop_reason=tool_use but no tool_use blocks.
                break
            messages.append({"role": "user", "content": tool_results})
            continue

        # Any other stop_reason (max_tokens, refusal) ends the run.
        break

    return {
        "task_id": task["id"],
        "arm": arm,
        "seed": seed,
        "model": model,
        "turns": turns,
        "tokens_in": tokens_in,
        "tokens_out": tokens_out,
        "final_text": final_text,
        "parsed_answer": parse_answer(final_text),
        "trace": trace,
        "timestamp": dt.datetime.now(dt.timezone.utc).isoformat(),
    }


def parse_answer(text: str | None) -> Any | None:
    """Extract a JSON value (array or object) from free-form text.

    Strategy: scan every balanced `[...]` and `{...}` substring, parse
    each, and return the last one that deserializes successfully. The
    "last" bias matches the convention that the final assistant message
    closes with the answer; earlier brackets are usually quoted text
    (e.g. the prompt echoing `#[cfg(test)]` or a JSON snippet in the
    instructions).
    """
    if not text:
        return None
    last_valid: Any = None
    for candidate in _balanced_groups(text):
        try:
            val = json.loads(candidate)
        except json.JSONDecodeError:
            continue
        if isinstance(val, (list, dict)):
            last_valid = val
    return last_valid


def _balanced_groups(text: str) -> list[str]:
    """Yield every balanced `[...]` and `{...}` substring.

    Handles nesting, skips over string literals (so `[` inside a quoted
    string doesn't open a group). Same balancing logic for both bracket
    kinds — useful because tasks may ask for arrays OR objects.
    """
    out: list[str] = []
    stack: list[tuple[str, int]] = []  # (closing_char, start_index)
    i = 0
    n = len(text)
    while i < n:
        c = text[i]
        if c == '"':
            # Skip past the string literal (respect \" escape).
            i += 1
            while i < n and text[i] != '"':
                if text[i] == "\\" and i + 1 < n:
                    i += 2
                    continue
                i += 1
            i += 1
            continue
        if c == "[" or c == "{":
            close = "]" if c == "[" else "}"
            if not stack:
                stack.append((close, i))
            else:
                stack.append((close, -1))  # nested — no new outer start
        elif c in "]}":
            if stack and stack[-1][0] == c:
                close, start = stack.pop()
                if not stack and start != -1:
                    out.append(text[start : i + 1])
            else:
                stack.clear()  # mismatched — reset
        i += 1
    return out


def load_task(path: Path) -> dict:
    with path.open() as f:
        return yaml.safe_load(f)


def _model_slug(model: str) -> str:
    # Short, filesystem-safe tag per model family.
    if "haiku" in model:
        return "haiku-4-5"
    if "sonnet" in model:
        return "sonnet-4-6"
    if "opus" in model:
        return "opus-4-7"
    return model.replace("/", "-").replace(":", "-")


def result_path(date: str, task_id: str, arm: str, seed: int, model: str, task_set: str) -> Path:
    # task_set slug is the short tag (E2, E4, etc.) derived from the
    # task-set dir name by taking the prefix before the first "_".
    slug = task_set.split("_", 1)[0] if "_" in task_set else task_set
    p = RESULTS_ROOT / date / _model_slug(model) / slug / f"{task_id}_{arm}_{seed}.json"
    p.parent.mkdir(parents=True, exist_ok=True)
    return p


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--task", type=Path, help="Single task YAML")
    ap.add_argument("--sweep", action="store_true", help="Run all tasks under --task-set (default: E2_navigation)")
    ap.add_argument("--task-set", default="E2_navigation",
                    help="Task directory under evals/tasks/ (e.g. E2_navigation, E4_swebench_like)")
    ap.add_argument("--arm", choices=["control", "treatment", "both"], default="both")
    ap.add_argument("--seed", type=int, help="Single seed (implies --seeds 1)")
    ap.add_argument("--seeds", type=int, default=3)
    ap.add_argument("--model", default="claude-sonnet-4-6")
    ap.add_argument("--max-turns", type=int, default=20)
    ap.add_argument("--dry-run", action="store_true", help="Print plan, don't call API")
    ap.add_argument(
        "--workers",
        type=int,
        default=1,
        help="Parallel task workers (ThreadPoolExecutor). Default 1 = sequential. Try 8 for Sonnet sweeps.",
    )
    args = ap.parse_args()

    if args.sweep:
        tasks = sorted((TASKS_ROOT / args.task_set).glob("*.yaml"))
    elif args.task:
        tasks = [args.task]
    else:
        ap.error("must pass --task or --sweep")

    arms = ["control", "treatment"] if args.arm == "both" else [args.arm]
    seeds = [args.seed] if args.seed is not None else list(range(1, args.seeds + 1))
    date = dt.date.today().isoformat()

    plan = [(t, a, s) for t in tasks for a in arms for s in seeds]
    print(f"Plan: {len(plan)} runs  ({len(tasks)} tasks × {len(arms)} arms × {len(seeds)} seeds)  model={args.model}")

    if args.dry_run:
        for t, a, s in plan:
            print(f"  {t.name:50s}  arm={a:9s}  seed={s}")
        return

    from anthropic import Anthropic  # imported lazily so --dry-run works without the SDK
    client = Anthropic()
    random.seed(0)
    random.shuffle(plan)

    # Skip runs whose result file already exists — keeps sweeps resumable
    # and lets the thread pool treat the remaining work as pure API fan-out.
    to_run: list[tuple] = []
    for t, a, s in plan:
        task = load_task(t)
        rp = result_path(date, task["id"], a, s, args.model, args.task_set)
        if rp.exists():
            print(f"skip (exists): {rp}")
            continue
        to_run.append((t, task, a, s, rp))

    def execute(item):
        t, task, a, s, rp = item
        try:
            result = run_one(client, task, a, s, args.model, args.max_turns)
        except Exception as e:
            result = {
                "task_id": task["id"],
                "arm": a,
                "seed": s,
                "model": args.model,
                "turns": 0,
                "tokens_in": 0,
                "tokens_out": 0,
                "final_text": None,
                "parsed_answer": None,
                "error": f"{type(e).__name__}: {e}",
                "timestamp": dt.datetime.now(dt.timezone.utc).isoformat(),
            }
        rp.write_text(json.dumps(result, indent=2))
        return (task["id"], a, s, result)

    if args.workers <= 1 or len(to_run) <= 1:
        for item in to_run:
            tid, a, s, _ = item[1]["id"], item[2], item[3], None  # readable unpack
            print(f"run: {tid}  arm={a}  seed={s}", flush=True)
            tid, arm, seed, result = execute(item)
            if "error" in result:
                print(f"  ! ERROR: {result['error']}")
            print(
                f"  → tokens_in={result['tokens_in']}  tokens_out={result['tokens_out']}  turns={result['turns']}"
            )
    else:
        # Parallel fan-out. Anthropic's Python client is thread-safe; each
        # worker makes independent message.create() calls. Per-task result
        # JSONs have unique paths so there's no write contention.
        from concurrent.futures import ThreadPoolExecutor, as_completed
        import time as _time

        n_workers = min(args.workers, len(to_run))
        print(f"parallel: running {len(to_run)} tasks with {n_workers} workers", flush=True)
        start = _time.time()
        done = 0
        with ThreadPoolExecutor(max_workers=n_workers) as pool:
            futures = {pool.submit(execute, item): item for item in to_run}
            for fut in as_completed(futures):
                tid, arm, seed, result = fut.result()
                done += 1
                tag = "ERROR " if "error" in result else ""
                print(
                    f"[{done}/{len(to_run)}] {tag}{tid:<8} arm={arm:<9} seed={seed}  "
                    f"tokens_in={result['tokens_in']:>7}  tokens_out={result['tokens_out']:>5}  "
                    f"turns={result['turns']}",
                    flush=True,
                )
        print(f"parallel: done in {_time.time() - start:.1f}s")


if __name__ == "__main__":
    main()
