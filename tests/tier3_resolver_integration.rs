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
    // `--stdout` writes entities to stdout and refs to stderr. Tests look
    // at both, so concatenate the two streams and parse JSON lines from
    // the union. (Refs are line-oriented and self-contained, so order
    // between the two streams doesn't matter.)
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    stdout
        .lines()
        .chain(stderr.lines())
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
        Some(0.95),
        "same-file tier-1 must remain 0.95; got {:?}",
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

// --- P0.1 — self/this member-call resolution -------------------------------
//
// Repowise's `_resolve_member_call` Strategy 3 (call_resolver.py:289-302):
// when the call's receiver is `self` (Python/Ruby) or `this`
// (Java/Kotlin/JS/TS/C#/Swift), the binding is *unambiguously* the caller's
// own class. Look up the method on that class — confidence 0.95.
//
// Currently sigil leaves these refs unresolved: tier-3 skips any ref whose
// `name` contains `.`, so `self.b()` is invisible to global-unique
// resolution. This is the highest-impact gap — likely doubles resolved-edge
// count for OO codebases.

#[test]
fn python_self_call_resolves_to_sibling_method_at_confidence_ninety_five() {
    // class Foo { def a(self): self.b() }
    // The `self.b()` call must resolve to `Foo.b` in the same file, at
    // confidence 0.95.
    let dir = fresh_dir("py-self");
    write(
        &dir,
        "foo.py",
        "class Foo:\n    def a(self):\n        self.b()\n    def b(self):\n        pass\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let call = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("foo.py")
                && r["name"].as_str() == Some("self.b")
        })
        .unwrap_or_else(|| {
            let all_refs: Vec<_> = refs.iter().map(|r| (
                r["file"].as_str(),
                r["name"].as_str(),
                r["caller"].as_str(),
                r["kind"].as_str(),
                r["confidence"].as_f64(),
            )).collect();
            panic!("expected self.b() call in refs.\nALL REFS:\n{all_refs:#?}")
        });
    assert_eq!(
        call["confidence"].as_f64(),
        Some(0.95),
        "self.b() should resolve via member-call Strategy 3 at 0.95; got {:?}",
        call["confidence"]
    );
}

#[test]
fn java_this_call_resolves_to_sibling_method_at_confidence_ninety_five() {
    // class Foo { void a() { this.b(); } void b() {} }
    // Same resolver must handle Java's `this.X()` receiver. Confidence 0.95.
    let dir = fresh_dir("java-this");
    write(
        &dir,
        "Foo.java",
        "class Foo {\n    void a() {\n        this.b();\n    }\n    void b() {}\n}\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let call = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("Foo.java")
                && r["name"].as_str() == Some("this.b")
        })
        .unwrap_or_else(|| {
            let all_refs: Vec<_> = refs.iter().map(|r| (
                r["file"].as_str(),
                r["name"].as_str(),
                r["caller"].as_str(),
                r["kind"].as_str(),
                r["confidence"].as_f64(),
            )).collect();
            panic!("expected this.b() call in refs.\nALL REFS:\n{all_refs:#?}")
        });
    assert_eq!(
        call["confidence"].as_f64(),
        Some(0.95),
        "this.b() should resolve via member-call Strategy 3 at 0.95; got {:?}",
        call["confidence"]
    );
}

#[test]
fn self_call_does_not_bind_to_other_class_in_same_file() {
    // class Foo { def a(self): self.b() }   # NO `b` method on Foo
    // class Bar { def b(self): pass }        # `b` exists, but on Bar
    //
    // Tempting wrong behavior: the resolver finds *some* method named `b`
    // in this file and binds. Correct behavior: refuse — `self.b()` from
    // inside Foo's method can only mean Foo.b, which doesn't exist.
    let dir = fresh_dir("py-self-wrong-class");
    write(
        &dir,
        "foo.py",
        "class Foo:\n    def a(self):\n        self.b()\n\nclass Bar:\n    def b(self):\n        pass\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let call = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("foo.py")
                && r["name"].as_str() == Some("self.b")
        })
        .expect("self.b() call should be in refs");
    assert!(
        call["confidence"].is_null() || call.get("confidence").is_none(),
        "self.b() from Foo must NOT bind to Bar.b; got {:?}",
        call["confidence"]
    );
}

#[test]
fn no_tier3_flag_also_skips_self_this_pass() {
    // --no-tier3 must skip the member-call upgrade. Same Python fixture
    // as the tracer; with the flag, self.b() must stay unresolved (not 0.95).
    let dir = fresh_dir("no-tier3-self");
    write(
        &dir,
        "foo.py",
        "class Foo:\n    def a(self):\n        self.b()\n    def b(self):\n        pass\n",
    );
    let refs = run_index_with_refs(&dir, &["--no-tier3"]);

    let call = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("foo.py")
                && r["name"].as_str() == Some("self.b")
        })
        .expect("self.b() call should be in refs");
    assert_ne!(
        call["confidence"].as_f64(),
        Some(0.95),
        "--no-tier3 must skip the self/this upgrade; got {:?}",
        call["confidence"]
    );
}

// --- P0.2 — known-class receiver (Strategy 2) ------------------------------
//
// Repowise call_resolver.py:273-288. When the call is `ClassName.method()`
// and `ClassName` is a known class in the index, the binding is
// unambiguous: it's that class's method. Same-file → 0.93; class imported
// into caller's file via a known import → 0.88.

#[test]
fn python_same_file_class_method_call_resolves_at_confidence_ninety_three() {
    // class Foo: @staticmethod def create(): pass
    // def driver(): Foo.create()
    // Same file, no import. Confidence 0.93.
    let dir = fresh_dir("py-class-same-file");
    write(
        &dir,
        "foo.py",
        "class Foo:\n    @staticmethod\n    def create():\n        pass\n\ndef driver():\n    Foo.create()\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let call = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("foo.py")
                && r["name"].as_str() == Some("Foo.create")
        })
        .unwrap_or_else(|| {
            let all_refs: Vec<_> = refs.iter().map(|r| (
                r["file"].as_str(),
                r["name"].as_str(),
                r["caller"].as_str(),
                r["kind"].as_str(),
                r["confidence"].as_f64(),
            )).collect();
            panic!("expected Foo.create call.\nALL REFS:\n{all_refs:#?}")
        });
    assert_eq!(
        call["confidence"].as_f64(),
        Some(0.93),
        "same-file known-class receiver should resolve at 0.93; got {:?}",
        call["confidence"]
    );
}

#[test]
fn python_imported_class_method_call_resolves_at_confidence_eighty_eight() {
    // foo.py:    class Foo: @staticmethod def create(): pass
    // caller.py: from foo import Foo
    //            def driver(): Foo.create()
    //
    // Tier-2 already emits Foo.create at 0.8 (the import alias is present).
    // The receiver-aware Strategy 2 upgrades to 0.88 because `Foo` resolves
    // to a *unique* class entity with a `create` method in the index.
    let dir = fresh_dir("py-imported-class");
    write(
        &dir,
        "foo.py",
        "class Foo:\n    @staticmethod\n    def create():\n        pass\n",
    );
    write(
        &dir,
        "caller.py",
        "from foo import Foo\n\ndef driver():\n    Foo.create()\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let call = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("caller.py")
                && r["name"].as_str() == Some("Foo.create")
        })
        .unwrap_or_else(|| {
            let all_refs: Vec<_> = refs.iter().map(|r| (
                r["file"].as_str(),
                r["name"].as_str(),
                r["caller"].as_str(),
                r["kind"].as_str(),
                r["confidence"].as_f64(),
            )).collect();
            panic!("expected Foo.create call from caller.py.\nALL REFS:\n{all_refs:#?}")
        });
    assert_eq!(
        call["confidence"].as_f64(),
        Some(0.88),
        "imported known-class receiver should resolve at 0.88; got {:?}",
        call["confidence"]
    );
}

#[test]
fn imported_class_resolution_does_not_cross_language() {
    // Python caller, Rust-only class with same name. The receiver-aware
    // resolver must NOT promote — `Foo.create()` in Python can't be
    // bound to a Rust class.
    let dir = fresh_dir("xlang-class");
    write(
        &dir,
        "caller.py",
        "def driver():\n    Foo.create()\n",
    );
    write(
        &dir,
        "defs.rs",
        "struct Foo;\nimpl Foo {\n    pub fn create() {}\n}\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let call = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("caller.py")
                && r["name"].as_str() == Some("Foo.create")
        })
        .expect("Foo.create() call should be in refs");
    assert_ne!(
        call["confidence"].as_f64(),
        Some(0.88),
        "Strategy 2 must not cross language boundaries; got {:?}",
        call["confidence"]
    );
}

// --- P0.3 — tier-2b fallback (imported-file scan) --------------------------
//
// Repowise call_resolver.py:228-234. When a bare-name call has no same-file
// def, no import-alias binding, and the global-unique check fails (>1
// match), scan the caller's imports — if exactly one imported file defines
// that name as callable, bind at 0.85.
//
// Closes the gap for `from utils import *` style and other unbound-name
// resolutions where the global ambiguity is broken by the caller's imports.

#[test]
fn python_star_import_resolves_bare_call_to_imported_file_at_confidence_eighty_five() {
    // caller.py:   from utils import *
    //              def driver(): helper()
    // utils.py:    def helper(): pass
    // other.py:    def helper(): pass   # second def → global-unique fails
    //
    // Tier-3 global-unique sees 2 global matches → unresolved.
    // Tier-2b sees `from utils import *` in caller; `utils.py` has
    // `helper` → bind at 0.85.
    let dir = fresh_dir("py-star-import");
    write(
        &dir,
        "utils.py",
        "def helper():\n    pass\n",
    );
    write(
        &dir,
        "other.py",
        "def helper():\n    pass\n",
    );
    write(
        &dir,
        "caller.py",
        "from utils import *\n\ndef driver():\n    helper()\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let call = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("caller.py")
                && r["name"].as_str() == Some("helper")
        })
        .unwrap_or_else(|| {
            let all_refs: Vec<_> = refs.iter().map(|r| (
                r["file"].as_str(),
                r["name"].as_str(),
                r["kind"].as_str(),
                r["confidence"].as_f64(),
            )).collect();
            panic!("expected helper() call from caller.py.\nALL REFS:\n{all_refs:#?}")
        });
    assert_eq!(
        call["confidence"].as_f64(),
        Some(0.85),
        "tier-2b star-import scan should resolve at 0.85; got {:?}",
        call["confidence"]
    );
}

#[test]
fn tier2b_does_not_bind_when_multiple_imports_define_same_name() {
    // caller.py imports BOTH utils and helpers via star; both define
    // `do_thing`. Ambiguous — tier-2b must NOT bind.
    let dir = fresh_dir("tier2b-ambig");
    write(&dir, "utils.py", "def do_thing():\n    pass\n");
    write(&dir, "helpers.py", "def do_thing():\n    pass\n");
    write(
        &dir,
        "caller.py",
        "from utils import *\nfrom helpers import *\n\ndef driver():\n    do_thing()\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let call = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("caller.py")
                && r["name"].as_str() == Some("do_thing")
        })
        .expect("do_thing() call should be in refs");
    assert_ne!(
        call["confidence"].as_f64(),
        Some(0.85),
        "tier-2b must NOT bind when multiple imports define the name; got {:?}",
        call["confidence"]
    );
}

// --- P1.4 — TS tsconfig.json paths resolver --------------------------------
//
// Real TS projects use path mappings (`"@/*": ["src/*"]`). Without
// reading tsconfig, sigil's import resolver returns None for aliased
// imports — barrel-follow can't find the target file.
//
// Repowise resolves tsconfig paths in resolvers/typescript.py; sigil
// extends `resolve_module_path` (src/index.rs) to apply longest-prefix
// path-mapping before falling through to relative probing.

#[test]
fn tsconfig_paths_alias_resolves_through_barrel_at_confidence_seven() {
    // caller.ts:           import { helper } from "@/utils"; helper();
    // tsconfig.json:       paths: { "@/*": ["src/*"] }
    // src/utils.ts:        export { helper } from "./internal/h";   (barrel)
    // src/internal/h.ts:   export function helper() {}
    //
    // Without tsconfig support `@/utils` can't be mapped to a file —
    // barrel-follow stays silent. With tsconfig: `@/utils` → src/utils,
    // probe → src/utils.ts (the barrel), follow re-export → emit a 0.7
    // edge pointing at src/internal/h.
    let dir = fresh_dir("tsconfig-paths");
    write(
        &dir,
        "tsconfig.json",
        "{\n  \"compilerOptions\": {\n    \"baseUrl\": \".\",\n    \"paths\": { \"@/*\": [\"src/*\"] }\n  }\n}\n",
    );
    write(
        &dir,
        "src/utils.ts",
        "export { helper } from \"./internal/h\";\n",
    );
    write(
        &dir,
        "src/internal/h.ts",
        "export function helper() {}\n",
    );
    write(
        &dir,
        "caller.ts",
        "import { helper } from \"@/utils\";\nfunction driver() { helper(); }\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let resolved = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("caller.ts")
                && r["name"]
                    .as_str()
                    .map(|s| s.contains("src/internal/h"))
                    .unwrap_or(false)
        })
        .unwrap_or_else(|| {
            let all_refs: Vec<_> = refs.iter().map(|r| (
                r["file"].as_str(),
                r["name"].as_str(),
                r["kind"].as_str(),
                r["confidence"].as_f64(),
            )).collect();
            panic!("expected barrel-resolved edge via tsconfig paths.\nALL REFS:\n{all_refs:#?}")
        });
    assert_eq!(
        resolved["confidence"].as_f64(),
        Some(0.7),
        "tsconfig+barrel-follow edge should be at 0.7; got {:?}",
        resolved["confidence"]
    );
}

#[test]
fn tsconfig_paths_longest_prefix_wins() {
    // Two aliases — `"@/*": ["src/*"]` and `"@/comp/*": ["src/components/*"]`.
    // The longer prefix must win for `@/comp/Button`.
    let dir = fresh_dir("tsconfig-longest");
    write(
        &dir,
        "tsconfig.json",
        "{\n  \"compilerOptions\": {\n    \"paths\": {\n      \"@/*\": [\"src/*\"],\n      \"@/comp/*\": [\"src/components/*\"]\n    }\n  }\n}\n",
    );
    write(
        &dir,
        "src/components/Button.ts",
        "export { useButton } from \"./internal/useButton\";\n",
    );
    write(
        &dir,
        "src/components/internal/useButton.ts",
        "export function useButton() {}\n",
    );
    write(
        &dir,
        "caller.ts",
        "import { useButton } from \"@/comp/Button\";\nfunction driver() { useButton(); }\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let resolved = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("caller.ts")
                && r["name"]
                    .as_str()
                    .map(|s| s.contains("src/components/internal/useButton"))
                    .unwrap_or(false)
        })
        .unwrap_or_else(|| {
            let all_refs: Vec<_> = refs.iter().map(|r| (
                r["file"].as_str(),
                r["name"].as_str(),
                r["confidence"].as_f64(),
            )).collect();
            panic!("expected longest-prefix tsconfig mapping to win.\nALL REFS:\n{all_refs:#?}")
        });
    assert_eq!(
        resolved["confidence"].as_f64(),
        Some(0.7),
        "longest-prefix tsconfig + barrel-follow should be at 0.7; got {:?}",
        resolved["confidence"]
    );
}

// --- P1.5 — Go go.mod multi-module resolver --------------------------------
//
// Go imports are canonical paths (`github.com/acme/foo/internal/utils`).
// Without reading `go.mod`, sigil's tier-2 emits the canonical path as the
// edge name but never locates the actual `.go` file that defines the
// called function. With go.mod awareness, the resolver maps the module
// prefix to the workspace root and emits an additional file-resolved
// edge at confidence 0.7 (barrel-follow analog for Go).

#[test]
fn go_module_path_resolves_to_actual_file_at_confidence_seven() {
    // go.mod:                       module github.com/acme/myproj
    // internal/utils/helper.go:     package utils; func Helper() {}
    // main.go:                      import ".../internal/utils"; utils.Helper()
    //
    // Expected: a new call edge pointing at internal/utils/helper.go at 0.7.
    let dir = fresh_dir("go-mod");
    write(
        &dir,
        "go.mod",
        "module github.com/acme/myproj\n\ngo 1.21\n",
    );
    write(
        &dir,
        "internal/utils/helper.go",
        "package utils\n\nfunc Helper() {}\n",
    );
    write(
        &dir,
        "main.go",
        "package main\n\nimport \"github.com/acme/myproj/internal/utils\"\n\nfunc main() {\n    utils.Helper()\n}\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let resolved = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("main.go")
                && r["name"]
                    .as_str()
                    .map(|s| s.contains("internal/utils/helper.go"))
                    .unwrap_or(false)
        })
        .unwrap_or_else(|| {
            let all_refs: Vec<_> = refs.iter().map(|r| (
                r["file"].as_str(),
                r["name"].as_str(),
                r["confidence"].as_f64(),
            )).collect();
            panic!("expected go.mod-resolved file edge.\nALL REFS:\n{all_refs:#?}")
        });
    assert_eq!(
        resolved["confidence"].as_f64(),
        Some(0.7),
        "go.mod-resolved file edge should be at 0.7; got {:?}",
        resolved["confidence"]
    );
}

#[test]
fn go_third_party_import_does_not_emit_phantom_file_edge() {
    // go.mod declares our module, but the caller imports a third-party
    // package (`github.com/external/lib`). The resolver must not invent
    // a file edge — only workspace-internal imports get the 0.7 promotion.
    let dir = fresh_dir("go-third-party");
    write(
        &dir,
        "go.mod",
        "module github.com/acme/myproj\n\ngo 1.21\n",
    );
    write(
        &dir,
        "main.go",
        "package main\n\nimport \"github.com/external/lib\"\n\nfunc main() {\n    lib.Doit()\n}\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    // No edge should mention a fabricated .go file path under external/lib.
    let bad = refs.iter().any(|r| {
        r["kind"].as_str() == Some("call")
            && r["confidence"].as_f64() == Some(0.7)
            && r["name"]
                .as_str()
                .map(|s| s.contains(".go") && s.contains("Doit"))
                .unwrap_or(false)
    });
    assert!(!bad, "third-party imports must not produce phantom .go file edges");
}

// --- P1.6 — PHP composer.json PSR-4 resolver -------------------------------
//
// PSR-4 maps namespace prefixes to directories (e.g. `App\\` → `src/`).
// Tier-2 emits PHP call edges with form `<canonical-namespace-path>/<rest>`
// at 0.8. With composer.json awareness, the resolver maps the namespace
// prefix to a filesystem directory, scans the directory for a callable
// matching the trailing leaf, and emits an additional 0.7 file-resolved
// edge.

#[test]
fn php_psr4_function_import_resolves_to_actual_file_at_confidence_seven() {
    // composer.json:               "autoload": { "psr-4": { "App\\": "src/" } }
    // src/Service/Mailer.php:      namespace App\Service; function send_email() {}
    // index.php:                   use function App\Service\send_email;
    //                              function driver() { send_email(); }
    //
    // Expected: a 0.7 edge pointing at src/Service/Mailer.php.
    let dir = fresh_dir("php-psr4");
    write(
        &dir,
        "composer.json",
        "{\n  \"autoload\": { \"psr-4\": { \"App\\\\\": \"src/\" } }\n}\n",
    );
    write(
        &dir,
        "src/Service/Mailer.php",
        "<?php\nnamespace App\\Service;\n\nfunction send_email() {}\n",
    );
    write(
        &dir,
        "index.php",
        "<?php\nuse function App\\Service\\send_email;\n\nfunction driver() {\n    send_email();\n}\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let resolved = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("index.php")
                && r["name"]
                    .as_str()
                    .map(|s| s.contains("src/Service/Mailer.php"))
                    .unwrap_or(false)
        })
        .unwrap_or_else(|| {
            let all_refs: Vec<_> = refs.iter().map(|r| (
                r["file"].as_str(),
                r["name"].as_str(),
                r["confidence"].as_f64(),
            )).collect();
            panic!("expected PSR-4-resolved file edge.\nALL REFS:\n{all_refs:#?}")
        });
    assert_eq!(
        resolved["confidence"].as_f64(),
        Some(0.7),
        "PSR-4-resolved file edge should be at 0.7; got {:?}",
        resolved["confidence"]
    );
}

// --- P2.9 — Rust Cargo workspace resolver ----------------------------------
//
// Cargo workspaces declare crate members at the root `Cargo.toml`. Rust
// source uses crate names with underscores (`myapp_core`) while the
// Cargo.toml `name` and directory often use hyphens (`myapp-core`).
// Without Cargo awareness, the cross-crate import `myapp_core::helper`
// can't be located in the workspace.

#[test]
fn rust_cargo_workspace_import_resolves_to_actual_file_at_confidence_seven() {
    // Root Cargo.toml:    [workspace] members = ["crates/*"]
    // crates/myapp-core/Cargo.toml:   name = "myapp-core"
    // crates/myapp-core/src/lib.rs:   pub fn helper() {}
    // crates/myapp-cli/src/main.rs:   use myapp_core::helper; fn main() { helper() }
    let dir = fresh_dir("cargo-workspace");
    write(
        &dir,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\"]\n",
    );
    write(
        &dir,
        "crates/myapp-core/Cargo.toml",
        "[package]\nname = \"myapp-core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(
        &dir,
        "crates/myapp-cli/Cargo.toml",
        "[package]\nname = \"myapp-cli\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(
        &dir,
        "crates/myapp-core/src/lib.rs",
        "pub fn helper() {}\n",
    );
    write(
        &dir,
        "crates/myapp-cli/src/main.rs",
        "use myapp_core::helper;\n\nfn main() {\n    helper();\n}\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let resolved = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("crates/myapp-cli/src/main.rs")
                && r["name"]
                    .as_str()
                    .map(|s| s.contains("crates/myapp-core/src/lib.rs"))
                    .unwrap_or(false)
        })
        .unwrap_or_else(|| {
            let all_refs: Vec<_> = refs.iter().map(|r| (
                r["file"].as_str(),
                r["name"].as_str(),
                r["confidence"].as_f64(),
            )).collect();
            panic!("expected Cargo-resolved file edge.\nALL REFS:\n{all_refs:#?}")
        });
    assert_eq!(
        resolved["confidence"].as_f64(),
        Some(0.7),
        "Cargo-resolved file edge should be at 0.7; got {:?}",
        resolved["confidence"]
    );
}

// --- P2.12 — Ruby Rails autoload resolver ----------------------------------
//
// Rails autoload convention: class `UserMailer` lives in `*_mailer.rb` —
// CamelCase class → snake_case filename. The Rails-conventional paths
// (app/**/*.rb, lib/**/*.rb) carry stronger evidence than a bare
// global-unique check, especially when multiple classes share a name.
//
// Strategy 2 (P0.2) already handles single-match cases at 0.88; this pass
// emits an additional 0.7 file-resolved edge so consumers can locate the
// actual `.rb` file. It also disambiguates when more than one class
// shares a name — only the Rails-conventional file path wins.

#[test]
fn rails_autoload_picks_app_conventional_file_at_confidence_seven() {
    // Two `UserMailer` classes — one in app/services (Rails canonical),
    // one in lib/legacy (also valid Ruby but not the autoload choice).
    // The caller's `UserMailer.deliver` should produce a 0.7 file edge
    // pointing at app/services/user_mailer.rb.
    let dir = fresh_dir("rails-autoload");
    write(
        &dir,
        "app/services/user_mailer.rb",
        "class UserMailer\n  def self.deliver\n  end\nend\n",
    );
    write(
        &dir,
        "lib/legacy/user_mailer.rb",
        "class UserMailer\n  def self.deliver\n  end\nend\n",
    );
    write(
        &dir,
        "app/controllers/users_controller.rb",
        "class UsersController\n  def create\n    UserMailer.deliver\n  end\nend\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let resolved = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("app/controllers/users_controller.rb")
                && r["name"]
                    .as_str()
                    .map(|s| s.contains("app/services/user_mailer.rb"))
                    .unwrap_or(false)
        })
        .unwrap_or_else(|| {
            let all_refs: Vec<_> = refs.iter().map(|r| (
                r["file"].as_str(),
                r["name"].as_str(),
                r["confidence"].as_f64(),
            )).collect();
            panic!("expected Rails-resolved file edge.\nALL REFS:\n{all_refs:#?}")
        });
    assert_eq!(
        resolved["confidence"].as_f64(),
        Some(0.7),
        "Rails-resolved file edge should be at 0.7; got {:?}",
        resolved["confidence"]
    );
}

// --- P2.10 — Swift Package.swift target resolver ---------------------------
//
// Swift Package Manager declares `targets` in `Package.swift`; each target's
// sources live under `Sources/<TargetName>/` by default (overridable via
// `path:`). The Swift import entity carries only the target name
// (`import MyCore`); without SPM awareness, a call to `helper()` after
// `import MyCore` can't be located when the same function name exists in
// multiple targets.
//
// Repowise: resolvers/swift_spm.py uses regex over Package.swift —
// sigil mirrors that approach (no Swift parser dependency).

#[test]
fn swift_spm_target_resolves_call_when_global_unique_ambiguous_at_confidence_seven() {
    // Sources/MyCore/Helper.swift:    public func helper() {}
    // Sources/OtherLib/Helper.swift:  public func helper() {}   (same name)
    // Sources/App/main.swift:         import MyCore; helper()
    //
    // Global-unique fails (2 helpers). With SPM the caller's
    // `import MyCore` disambiguates → 0.7 edge pointing at
    // Sources/MyCore/Helper.swift.
    let dir = fresh_dir("swift-spm");
    write(
        &dir,
        "Package.swift",
        "import PackageDescription\n\nlet package = Package(\n    name: \"MyLib\",\n    targets: [\n        .target(name: \"MyCore\"),\n        .target(name: \"OtherLib\"),\n        .executableTarget(name: \"App\", dependencies: [\"MyCore\"]),\n    ]\n)\n",
    );
    write(
        &dir,
        "Sources/MyCore/Helper.swift",
        "public func helper() {}\n",
    );
    write(
        &dir,
        "Sources/OtherLib/Helper.swift",
        "public func helper() {}\n",
    );
    write(
        &dir,
        "Sources/App/main.swift",
        "import MyCore\n\nhelper()\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let resolved = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("Sources/App/main.swift")
                && r["name"]
                    .as_str()
                    .map(|s| s.contains("Sources/MyCore/Helper.swift"))
                    .unwrap_or(false)
        })
        .unwrap_or_else(|| {
            let all_refs: Vec<_> = refs.iter().map(|r| (
                r["file"].as_str(),
                r["name"].as_str(),
                r["confidence"].as_f64(),
            )).collect();
            panic!("expected SPM-resolved file edge.\nALL REFS:\n{all_refs:#?}")
        });
    assert_eq!(
        resolved["confidence"].as_f64(),
        Some(0.7),
        "SPM-resolved file edge should be at 0.7; got {:?}",
        resolved["confidence"]
    );
}

// --- P2.7 — Kotlin Gradle / FQN-aware import resolver ----------------------
//
// Kotlin imports are fully-qualified (`import com.example.helper`). When
// the same simple name exists in multiple packages globally, tier-3
// global-unique fails. The caller's FQ import disambiguates: scan files
// whose path contains the package-as-directory form, find the one that
// defines the callable.
//
// Repowise: resolvers/kotlin_gradle.py also parses settings.gradle for
// multi-module layouts. Sigil's path-based approach covers the same
// cases without needing to read settings.gradle — Kotlin's standard
// `src/main/kotlin/<pkg>/` layout encodes the package in the path.

#[test]
fn kotlin_fqn_import_picks_right_package_at_confidence_seven() {
    // Two `helper()` funcs, different packages.
    // core/.../com/example/Helper.kt:   package com.example; fun helper() {}
    // other/.../com/other/Helper.kt:    package com.other;   fun helper() {}
    // app/.../com/example/Main.kt:      import com.example.helper; helper()
    //
    // Global-unique fails (2 helpers). With FQN-aware resolution,
    // `import com.example.helper` → the helper in core's com/example/ dir.
    // Expected: 0.7 edge pointing at core's Helper.kt.
    let dir = fresh_dir("kotlin-fqn");
    write(
        &dir,
        "core/src/main/kotlin/com/example/Helper.kt",
        "package com.example\n\nfun helper() {}\n",
    );
    write(
        &dir,
        "other/src/main/kotlin/com/other/Helper.kt",
        "package com.other\n\nfun helper() {}\n",
    );
    write(
        &dir,
        "app/src/main/kotlin/com/example/Main.kt",
        "package com.example\n\nimport com.example.helper\n\nfun main() {\n    helper()\n}\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let resolved = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("app/src/main/kotlin/com/example/Main.kt")
                && r["name"]
                    .as_str()
                    .map(|s| s.contains("core/src/main/kotlin/com/example/Helper.kt"))
                    .unwrap_or(false)
        })
        .unwrap_or_else(|| {
            let all_refs: Vec<_> = refs.iter().map(|r| (
                r["file"].as_str(),
                r["name"].as_str(),
                r["confidence"].as_f64(),
            )).collect();
            panic!("expected Kotlin FQN-resolved file edge.\nALL REFS:\n{all_refs:#?}")
        });
    assert_eq!(
        resolved["confidence"].as_f64(),
        Some(0.7),
        "Kotlin-FQN-resolved file edge should be at 0.7; got {:?}",
        resolved["confidence"]
    );
}

// --- P2.8 — Scala build-tool / FQN-aware import resolver -------------------
//
// Scala imports are fully-qualified (`import com.example.Helper.helper`).
// Same path-based disambiguation as Kotlin: scan `*/com/example/` for the
// callable. Repowise's `scala_build.py` parses sbt/Mill build files for
// non-standard layouts — the standard `src/main/scala/<pkg>/` layout is
// covered by path heuristics alone.

#[test]
fn scala_fqn_import_picks_right_package_at_confidence_seven() {
    // Two `helper()` methods, different packages — global-unique fails.
    let dir = fresh_dir("scala-fqn");
    write(
        &dir,
        "core/src/main/scala/com/example/Helper.scala",
        "package com.example\n\nobject Helper {\n  def helper(): Unit = {}\n}\n",
    );
    write(
        &dir,
        "other/src/main/scala/com/other/Helper.scala",
        "package com.other\n\nobject Helper {\n  def helper(): Unit = {}\n}\n",
    );
    write(
        &dir,
        "app/src/main/scala/com/example/Main.scala",
        "package com.example\n\nimport com.example.Helper.helper\n\nobject Main {\n  def main(args: Array[String]): Unit = {\n    helper()\n  }\n}\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let resolved = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("app/src/main/scala/com/example/Main.scala")
                && r["name"]
                    .as_str()
                    .map(|s| s.contains("core/src/main/scala/com/example/Helper.scala"))
                    .unwrap_or(false)
        })
        .unwrap_or_else(|| {
            let all_refs: Vec<_> = refs.iter().map(|r| (
                r["file"].as_str(),
                r["name"].as_str(),
                r["confidence"].as_f64(),
            )).collect();
            panic!("expected Scala FQN-resolved file edge.\nALL REFS:\n{all_refs:#?}")
        });
    assert_eq!(
        resolved["confidence"].as_f64(),
        Some(0.7),
        "Scala-FQN-resolved file edge should be at 0.7; got {:?}",
        resolved["confidence"]
    );
}

// --- P2.11 — C/C++ #include resolver (compile_commands.json) ---------------
//
// Repowise's resolver scope (cpp.py, 37 LOC) is narrowly focused: map
// `#include "foo.h"` → an actual filesystem path using compile_commands
// `-I/-isystem/-iquote` directories, fallback to importer-relative,
// fallback to stem match. Does NOT try to chase declaration → definition
// linkage — that ambiguity is fundamental to C++.

#[test]
fn cpp_include_resolves_via_compile_commands_at_confidence_seven() {
    // Two ambiguous helper.h files in include/a and include/b.
    // compile_commands says main.cpp uses `-Iinclude/a`, so the include
    // unambiguously resolves to include/a/helper.h. Expected: 0.7 edge.
    let dir = fresh_dir("cpp-include");
    write(
        &dir,
        "include/a/helper.h",
        "#pragma once\nvoid helper();\n",
    );
    write(
        &dir,
        "include/b/helper.h",
        "#pragma once\nvoid helper();\n",
    );
    write(
        &dir,
        "src/main.cpp",
        "#include \"helper.h\"\nint main() {\n    helper();\n    return 0;\n}\n",
    );
    write(
        &dir,
        "compile_commands.json",
        "[{\"file\": \"src/main.cpp\", \"directory\": \".\", \"arguments\": [\"c++\", \"-Iinclude/a\", \"-c\", \"src/main.cpp\"]}]\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let resolved = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("src/main.cpp")
                && r["name"]
                    .as_str()
                    .map(|s| s.contains("include/a/helper.h"))
                    .unwrap_or(false)
        })
        .unwrap_or_else(|| {
            let all_refs: Vec<_> = refs.iter().map(|r| (
                r["file"].as_str(),
                r["name"].as_str(),
                r["confidence"].as_f64(),
            )).collect();
            panic!("expected compile_commands-resolved edge.\nALL REFS:\n{all_refs:#?}")
        });
    assert_eq!(
        resolved["confidence"].as_f64(),
        Some(0.7),
        "compile_commands-resolved edge should be at 0.7; got {:?}",
        resolved["confidence"]
    );
}

// --- P3.13 — C# csproj / sln / GlobalUsings resolver -----------------------
//
// Ports repowise's `dotnet/` package (~620 LOC in repowise; focused
// equivalent here). Builds a namespace → file map from `.cs` files'
// `namespace` declarations (both block-form and C# 10+ file-scoped),
// then resolves `Class.Method` calls by intersecting the caller's
// `using` namespaces with the candidate files. Disambiguates same-named
// classes across namespaces — Strategy 2 alone can't because the global
// class-method index sees multiple matches.

#[test]
fn csharp_using_directive_disambiguates_class_call_at_confidence_seven() {
    // Two `Helper.Do` definitions in different namespaces.
    // App/Main.cs has `using ProjA.Foo;` — picks ProjA's Helper.
    let dir = fresh_dir("csharp-using");
    write(
        &dir,
        "ProjA/Foo/Helper.cs",
        "namespace ProjA.Foo;\npublic static class Helper {\n    public static void Do() {}\n}\n",
    );
    write(
        &dir,
        "ProjB/Foo/Helper.cs",
        "namespace ProjB.Foo;\npublic static class Helper {\n    public static void Do() {}\n}\n",
    );
    write(
        &dir,
        "App/Main.cs",
        "using ProjA.Foo;\nclass App {\n    static void Main() {\n        Helper.Do();\n    }\n}\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let resolved = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("App/Main.cs")
                && r["name"]
                    .as_str()
                    .map(|s| s.contains("ProjA/Foo/Helper.cs"))
                    .unwrap_or(false)
        })
        .unwrap_or_else(|| {
            let all_refs: Vec<_> = refs.iter().map(|r| (
                r["file"].as_str(),
                r["name"].as_str(),
                r["confidence"].as_f64(),
            )).collect();
            panic!("expected C# using-disambiguated edge.\nALL REFS:\n{all_refs:#?}")
        });
    assert_eq!(
        resolved["confidence"].as_f64(),
        Some(0.7),
        "C# using-disambiguated edge should be at 0.7; got {:?}",
        resolved["confidence"]
    );
}

#[test]
fn csharp_implicit_usings_resolves_without_explicit_using() {
    // SDK-style project with <ImplicitUsings>enable</ImplicitUsings>.
    // The default implicit-using set includes `System.Linq`, so calls
    // to types in System.Linq (or any default namespace) resolve via
    // the project's globals without a `using` directive in the file.
    //
    // Here we test: caller uses `Foo.Bar` from project's <Using/> item.
    let dir = fresh_dir("csharp-implicit");
    write(
        &dir,
        "App/App.csproj",
        "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <PropertyGroup>\n    <ImplicitUsings>enable</ImplicitUsings>\n  </PropertyGroup>\n  <ItemGroup>\n    <Using Include=\"Foo.Bar\" />\n  </ItemGroup>\n</Project>\n",
    );
    write(
        &dir,
        "Foo/Bar/Widget.cs",
        "namespace Foo.Bar;\npublic static class Widget {\n    public static void Render() {}\n}\n",
    );
    // App/Main.cs has NO `using Foo.Bar;` — it must come from the
    // project's <Using/> ItemGroup.
    write(
        &dir,
        "App/Main.cs",
        "class App {\n    static void Main() {\n        Widget.Render();\n    }\n}\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let resolved = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("App/Main.cs")
                && r["name"]
                    .as_str()
                    .map(|s| s.contains("Foo/Bar/Widget.cs"))
                    .unwrap_or(false)
        })
        .unwrap_or_else(|| {
            let all_refs: Vec<_> = refs.iter().map(|r| (
                r["file"].as_str(),
                r["name"].as_str(),
                r["confidence"].as_f64(),
            )).collect();
            panic!("expected resolution via project's <Using/> item.\nALL REFS:\n{all_refs:#?}")
        });
    assert_eq!(
        resolved["confidence"].as_f64(),
        Some(0.7),
        "C# <Using/>-resolved edge should be at 0.7; got {:?}",
        resolved["confidence"]
    );
}

#[test]
fn csharp_global_using_directive_disambiguates_call() {
    // C# 10's `global using` directive applies to every .cs file in the
    // project. Caller's own file has no `using Foo.Bar;` — it comes from
    // the project-level `global using` in a sibling file.
    let dir = fresh_dir("csharp-global");
    write(
        &dir,
        "App/App.csproj",
        "<Project Sdk=\"Microsoft.NET.Sdk\"></Project>\n",
    );
    write(
        &dir,
        "App/GlobalUsings.cs",
        "global using Foo.Bar;\n",
    );
    write(
        &dir,
        "Foo/Bar/Service.cs",
        "namespace Foo.Bar;\npublic static class Service {\n    public static void Run() {}\n}\n",
    );
    write(
        &dir,
        "App/Main.cs",
        "class App {\n    static void Main() {\n        Service.Run();\n    }\n}\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let resolved = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("App/Main.cs")
                && r["name"]
                    .as_str()
                    .map(|s| s.contains("Foo/Bar/Service.cs"))
                    .unwrap_or(false)
        })
        .unwrap_or_else(|| {
            let all_refs: Vec<_> = refs.iter().map(|r| (
                r["file"].as_str(),
                r["name"].as_str(),
                r["confidence"].as_f64(),
            )).collect();
            panic!("expected resolution via `global using` directive.\nALL REFS:\n{all_refs:#?}")
        });
    assert_eq!(
        resolved["confidence"].as_f64(),
        Some(0.7),
        "C# global-using resolved edge should be at 0.7; got {:?}",
        resolved["confidence"]
    );
}

#[test]
fn csharp_project_reference_ranks_same_or_referenced_first() {
    // Two `Foo.Bar.Service` classes — one in a referenced project,
    // one in an unrelated project. App.csproj references CoreLib.csproj.
    // App/Main.cs has `using Foo.Bar;`. The resolver should prefer the
    // referenced project's file over the unrelated one.
    let dir = fresh_dir("csharp-projref");
    write(
        &dir,
        "App/App.csproj",
        "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <ItemGroup>\n    <ProjectReference Include=\"..\\CoreLib\\CoreLib.csproj\" />\n  </ItemGroup>\n</Project>\n",
    );
    write(
        &dir,
        "CoreLib/CoreLib.csproj",
        "<Project Sdk=\"Microsoft.NET.Sdk\"></Project>\n",
    );
    write(
        &dir,
        "Unrelated/Unrelated.csproj",
        "<Project Sdk=\"Microsoft.NET.Sdk\"></Project>\n",
    );
    write(
        &dir,
        "CoreLib/Service.cs",
        "namespace Foo.Bar;\npublic static class Service {\n    public static void Run() {}\n}\n",
    );
    write(
        &dir,
        "Unrelated/Service.cs",
        "namespace Foo.Bar;\npublic static class Service {\n    public static void Run() {}\n}\n",
    );
    write(
        &dir,
        "App/Main.cs",
        "using Foo.Bar;\nclass App {\n    static void Main() {\n        Service.Run();\n    }\n}\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let resolved = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("App/Main.cs")
                && r["name"].as_str().map(|s| s.contains("Service.cs")).unwrap_or(false)
                && r["confidence"].as_f64() == Some(0.7)
        })
        .unwrap_or_else(|| {
            let all_refs: Vec<_> = refs.iter().map(|r| (
                r["file"].as_str(),
                r["name"].as_str(),
                r["confidence"].as_f64(),
            )).collect();
            panic!("expected ProjectReference-ranked edge.\nALL REFS:\n{all_refs:#?}")
        });
    let edge_path = resolved["name"].as_str().unwrap();
    assert!(
        edge_path.contains("CoreLib/Service.cs"),
        "ProjectReference should win over unrelated project; got {}",
        edge_path,
    );
}

// --- P5.16 — granular callee_id field on Reference ------------------------
//
// File-resolved edges from the manifest resolvers carry a stable
// `callee_id` of form `<file>::<symbol-path>`. Lets downstream consumers
// (heritage, blast, IDE jump-to-def) reach the target entity without
// re-doing name matching.

#[test]
fn self_this_member_call_carries_callee_id() {
    // resolve_member_call Strategy 3 must populate callee_id alongside
    // the 0.95 confidence binding. Format: `<file>::<class>::<method>`.
    let dir = fresh_dir("callee-id-self-this");
    write(
        &dir,
        "foo.py",
        "class Foo:\n    def a(self):\n        self.b()\n    def b(self):\n        pass\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let call = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("foo.py")
                && r["name"].as_str() == Some("self.b")
        })
        .expect("self.b() call should be in refs");
    assert_eq!(
        call["callee_id"].as_str(),
        Some("foo.py::Foo::b"),
        "self/this binding should carry callee_id `<file>::<class>::<method>`; got {:?}",
        call["callee_id"],
    );
}

#[test]
fn imported_class_strategy2_carries_callee_id() {
    // resolve_member_call Strategy 2 (imported branch, 0.88) — the
    // target file is the unique global match. callee_id should point at
    // that file, NOT the caller's file.
    let dir = fresh_dir("callee-id-strategy2-imported");
    write(
        &dir,
        "foo.py",
        "class Foo:\n    @staticmethod\n    def create():\n        pass\n",
    );
    write(
        &dir,
        "caller.py",
        "from foo import Foo\n\ndef driver():\n    Foo.create()\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let call = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("caller.py")
                && r["name"].as_str() == Some("Foo.create")
        })
        .expect("Foo.create call should be in refs");
    assert_eq!(
        call["callee_id"].as_str(),
        Some("foo.py::Foo::create"),
        "imported-class Strategy 2 should carry callee_id targeting the defining file; got {:?}",
        call["callee_id"],
    );
}

// --- Issue #27 — Member-call Strategy 1 (module-alias receiver) -----------
//
// Repowise's _resolve_member_call Strategy 1/1b (call_resolver.py:247-272):
// when the receiver is a module alias bound by an import in the caller's
// file, the binding is the function/method by that name in the alias's
// target file. Confidence 0.88 — symmetric with Strategy 2 imported.

#[test]
fn python_module_import_receiver_resolves_at_confidence_eighty_eight() {
    // `import utils` + `utils.run()` — receiver `utils` matches an import
    // entity whose name is "utils". Strategy 1 should resolve `run` to
    // utils.py at confidence 0.88.
    let dir = fresh_dir("strategy1-python-import");
    write(&dir, "utils.py", "def run():\n    pass\n");
    write(
        &dir,
        "caller.py",
        "import utils\n\ndef driver():\n    utils.run()\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let call = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("caller.py")
                && r["name"].as_str() == Some("utils.run")
        })
        .expect("utils.run() call should be in refs");
    assert_eq!(
        call["confidence"].as_f64(),
        Some(0.88),
        "module-alias receiver Strategy 1 should resolve at 0.88; got {:?}",
        call["confidence"]
    );
    assert_eq!(
        call["callee_id"].as_str(),
        Some("utils.py::run"),
        "Strategy 1 callee_id should point at the target file's function"
    );
}

#[test]
fn python_alias_import_receiver_resolves_at_confidence_eighty_eight() {
    // `import numpy as np` + `np.array()` — receiver `np` matches the
    // alias of an import whose target is `numpy`. Within-workspace
    // version: `import inner as alias` + `alias.helper()`.
    let dir = fresh_dir("strategy1-python-alias");
    write(&dir, "inner.py", "def helper():\n    pass\n");
    write(
        &dir,
        "caller.py",
        "import inner as alias\n\ndef driver():\n    alias.helper()\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let call = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("caller.py")
                && r["name"].as_str() == Some("alias.helper")
        })
        .expect("alias.helper() call should be in refs");
    assert_eq!(
        call["confidence"].as_f64(),
        Some(0.88),
        "aliased-module receiver Strategy 1 should resolve at 0.88; got {:?}",
        call["confidence"]
    );
    assert_eq!(
        call["callee_id"].as_str(),
        Some("inner.py::helper"),
        "Strategy 1 callee_id should point at the alias's target file"
    );
}

#[test]
fn tier2b_imported_fallback_carries_callee_id() {
    // resolve_tier2b_imported_fallback (0.85) — the binding is the
    // unique imported file. callee_id should be `<that_file>::<name>`.
    let dir = fresh_dir("callee-id-tier2b");
    write(&dir, "utils.py", "def helper():\n    pass\n");
    write(&dir, "other.py", "def helper():\n    pass\n");
    write(
        &dir,
        "caller.py",
        "from utils import *\n\ndef driver():\n    helper()\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let call = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("caller.py")
                && r["name"].as_str() == Some("helper")
                && r["confidence"].as_f64() == Some(0.85)
        })
        .expect("helper() call should be tier-2b promoted to 0.85");
    assert_eq!(
        call["callee_id"].as_str(),
        Some("utils.py::helper"),
        "tier-2b should carry callee_id `<resolved_file>::<name>`; got {:?}",
        call["callee_id"],
    );
}

#[test]
fn barrel_follow_still_fires_after_other_tier3_passes_run() {
    // Regression guard: tier-3 / tier-2b / member-call run before
    // barrel-follow on the same refs vec. None of those upstream passes
    // should mutate confidence on barrel-shape edges (the
    // `<import-path>.<local>/<rest>` form contains both `.` and `/`,
    // which their guards reject). If a future change relaxed those
    // guards, barrel-follow's `!= Some(0.8)` filter would silently
    // skip the promoted edges — this test fails fast in that case.
    let dir = fresh_dir("barrel-after-tier3");
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

    // Barrel-follow must still emit the 0.7 file-resolved edge.
    let barrel_edge = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("caller.ts")
                && r["name"]
                    .as_str()
                    .map(|s| s.contains("internal/h"))
                    .unwrap_or(false)
        })
        .expect("barrel-follow 0.7 edge must still be emitted after upstream tier-3 passes");
    assert_eq!(
        barrel_edge["confidence"].as_f64(),
        Some(0.7),
        "barrel-follow edge confidence regressed; got {:?}",
        barrel_edge["confidence"],
    );
}

// --- Doc attachment for Swift/Scala/PHP (PR #24 carryover from #15) -------
//
// These three parsers were shipped in issue #19 / PR #24 using
// `extract_comment(.., parent_ctx, ..)` which parents inner comments to
// the *enclosing* scope (the class). Result: method-level docs get
// pinned to the class, and the class's own leading doc (above the
// declaration) is lost. The fix is the same `pending_docs` buffer
// pattern used by Rust/Python/TS/JS — collect leading doc comments and
// attach them to the *following* item.

#[test]
fn swift_doc_comment_attaches_to_following_class_and_method() {
    let dir = fresh_dir("swift-doc-attach");
    write(
        &dir,
        "Greeter.swift",
        "/// Greets a user.\nclass Greeter {\n    /// Says hello.\n    func hello() {}\n}\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let class = refs
        .iter()
        .find(|e| e["name"].as_str() == Some("Greeter") && e["kind"].as_str() == Some("class"))
        .expect("Greeter class entity should be emitted");
    assert_eq!(
        class["doc"].as_str(),
        Some("Greets a user."),
        "Swift class doc should be its leading `///` comment; got {:?}",
        class["doc"],
    );

    let method = refs
        .iter()
        .find(|e| e["name"].as_str() == Some("Greeter.hello") && e["kind"].as_str() == Some("method"))
        .expect("Greeter.hello method entity should be emitted");
    assert_eq!(
        method["doc"].as_str(),
        Some("Says hello."),
        "Swift method doc should be its own leading `///`; got {:?}",
        method["doc"],
    );
}

#[test]
fn scala_doc_comment_attaches_to_following_class_and_method() {
    let dir = fresh_dir("scala-doc-attach");
    write(
        &dir,
        "Greeter.scala",
        "/** Greets a user. */\nclass Greeter {\n  /** Says hello. */\n  def hello(): Unit = {}\n}\n",
    );
    let refs = run_index_with_refs(&dir, &[]);
    let class = refs
        .iter()
        .find(|e| e["name"].as_str() == Some("Greeter") && e["kind"].as_str() == Some("class"))
        .expect("Greeter class entity should be emitted");
    assert_eq!(
        class["doc"].as_str(),
        Some("Greets a user."),
        "Scala class doc should be leading Scaladoc; got {:?}",
        class["doc"],
    );
    let method = refs
        .iter()
        .find(|e| {
            e["name"].as_str() == Some("Greeter.hello")
                && e["kind"].as_str() == Some("method")
        })
        .expect("Greeter.hello method entity should be emitted");
    assert_eq!(
        method["doc"].as_str(),
        Some("Says hello."),
        "Scala method doc should be its own leading Scaladoc; got {:?}",
        method["doc"],
    );
}

#[test]
fn php_doc_comment_attaches_to_following_class_and_method() {
    let dir = fresh_dir("php-doc-attach");
    write(
        &dir,
        "Greeter.php",
        "<?php\n/** Greets a user. */\nclass Greeter {\n    /** Says hello. */\n    public function hello() {}\n}\n",
    );
    let refs = run_index_with_refs(&dir, &[]);
    let class = refs
        .iter()
        .find(|e| e["name"].as_str() == Some("Greeter") && e["kind"].as_str() == Some("class"))
        .expect("Greeter class entity should be emitted");
    assert_eq!(
        class["doc"].as_str(),
        Some("Greets a user."),
        "PHP class doc should be leading PHPDoc; got {:?}",
        class["doc"],
    );
    let method_name_variants = ["Greeter::hello", "Greeter.hello"];
    let method = refs
        .iter()
        .find(|e| {
            e["kind"].as_str() == Some("method")
                && e["name"]
                    .as_str()
                    .map(|n| method_name_variants.contains(&n))
                    .unwrap_or(false)
        })
        .expect("Greeter::hello method entity should be emitted");
    assert_eq!(
        method["doc"].as_str(),
        Some("Says hello."),
        "PHP method doc should be its own leading PHPDoc; got {:?}",
        method["doc"],
    );
}

#[test]
fn manifest_resolved_edges_carry_callee_id() {
    // Go file-resolved edge — `internal/utils/helper.go/Helper`.
    // The callee_id should mirror with `::` separator instead of `/`.
    let dir = fresh_dir("callee-id-go");
    write(
        &dir,
        "go.mod",
        "module github.com/acme/myproj\n\ngo 1.21\n",
    );
    write(
        &dir,
        "internal/utils/helper.go",
        "package utils\n\nfunc Helper() {}\n",
    );
    write(
        &dir,
        "main.go",
        "package main\n\nimport \"github.com/acme/myproj/internal/utils\"\n\nfunc main() {\n    utils.Helper()\n}\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let resolved = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("main.go")
                && r["confidence"].as_f64() == Some(0.7)
                && r["name"].as_str().map(|s| s.contains("internal/utils/helper.go")).unwrap_or(false)
        })
        .expect("go.mod-resolved edge");
    assert_eq!(
        resolved["callee_id"].as_str(),
        Some("internal/utils/helper.go::Helper"),
        "callee_id should be `<file>::<symbol>`; got {:?}",
        resolved["callee_id"],
    );
}

#[test]
fn csharp_callee_id_carries_class_and_method() {
    // C# resolver should emit callee_id as `<file>::<Class>::<Method>`.
    let dir = fresh_dir("callee-id-cs");
    write(
        &dir,
        "Lib/Helper.cs",
        "namespace Lib;\npublic static class Helper {\n    public static void Do() {}\n}\n",
    );
    write(
        &dir,
        "App/Main.cs",
        "using Lib;\nclass App {\n    static void Main() {\n        Helper.Do();\n    }\n}\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let resolved = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("call")
                && r["file"].as_str() == Some("App/Main.cs")
                && r["confidence"].as_f64() == Some(0.7)
        })
        .expect("C# resolved edge");
    assert_eq!(
        resolved["callee_id"].as_str(),
        Some("Lib/Helper.cs::Helper::Do"),
        "C# callee_id should be `<file>::<Class>::<Method>`; got {:?}",
        resolved["callee_id"],
    );
}

// --- P5.15 — `external:` sentinel entities for unresolved imports ---------
//
// Repowise emits a graph node for every import target that doesn't
// resolve to a workspace file. Sigil mirrors this: after tier-3 resolution
// runs, walk import entities whose target lives outside the workspace and
// emit a synthetic `{kind: "external", name: "external:<modpath>"}`
// entity. Lets downstream consumers (contracts, heritage, future
// cross-repo) see external dependencies as first-class nodes.

#[test]
fn go_third_party_import_emits_external_sentinel_entity() {
    // Workspace's go.mod declares `github.com/acme/myproj`. main.go
    // imports `github.com/external/lib` — third-party, doesn't resolve
    // to any workspace file. Expected: an external entity.
    let dir = fresh_dir("external-go");
    write(
        &dir,
        "go.mod",
        "module github.com/acme/myproj\n\ngo 1.21\n",
    );
    write(
        &dir,
        "main.go",
        "package main\n\nimport \"github.com/external/lib\"\n\nfunc main() {\n    lib.Doit()\n}\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    // The entities (not just refs) live on stderr too. Find the synthetic
    // external entity by kind+name.
    let external = refs
        .iter()
        .find(|r| {
            r["kind"].as_str() == Some("external")
                && r["name"].as_str() == Some("external:github.com/external/lib")
        })
        .unwrap_or_else(|| {
            let all_external: Vec<_> = refs
                .iter()
                .filter(|r| r["kind"].as_str() == Some("external"))
                .map(|r| r["name"].as_str())
                .collect();
            panic!("expected external sentinel entity.\nALL external entities: {all_external:?}")
        });
    assert_eq!(external["kind"].as_str(), Some("external"));
}

#[test]
fn workspace_internal_go_import_does_not_emit_external_sentinel() {
    // Workspace go.mod = github.com/acme/myproj, and main.go imports
    // github.com/acme/myproj/internal/utils which IS in the workspace.
    // Should NOT emit an external entity.
    let dir = fresh_dir("not-external-go");
    write(
        &dir,
        "go.mod",
        "module github.com/acme/myproj\n\ngo 1.21\n",
    );
    write(
        &dir,
        "internal/utils/helper.go",
        "package utils\n\nfunc Helper() {}\n",
    );
    write(
        &dir,
        "main.go",
        "package main\n\nimport \"github.com/acme/myproj/internal/utils\"\n\nfunc main() {\n    utils.Helper()\n}\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let bad = refs.iter().any(|r| {
        r["kind"].as_str() == Some("external")
            && r["name"]
                .as_str()
                .map(|s| s.contains("internal/utils"))
                .unwrap_or(false)
    });
    assert!(!bad, "workspace-internal imports must not be flagged external");
}

#[test]
fn rust_external_crate_emits_external_sentinel() {
    // Workspace defines `myapp_core` only. `serde_json` is third-party
    // → not in cargo.crates → should produce an external entity.
    let dir = fresh_dir("external-rust");
    write(
        &dir,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\"]\n",
    );
    write(
        &dir,
        "crates/myapp-core/Cargo.toml",
        "[package]\nname = \"myapp-core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(
        &dir,
        "crates/myapp-core/src/lib.rs",
        "use serde_json::Value;\n\npub fn helper() {}\n",
    );
    let refs = run_index_with_refs(&dir, &[]);

    let external = refs.iter().find(|r| {
        r["kind"].as_str() == Some("external")
            && r["name"]
                .as_str()
                .map(|s| s.contains("serde_json"))
                .unwrap_or(false)
    });
    assert!(
        external.is_some(),
        "expected external sentinel for `serde_json`"
    );
}
