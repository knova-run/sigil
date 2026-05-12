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
