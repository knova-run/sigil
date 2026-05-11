//! Tier-3 resolution: global-unique fallback + JS/TS + Python barrel one-hop.
//!
//! The CallResolver pattern is documented in the repowise call_resolver.py
//! port — three tiers checked in order:
//!
//!   * Tier 1 (1.0) — same-file bare-identifier call (per-parser, shipped)
//!   * Tier 2 (0.8) — file-local import-alias resolution (per-parser, shipped)
//!   * Tier 3 (0.5) — global-unique name match across the index, same-language only
//!   * Tier-3 barrel (0.7) — one-hop re-export follow for JS/TS + Python barrels
//!
//! These tests drive the `sigil index` orchestration layer in `src/index.rs`,
//! not any single parser — tier-3 is necessarily cross-file work.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn fresh_dir(tag: &str) -> PathBuf {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("sigil-tier3-{tag}-{pid}-{id}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn write(dir: &PathBuf, rel: &str, contents: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    std::fs::write(&path, contents).expect("write fixture");
}

fn run_index_with_refs(dir: &PathBuf, extra_args: &[&str]) -> Vec<serde_json::Value> {
    let mut args: Vec<&str> = vec!["index", "--root", dir.to_str().unwrap(), "--stdout", "--full"];
    args.extend_from_slice(extra_args);
    let output = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args(&args)
        .output()
        .expect("failed to run sigil");
    assert!(
        output.status.success(),
        "sigil failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

#[test]
fn cross_file_python_call_with_unique_def_gets_tier3_confidence() {
    // Two Python files, no `import` linking them. `caller.py` calls
    // `helper()`; `defs.py` is the only file that defines `helper`.
    // Without tier-3 the call has confidence 1.0 (same-file tier-1 sees it
    // as a bare identifier even though no local definition matches it) —
    // but the proper outcome for a call that has no same-file def AND no
    // import binding AND a unique global match is tier-3 confidence 0.5.
    //
    // We assert on the outcome by name: the unique-target file is `defs.py`
    // and that's where the resolved target lives.
    let dir = fresh_dir("global-unique");
    write(&dir, "caller.py", "def driver():\n    helper()\n");
    write(&dir, "defs.py", "def helper():\n    pass\n");
    let refs = run_index_with_refs(&dir, &[]);

    let call = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["name"].as_str() == Some("helper")
                && r["file"].as_str() == Some("caller.py")
        })
        .expect("helper() call from caller.py should be in refs");
    assert_eq!(
        call["confidence"].as_f64(),
        Some(0.5),
        "cross-file global-unique call should resolve to tier-3 confidence 0.5; got {:?}",
        call["confidence"]
    );
}

#[test]
fn ambiguous_global_match_stays_unresolved() {
    // Two files BOTH define `helper`; caller.py has no import. The call
    // is ambiguous (which `helper` did the caller mean?) so tier-3 must
    // NOT bind it — confidence should be None, mirroring repowise's
    // CallResolver tier-3 logic (`len(candidates) == 1` guard).
    let dir = fresh_dir("ambiguous");
    write(&dir, "caller.py", "def driver():\n    helper()\n");
    write(&dir, "defs_a.py", "def helper():\n    pass\n");
    write(&dir, "defs_b.py", "def helper():\n    pass\n");
    let refs = run_index_with_refs(&dir, &[]);

    let call = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["name"].as_str() == Some("helper")
                && r["file"].as_str() == Some("caller.py")
        })
        .expect("helper() call from caller.py should be in refs");
    assert!(
        call["confidence"].is_null() || call.get("confidence").is_none(),
        "ambiguous global name should NOT resolve via tier-3; got {:?}",
        call["confidence"]
    );
}

#[test]
fn same_file_tier1_match_is_preserved() {
    // `caller` and `helper` are in the SAME file. Tier-1 must hold at 1.0,
    // even though tier-3 also runs over the same index. There's also a
    // tempting second-file def of `helper` to ensure the resolver doesn't
    // get confused — same-file match wins (and shouldn't be demoted +
    // re-promoted to 0.5).
    let dir = fresh_dir("same-file");
    write(
        &dir,
        "caller.py",
        "def driver():\n    helper()\n\ndef helper():\n    pass\n",
    );
    write(&dir, "other.py", "def helper():\n    pass\n");
    let refs = run_index_with_refs(&dir, &[]);

    let call = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["name"].as_str() == Some("helper")
                && r["file"].as_str() == Some("caller.py")
        })
        .expect("helper() call from caller.py should be in refs");
    assert_eq!(
        call["confidence"].as_f64(),
        Some(1.0),
        "same-file tier-1 must remain 1.0; got {:?}",
        call["confidence"]
    );
}

#[test]
fn cross_language_global_unique_is_rejected() {
    // Python caller, Rust-only definition. Even though `register` is
    // globally unique by name, the tier-3 resolver must refuse to bind
    // across languages — a Python `register()` will never resolve to a
    // Rust `fn register`. Matches repowise's caller_lang == callee_lang
    // guard in call_resolver.py.
    let dir = fresh_dir("cross-lang");
    write(&dir, "caller.py", "def driver():\n    register()\n");
    write(&dir, "defs.rs", "pub fn register() {}\n");
    let refs = run_index_with_refs(&dir, &[]);

    let call = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["name"].as_str() == Some("register")
                && r["file"].as_str() == Some("caller.py")
        })
        .expect("register() call from caller.py should be in refs");
    assert!(
        call["confidence"].is_null() || call.get("confidence").is_none(),
        "cross-language unique match must NOT resolve; got {:?}",
        call["confidence"]
    );
}

#[test]
fn ts_barrel_reexport_emits_resolved_edge_at_confidence_seven() {
    // Barrel pattern:
    //   caller.ts:   import { helper } from "./utils";  helper();
    //   utils.ts:    export { helper } from "./internal/h";
    //   internal/h.ts: export function helper() {}
    //
    // Tier-2 already emits the raw `helper` + resolved `./utils.helper/`
    // edges at 0.8. Tier-3 barrel-follow should additionally emit an edge
    // at 0.7 pointing at the underlying definition file (`./internal/h.ts`).
    //
    // Repowise follows barrels one hop inside tier-2 (call_resolver.py:208).
    // Sigil emits this as an additional edge (rather than rewriting in
    // place) to preserve the original tier-2 edge for consumers that don't
    // want barrel-hop interpretation.
    let dir = fresh_dir("ts-barrel");
    write(
        &dir,
        "caller.ts",
        "import { helper } from \"./utils\";\nfunction driver() { helper(); }\n",
    );
    write(
        &dir,
        "utils.ts",
        "export { helper } from \"./internal/h\";\n",
    );
    write(
        &dir,
        "internal/h.ts",
        "export function helper() {}\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let resolved = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("caller.ts")
                && r["name"]
                    .as_str()
                    .map(|s| s.contains("internal/h"))
                    .unwrap_or(false)
        })
        .unwrap_or_else(|| {
            let all_refs: Vec<_> = refs.iter().map(|r| (
                r["file"].as_str(),
                r["name"].as_str(),
                r["kind"].as_str(),
                r["confidence"].as_f64(),
            )).collect();
            panic!(
                "expected tier-3 barrel-resolved edge pointing at internal/h.\nALL REFS:\n{all_refs:#?}"
            )
        });
    assert_eq!(
        resolved["confidence"].as_f64(),
        Some(0.7),
        "barrel-follow edge should carry confidence 0.7; got {:?}",
        resolved["confidence"]
    );
}

#[test]
fn python_init_barrel_emits_resolved_edge_at_confidence_seven() {
    // Python __init__.py barrel pattern:
    //   caller.py:           from utils import helper
    //                        helper()
    //   utils/__init__.py:   from .internal import helper
    //   utils/internal.py:   def helper(): ...
    //
    // Tier-2's `pkg.member` form for `from pkg import x` produces edge
    // name `utils.helper/`. Tier-3 barrel-follow should add a 0.7 edge
    // pointing at the underlying definition file (`utils/internal.py`).
    let dir = fresh_dir("py-barrel");
    write(&dir, "caller.py", "from utils import helper\n\ndef driver():\n    helper()\n");
    write(&dir, "utils/__init__.py", "from .internal import helper\n");
    write(&dir, "utils/internal.py", "def helper():\n    pass\n");
    let refs = run_index_with_refs(&dir, &[]);

    let resolved = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("caller.py")
                && r["name"]
                    .as_str()
                    .map(|s| s.contains("utils/internal"))
                    .unwrap_or(false)
        })
        .unwrap_or_else(|| {
            let all_refs: Vec<_> = refs.iter().map(|r| (
                r["file"].as_str(),
                r["name"].as_str(),
                r["kind"].as_str(),
                r["confidence"].as_f64(),
            )).collect();
            panic!(
                "expected tier-3 barrel-resolved edge pointing at utils/internal.\nALL REFS:\n{all_refs:#?}"
            )
        });
    assert_eq!(
        resolved["confidence"].as_f64(),
        Some(0.7),
        "Python barrel-follow edge should carry confidence 0.7; got {:?}",
        resolved["confidence"]
    );
}

#[test]
fn no_tier3_flag_skips_both_passes() {
    // Same setup as the tracer (cross-file global-unique). With --no-tier3
    // the global-unique upgrade must not fire — the call stays at its
    // false-tier-1 1.0 (parser-emitted, unverified) because the demotion
    // pass also lives in resolve_tier3.
    //
    // The barrel-follow pass is also skipped: the TS barrel fixture would
    // not produce an `internal/h` edge under --no-tier3.
    let dir = fresh_dir("no-tier3");
    write(&dir, "caller.py", "def driver():\n    helper()\n");
    write(&dir, "defs.py", "def helper():\n    pass\n");
    let refs = run_index_with_refs(&dir, &["--no-tier3"]);

    let call = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["name"].as_str() == Some("helper")
                && r["file"].as_str() == Some("caller.py")
        })
        .expect("helper() call should exist");
    assert_ne!(
        call["confidence"].as_f64(),
        Some(0.5),
        "--no-tier3 must skip the global-unique upgrade; got {:?}",
        call["confidence"]
    );
}
