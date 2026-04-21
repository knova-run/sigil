//! Platform hook installers — the distribution lever from §4.4.
//!
//! Each sub-module writes a platform-appropriate always-on instruction
//! that makes agents aware of sigil's commands *without leading the
//! prompt*. The text is strictly capability-describing (what each command
//! does, when it fits) — never preference-giving ("use sigil instead of
//! grep"). That's the honest answer to the leading-prompt problem in §11
//! of the plan.
//!
//! All installers are idempotent: re-running upgrades the sigil block in
//! place via sentinel markers; uninstallers remove only the sigil block
//! and leave user content intact.

pub mod aider;
pub mod claude;
pub mod codex;
pub mod copilot;
pub mod cursor;
pub mod gemini;
pub mod githook;
pub mod opencode;

/// Escape single quotes for safe embedding in a single-quoted shell string.
/// `it's` → `it'\''s`. Shared across installers that emit shell commands.
pub(crate) fn shell_escape_single_quoted(s: &str) -> String {
    s.replace('\'', "'\\''")
}

pub const MARKER_BEGIN: &str = "<!-- sigil:begin -->";
pub const MARKER_END: &str = "<!-- sigil:end -->";
pub const JSON_MARKER: &str = "sigil";

/// The capability block every installer embeds. Directive by design:
/// a question→command table so the agent picks sigil on-pattern rather
/// than improvising a grep chain. One worked example grounds the
/// "sigil-first" flow.
///
/// This phrasing was validated on the E4 click eval — adding the
/// flowchart + example dropped Sonnet treatment from 18,069 → 5,521
/// tokens (2.22× vs control), with all 3 seeds producing byte-
/// identical `sigil where ...` → answer paths in 2 turns.
pub fn capability_block() -> String {
    "This repo has sigil installed — a deterministic structural code \
intelligence tool (BLAKE3 + tree-sitter, no LLM inference). Sigil's \
pre-computed index answers structural questions in one shot, without \
multi-grep exploration.

## Structural questions — reach for sigil FIRST

| Question shape                           | Command                                        |
|------------------------------------------|------------------------------------------------|
| \"where is X defined?\"                  | `sigil where <X>`                              |
| \"where is X defined on class C?\"       | `sigil where <X> --parent <C>`                 |
| \"where is X in file/subtree F?\"        | `sigil where <X> --file <F>`                   |
| \"how does X fit in the codebase?\"      | `sigil context <X>`                            |
| \"locate X and show its body\"           | `sigil context <X> --with-body`                |
| \"who calls X?\"                         | `sigil callers <X>` (add `--group-by file`)    |
| \"what does X call?\"                    | `sigil callees <X>` (add `--group-by name`)    |
| \"list the Xs in file F (just names)\"   | `sigil symbols <F> --depth 1 --names-only`     |
| \"full entities in file F\"              | `sigil symbols <F> --depth 1`                  |
| \"structural tree under dir D\"          | `sigil outline --path <D>`                     |
| \"find anything matching 'foo'\"         | `sigil search foo`                             |
| \"impact of editing X?\"                 | `sigil blast <X>`                              |
| \"entity-level diff of a commit range\"  | `sigil diff A..B --markdown`                   |
| \"PR review (diff + blast + co-change)\" | `sigil review A..B --markdown`                 |
| \"diff two files without git\"           | `sigil diff --files OLD NEW`                   |
| \"clones / duplicated functions\"        | `sigil duplicates`                             |
| cold-start orientation                   | `sigil map --tokens N`                         |

All commands accept `--json` for machine-readable output. Script-facing \
commands (`symbols`, `children`, `callers`, `callees`, `search`) default \
to unbounded results, minified JSON. Pair with the pre-written \
`.sigil/SIGIL_MAP.md` (when `sigil index` has run) and file-level \
PageRank at `.sigil/rank.json`.

## File-system / text questions — use grep / read_file / bash

| Question shape                           | Tool                             |
|------------------------------------------|----------------------------------|
| \"which files exist under dir D?\"       | `ls` / `find` / `bash`           |
| \"text content X inside known file F\"   | `grep` / `read_file`             |
| \"lines matching a regex in the repo\"   | `grep`                           |
| language-specific syntactic pattern      | `grep` (e.g. Rust `^pub mod`)    |
| sigil returned empty AND no \"Did you    | `grep` — confirm the name        |
| mean?\" suggestion on stderr             | really doesn't exist textually   |

Two rules of thumb:

- `sigil_symbols` with `--names-only` when the answer is a LIST OF NAMES. \
  Typical drop: ~3 KB of full entity records → ~300 bytes.
- `sigil_outline` surfaces CLASSES AND FUNCTIONS under a path, not a \
  plain file listing. Use `ls` / `bash` for pure file enumeration.

Empty sigil results are data, not failure. Sigil prints a \
`Did you mean: X, Y, Z?` hint on stderr when the queried name is close \
to something known — retry with a suggestion before falling back to grep.

First-query note: sigil auto-runs `sigil index` if `.sigil/` is missing \
and emits a one-line `sigil: no index at ...` to stderr. Not an error — \
just zero-config onboarding. Set `SIGIL_NO_AUTO_INDEX=1` to disable.

## Worked example — one-shot find-definition

Q: *Find the method on class `Parameter` that resolves the default \
value when a callable is passed.*

**Bad path (grep-first, 4+ turns):**

```
grep -rn \"default\" src/**/*.py   # hundreds of hits
grep \"class Parameter\"           # narrow file
read_file src/click/core.py:1-200  # wrong range
read_file src/click/core.py:2000-  # finally find it
```

**Good path (1 sigil call):**

```
sigil where get_default
→ get_default
  Parameter.get_default  src/click/core.py:2249-2251  (method, 3 overloads)
    def get_default(self, ctx: Context, call: bool = True) -> Any
  Option.get_default     src/click/core.py:2891-2905  (method)
    def get_default(self, ctx: Context, call: bool = True) -> Any
```

Answer: `Parameter.get_default` at `src/click/core.py:2249`, with \
`Option.get_default` as an override. Done in one command.

## Worked example — many hits → narrow the search

`sigil where` caps at 10 rank-ordered rows and prints a one-line hint \
on stderr when more matched. When a bug report names a class or file, \
add a filter instead of scanning ranked rows:

```
sigil where to_python
# stderr: sigil: 38 definitions matched, showing top 10 by rank.
#         Narrow with `--parent CLASS`, `--file PATH_SUBSTR`, or
#         rerun with --limit 0.

sigil where to_python --parent ModelChoiceField
# → exactly 1 row, answer in one call
```

For compound filters the flags don't express, drop to SQL: `sigil query \
\"SELECT file, parent, line_start FROM entities WHERE name = 'to_python' \
AND parent LIKE '%Choice%'\"`."
        .to_string()
}

/// Wrap the capability block in the sentinel markers so we can round-trip it.
pub fn capability_block_with_markers() -> String {
    format!(
        "{MARKER_BEGIN}\n{}\n{MARKER_END}",
        capability_block()
    )
}

/// Insert or replace the sigil block in a Markdown-ish file. Preserves
/// any content outside the markers. Creates the file (with just the
/// block) if it doesn't exist.
pub fn upsert_marker_block(
    path: &std::path::Path,
    block_body: &str,
) -> std::io::Result<UpsertResult> {
    let wrapped = format!("{MARKER_BEGIN}\n{block_body}\n{MARKER_END}");
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // New file: ensure trailing newline.
        let content = format!("{wrapped}\n");
        std::fs::write(path, content)?;
        return Ok(UpsertResult::Created);
    }
    let existing = std::fs::read_to_string(path)?;
    let updated = replace_marker_block(&existing, &wrapped);
    if updated == existing {
        return Ok(UpsertResult::Unchanged);
    }
    std::fs::write(path, updated)?;
    Ok(UpsertResult::Updated)
}

/// Remove the sigil block from a file. Returns true when a block was
/// present and removed.
pub fn remove_marker_block(path: &std::path::Path) -> std::io::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let existing = std::fs::read_to_string(path)?;
    let (updated, removed) = strip_marker_block(&existing);
    if !removed {
        return Ok(false);
    }
    if updated.trim().is_empty() {
        // If removing the sigil block left the file empty, delete it so we
        // don't leave behind an empty CLAUDE.md / AGENTS.md.
        std::fs::remove_file(path)?;
    } else {
        std::fs::write(path, updated)?;
    }
    Ok(true)
}

/// Idempotent replacement: if the file already contains BEGIN..END, swap
/// the content. Otherwise append the new block separated by a blank line.
fn replace_marker_block(existing: &str, wrapped_block: &str) -> String {
    if let Some((begin, end_line_idx)) = find_marker_bounds(existing) {
        let before = &existing[..begin];
        let after = &existing[end_line_idx..];
        // Preserve the trailing newline that existed after the END marker so
        // round-tripping the same block doesn't create a no-content diff.
        let trailing_nl = if existing[..end_line_idx].ends_with('\n') && !after.starts_with('\n') {
            "\n"
        } else {
            ""
        };
        format!("{before}{wrapped_block}{trailing_nl}{after}")
    } else {
        let mut out = existing.to_string();
        if !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.ends_with("\n\n") && !out.is_empty() {
            out.push('\n');
        }
        out.push_str(wrapped_block);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out
    }
}

/// Strip the marker block. Returns (new_content, was_removed).
fn strip_marker_block(existing: &str) -> (String, bool) {
    match find_marker_bounds(existing) {
        Some((begin, end)) => {
            let before = &existing[..begin];
            let after = &existing[end..];
            // Collapse any blank-line fencing we added around the block.
            let mut out = String::with_capacity(existing.len());
            out.push_str(before.trim_end_matches('\n'));
            if !before.is_empty() && !after.is_empty() {
                out.push('\n');
            }
            out.push_str(after.trim_start_matches('\n'));
            (out, true)
        }
        None => (existing.to_string(), false),
    }
}

/// Returns (byte offset of BEGIN, byte offset AFTER the END line).
fn find_marker_bounds(s: &str) -> Option<(usize, usize)> {
    let begin = s.find(MARKER_BEGIN)?;
    let end_start = s[begin..].find(MARKER_END)?;
    let end_abs = begin + end_start + MARKER_END.len();
    // Include the trailing newline after MARKER_END, if any, so we don't
    // leave a floating blank line on removal.
    let end_with_newline = if s[end_abs..].starts_with('\n') {
        end_abs + 1
    } else {
        end_abs
    };
    Some((begin, end_with_newline))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertResult {
    Created,
    Updated,
    Unchanged,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_block_lists_all_shipped_commands() {
        let b = capability_block();
        for cmd in [
            "sigil map",
            "sigil context",
            "sigil review",
            "sigil blast",
            "sigil duplicates",
            "sigil callers",
            "sigil callees",
            "sigil symbols",
            "sigil search",
        ] {
            assert!(b.contains(cmd), "capability block missing `{cmd}`");
        }
    }

    #[test]
    fn capability_block_avoids_preference_language() {
        let b = capability_block().to_lowercase();
        // Honest non-leading text — describe the tool, don't instruct the
        // model. Callers of the plan read §11 to see why.
        for banned in [
            "prefer sigil",
            "use sigil instead",
            "don't use grep",
            "replace grep",
            "must use",
        ] {
            assert!(
                !b.contains(banned),
                "capability block contains preference language: `{banned}`"
            );
        }
    }

    #[test]
    fn replace_marker_block_roundtrips_without_changing_user_content() {
        let pre = "# CLAUDE.md\n\nAlready here text.\n\n<!-- sigil:begin -->\nold sigil stuff\n<!-- sigil:end -->\n\nTail content.\n";
        let wrapped = "<!-- sigil:begin -->\nnew sigil stuff\n<!-- sigil:end -->";
        let after = replace_marker_block(pre, wrapped);
        assert!(after.contains("# CLAUDE.md"));
        assert!(after.contains("Already here text."));
        assert!(after.contains("new sigil stuff"));
        assert!(!after.contains("old sigil stuff"));
        assert!(after.contains("Tail content."));
    }

    #[test]
    fn replace_marker_block_appends_when_markers_absent() {
        let pre = "# CLAUDE.md\n\nexisting\n";
        let wrapped = "<!-- sigil:begin -->\nblock\n<!-- sigil:end -->";
        let after = replace_marker_block(pre, wrapped);
        assert!(after.contains("existing"));
        assert!(after.contains("<!-- sigil:begin -->"));
        assert!(after.ends_with("<!-- sigil:end -->\n"));
    }

    #[test]
    fn strip_marker_block_removes_cleanly() {
        let pre = "# CLAUDE.md\n\nuser text\n\n<!-- sigil:begin -->\nsigil\n<!-- sigil:end -->\n\ntail\n";
        let (after, removed) = strip_marker_block(pre);
        assert!(removed);
        assert!(after.contains("# CLAUDE.md"));
        assert!(after.contains("user text"));
        assert!(after.contains("tail"));
        assert!(!after.contains("<!-- sigil:begin -->"));
    }

    #[test]
    fn strip_marker_block_idempotent_when_absent() {
        let pre = "# CLAUDE.md\nno sigil here\n";
        let (after, removed) = strip_marker_block(pre);
        assert!(!removed);
        assert_eq!(after, pre);
    }

    #[test]
    fn upsert_marker_block_creates_new_file() {
        let tmp = std::env::temp_dir().join(format!("sigil_install_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let path = tmp.join("sub/CLAUDE.md");
        let r = upsert_marker_block(&path, "hello sigil").unwrap();
        assert_eq!(r, UpsertResult::Created);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("hello sigil"));
        assert!(content.starts_with(MARKER_BEGIN));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn upsert_marker_block_upgrade_in_place() {
        let tmp = std::env::temp_dir().join(format!("sigil_install_up_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("CLAUDE.md");
        std::fs::write(&path, "# CLAUDE.md\n\n<!-- sigil:begin -->\nv1\n<!-- sigil:end -->\n").unwrap();
        let r = upsert_marker_block(&path, "v2").unwrap();
        assert_eq!(r, UpsertResult::Updated);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("v2"));
        assert!(!content.contains("v1"));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn upsert_marker_block_is_noop_when_content_matches() {
        let tmp = std::env::temp_dir().join(format!("sigil_install_noop_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("CLAUDE.md");
        upsert_marker_block(&path, "same").unwrap();
        let r = upsert_marker_block(&path, "same").unwrap();
        assert_eq!(r, UpsertResult::Unchanged);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn remove_marker_block_deletes_empty_file_after_removal() {
        let tmp = std::env::temp_dir().join(format!("sigil_install_rm_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("CLAUDE.md");
        upsert_marker_block(&path, "only block").unwrap();
        let removed = remove_marker_block(&path).unwrap();
        assert!(removed);
        assert!(!path.exists(), "empty file should be deleted, not left as blank");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn remove_marker_block_preserves_user_content() {
        let tmp = std::env::temp_dir().join(format!("sigil_install_pres_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("CLAUDE.md");
        std::fs::write(
            &path,
            "# CLAUDE.md\n\nuser text.\n\n<!-- sigil:begin -->\nsigil\n<!-- sigil:end -->\n\ntail\n",
        )
        .unwrap();
        let removed = remove_marker_block(&path).unwrap();
        assert!(removed);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("# CLAUDE.md"));
        assert!(content.contains("user text."));
        assert!(content.contains("tail"));
        assert!(!content.contains("<!-- sigil:begin -->"));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn remove_marker_block_missing_file_returns_false() {
        let tmp = std::env::temp_dir().join(format!("sigil_install_miss_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let removed = remove_marker_block(&tmp.join("nothing.md")).unwrap();
        assert!(!removed);
    }
}
