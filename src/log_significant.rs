//! `sigil log --significant <file>` — git history filtered to commits
//! likely to carry intent.
//!
//! Git history is dominated by noise: bump-version commits, lint passes,
//! formatter sweeps, dependency upgrades. None of those help an agent
//! understand *why* a file looks the way it does. This filter keeps the
//! ones that do:
//!
//! - Drop merges (`--no-merges`).
//! - Drop subjects that match `^(chore|deps|fmt|lint|whitespace|Bump |dependabot|renovate)`
//!   (case-insensitive on the leading token).
//! - Drop subjects shorter than 30 characters — those rarely carry enough
//!   context to be worth surfacing.
//!
//! Output is JSON per commit so a downstream agent can stitch them into a
//! narrative (`sigil context <file>`-style) without re-parsing git output.

use anyhow::Result;
use serde::Serialize;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Serialize, PartialEq)]
pub struct SignificantCommit {
    pub sha: String,
    pub date: String,
    pub author: String,
    pub subject: String,
    pub body: String,
    pub paths: Vec<String>,
}

/// Field separator inside a commit record. Picked to be unlikely to
/// appear in commit metadata — git's own `%x1f` (unit-separator).
const FIELD_SEP: &str = "\x1f";
/// Record separator between commits. ASCII record-separator `%x1e`.
const RECORD_SEP: &str = "\x1e";

/// Walk `git log` for `file` in `repo`, filter to "significant" commits,
/// and return up to `limit` rows (most-recent first — same order as
/// `git log`). `limit == 0` means unlimited, matching the convention
/// other sigil commands use for cap arguments (`--max-results 0`,
/// `--max 0`, etc.).
pub fn mine(repo: &Path, file: &str, limit: usize) -> Result<Vec<SignificantCommit>> {
    // %H sha · %aI author-date (ISO 8601 strict) · %ae author email ·
    // %s subject · %b body. RECORD_SEP at the start of the format string
    // lets us split records first, then fields, without ambiguity.
    let format = format!(
        "{rs}%H{fs}%aI{fs}%ae{fs}%s{fs}%b",
        rs = RECORD_SEP,
        fs = FIELD_SEP
    );
    let output = Command::new("git")
        .args([
            "log",
            "--no-merges",
            "--follow",
            &format!("--pretty=format:{}", format),
            "--name-only",
            "--",
            file,
        ])
        .current_dir(repo)
        .output()?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    let mut out = Vec::new();
    for record in text.split(RECORD_SEP) {
        if record.trim().is_empty() {
            continue;
        }
        let mut parts = record.splitn(5, FIELD_SEP);
        let sha = parts.next().unwrap_or("").to_string();
        let date = parts.next().unwrap_or("").to_string();
        let author = parts.next().unwrap_or("").to_string();
        let subject = parts.next().unwrap_or("").to_string();
        let tail = parts.next().unwrap_or("");
        // Tail is `<body>\n<path>\n<path>...\n`. Body terminates at the
        // first blank line followed by file paths — git emits the body,
        // then a blank line, then `--name-only` output. We split at the
        // last blank line so a multi-paragraph body stays intact.
        let (body, paths) = split_body_and_paths(tail);

        if !is_significant(&subject) {
            continue;
        }
        out.push(SignificantCommit {
            sha,
            date,
            author,
            subject,
            body,
            paths,
        });
        if limit > 0 && out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

/// Return true if the commit subject looks intent-bearing.
///
/// Filters out merges (already excluded by `--no-merges`), version bumps,
/// dependency updates, lint/format/whitespace sweeps, and short subjects
/// that rarely carry useful context.
pub fn is_significant(subject: &str) -> bool {
    let trimmed = subject.trim();
    if trimmed.chars().count() < 30 {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    // Conventional-commit-style prefixes: `chore:`, `chore(scope):`, etc.
    // `deps` only fires in its conventional-commit shapes (`deps:`,
    // `deps(...)`, `deps `) so legitimate-content subjects starting with
    // the same four letters (e.g. "deps build pipeline rewrite") aren't
    // swallowed.
    const NOISE_PREFIXES: &[&str] = &[
        "chore", "fmt", "lint", "whitespace", "bump ", "dependabot", "renovate",
        "deps:", "deps(", "deps ",
    ];
    for prefix in NOISE_PREFIXES {
        if lower.starts_with(prefix) {
            return false;
        }
    }
    true
}

/// Split the post-subject tail into (body, paths). `git log --name-only`
/// emits the body followed by a blank line followed by one path per line.
/// If there's no blank line, treat the whole tail as paths (empty body).
fn split_body_and_paths(tail: &str) -> (String, Vec<String>) {
    // Strip trailing whitespace lines first — git emits a trailing blank
    // before the next record separator, and we don't want that to look
    // like the body/paths boundary.
    let trimmed = tail.trim_end_matches(|c: char| c == '\n' || c == '\r' || c == ' ');
    let lines: Vec<&str> = trimmed.lines().collect();
    // The last blank line inside the trimmed text is the body/paths
    // boundary. Walking from the end keeps a multi-paragraph body intact.
    let boundary = lines.iter().rposition(|l| l.trim().is_empty());
    let (body_lines, path_lines): (&[&str], &[&str]) = match boundary {
        Some(idx) => (&lines[..idx], &lines[idx + 1..]),
        None => (&[], &lines[..]),
    };
    let body = body_lines.join("\n").trim().to_string();
    let paths: Vec<String> = path_lines
        .iter()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    (body, paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_short_subjects() {
        assert!(!is_significant("fix bug"));
        assert!(!is_significant("update"));
    }

    #[test]
    fn rejects_noise_prefixes() {
        assert!(!is_significant("chore: bump version to 0.5.0 release"));
        assert!(!is_significant("CHORE(deps): upgrade to latest serde"));
        assert!(!is_significant("Bump version: 0.4.1 -> 0.4.2 release"));
        assert!(!is_significant("deps: bump tokio from 1.30 to 1.31"));
        assert!(!is_significant("lint: apply rustfmt across the workspace"));
        assert!(!is_significant(
            "dependabot[bot] bump async-trait from 0.1 to 0.2"
        ));
    }

    #[test]
    fn accepts_intentful_subjects() {
        assert!(is_significant(
            "feat: extract per-file ownership from git log"
        ));
        assert!(is_significant(
            "fix race condition in cochange manifest writer"
        ));
        assert!(is_significant(
            "refactor decisions extractor to use byte-safe slicing"
        ));
    }

    #[test]
    fn deps_noise_prefix_is_anchored_to_conventional_commit_shapes() {
        // `deps` only fires in its conventional-commit shapes — `deps:`,
        // `deps(scope)`, or `deps ` followed by content. The bare token
        // (e.g. hypothetical "depsy: ...") is no longer auto-filtered
        // because the prefix is explicit, not bare. Real dependency-bump
        // subjects all use the conventional shapes and stay filtered.
        assert!(!is_significant("deps: bump tokio from 1.30 to 1.31"));
        assert!(!is_significant("deps(rust): upgrade to latest serde release"));
        assert!(!is_significant("deps bump async-trait from 0.1 to 0.2"));
        assert!(is_significant(
            "depsy: a long descriptive subject for a hypothetical not-deps tool"
        ));
    }

    #[test]
    fn body_paths_split_with_blank_line_separator() {
        let tail = "Body paragraph one.\n\nBody paragraph two.\n\nsrc/foo.rs\nsrc/bar.rs";
        let (body, paths) = split_body_and_paths(tail);
        assert_eq!(body, "Body paragraph one.\n\nBody paragraph two.");
        assert_eq!(paths, vec!["src/foo.rs", "src/bar.rs"]);
    }

    #[test]
    fn body_paths_split_when_no_body() {
        let tail = "\nsrc/foo.rs\nsrc/bar.rs";
        let (body, paths) = split_body_and_paths(tail);
        assert_eq!(body, "");
        assert_eq!(paths, vec!["src/foo.rs", "src/bar.rs"]);
    }
}
