//! Crash-consistency kill-point harness (F26 gate — spec §8): for EVERY
//! kill point in `velocity_code_kernel::fault`'s fixed vocabulary
//! (`pre_journal`, `post_journal_entry`, `mid_files`, `pre_commit_marker`,
//! `post_commit_marker`), killing the real `vc` binary at that exact
//! point (exit code 86 — `std::process::exit`, no unwind, no `Drop`) and
//! then running `vc doctor --rollback` must leave plan A's two touched
//! files fully-pre or fully-post *together* — never one file rolled back
//! and the other not. That mixed state is the failure this harness exists
//! to catch, 100% of runs, no exceptions (pre-registered milestone gate).
//!
//! Plan A is a two-file plan built through `vc plan import` (one unified
//! diff touching both `a.rs` and `b.rs`), not two separate single-file
//! plans — a single-file plan can never make `mid_files` fire at all
//! (`apply::commit_files` only calls `fault::point("mid_files")` when
//! `changes.len() > 1`), so a two-file plan is the only way to exercise a
//! genuine partial multi-file write. Plan B (a single-file `vc plan edit`
//! on a third file, `c.txt`) is the post-recovery sanity check: the
//! kernel must still be usable after recovery, not just internally
//! consistent.
//!
//! Lives in `vc-cli` (not `vc-kernel`, where the brief originally staged
//! it) because `CARGO_BIN_EXE_vc` — the env var Cargo sets to the built
//! binary's path — is only populated for integration tests belonging to
//! the package that owns the `[[bin]]` target. Gated out of the default
//! `cargo test --workspace` run via `required-features` in
//! `vc-cli/Cargo.toml` (see the comment there): without the
//! `fault-injection` feature, `VC_FAULT_POINT` never fires an exit, and
//! every assertion here would fail against a fault point that simply
//! never happened.

use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn vc_bin() -> &'static str {
    env!("CARGO_BIN_EXE_vc")
}

/// Run `vc` with `args` in `dir`. `point` is `Some(name)` only for the one
/// call in each test meant to be killed; every other call passes `None`,
/// which explicitly clears `VC_FAULT_POINT` rather than merely omitting
/// it, so a stray ambient env var can never leak fault injection into a
/// call that isn't supposed to have any.
fn run(dir: &Path, point: Option<&str>, args: &[&str]) -> Output {
    let mut c = Command::new(vc_bin());
    c.current_dir(dir).args(args);
    match point {
        Some(p) => c.env("VC_FAULT_POINT", p),
        None => c.env_remove("VC_FAULT_POINT"),
    };
    c.output().unwrap()
}

/// Same as `run`, but feeds `stdin_text` to the child's stdin — for `plan
/// import`, which reads a unified diff from stdin.
fn run_stdin(dir: &Path, args: &[&str], stdin_text: &str) -> Output {
    use std::io::Write as _;
    let mut c = Command::new(vc_bin());
    c.current_dir(dir)
        .args(args)
        .env_remove("VC_FAULT_POINT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = c.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin_text.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "not JSON: {e}\nstdout={:?}\nstderr={:?}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn sha8_of(out: &Output) -> String {
    json(out)["sha8"].as_str().unwrap().to_string()
}

const PRE_A: &str = "fn old() {}\n";
const POST_A: &str = "fn new() {}\n";
const PRE_B: &str = "fn call() {\n    old();\n}\n";
const POST_B: &str = "fn call() {\n    new();\n}\n";
const PRE_C: &str = "post-recovery sanity check\n";
const POST_C: &str = "post-recovery sanity check OK\n";

/// One diff, two files: `a.rs` is a whole-file, no-context replacement
/// (proves the importer handles a hunk with no context lines); `b.rs`
/// changes one line inside two lines of context (an ordinary hunk). Both
/// hunks' accumulated old/new text must match each file's exact current
/// bytes — the importer is strict, zero-fuzz.
const PLAN_A_DIFF: &str = "\
--- a/a.rs
+++ b/a.rs
@@ -1,1 +1,1 @@
-fn old() {}
+fn new() {}
--- a/b.rs
+++ b/b.rs
@@ -1,3 +1,3 @@
 fn call() {
-    old();
+    new();
 }
";

/// Arrange a fresh repo, build plan A (two-file import) and plan B
/// (single-file edit on `c.txt`), kill `vc apply <plan A>` at `point`,
/// recover with `vc doctor --rollback`, and check:
///
/// 1. the fault point actually fired (exit 86) — otherwise every
///    assertion below would be checking a no-op, not a real kill;
/// 2. the *specific* on-disk shape the point is documented to produce,
///    checked before recovery runs — this is what proves the harness
///    genuinely exercised the named structural moment rather than
///    landing on an already-consistent state by luck. It matters most
///    for `mid_files`: the whole reason plan A is two files is to prove
///    a real mixed a.rs-post/b.rs-pre state existed right before
///    `doctor` ran;
/// 3. post-recovery, a.rs and b.rs are consistent *together* (both pre
///    or both post) — the F26 gate itself;
/// 4. post-recovery, they land on the specific side `doctor`'s own
///    contract promises (uncommitted always rolls back to pre; a
///    committed entry is left alone at post) — stricter than #3 alone,
///    and catches a kernel bug that rolls the "wrong" direction while
///    staying internally consistent, which #3 alone would miss;
/// 5. `c.txt`, never named by plan A, is untouched throughout;
/// 6. no leftover `.vc-tmp-*` file from the write-through discipline;
/// 7. the kernel is still usable: plan B applies cleanly post-recovery.
fn assert_kill_point_recovers(point: &str) {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();

    std::fs::write(r.join("a.rs"), PRE_A).unwrap();
    std::fs::write(r.join("b.rs"), PRE_B).unwrap();
    std::fs::write(r.join("c.txt"), PRE_C).unwrap();

    // Plan A: two-file plan via `plan import` (R2 fixture ruling) — the
    // only way to land two files in ONE apply, which is what makes
    // `mid_files` meaningful.
    let out = run_stdin(r, &["--json", "plan", "import"], PLAN_A_DIFF);
    assert!(
        out.status.success(),
        "plan import failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    assert_eq!(v["files"], 2, "fixture must touch exactly 2 files");
    assert_eq!(v["sites"], 2, "fixture must resolve exactly 2 edit sites");
    let sha8a = v["sha8"].as_str().unwrap().to_string();

    // Plan B: single-file `plan edit` on c.txt — the post-recovery
    // sanity apply.
    let out = run(
        r,
        None,
        &[
            "--json",
            "plan",
            "edit",
            "c.txt",
            "--old",
            "sanity check",
            "--new",
            "sanity check OK",
        ],
    );
    assert!(
        out.status.success(),
        "plan edit (c.txt) failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let sha8b = sha8_of(&out);

    // Kill during apply of plan A.
    let killed = run(r, Some(point), &["apply", &sha8a]);
    assert_eq!(
        killed.status.code(),
        Some(86),
        "point {point} did not fire (exit {:?}, stderr {})",
        killed.status.code(),
        String::from_utf8_lossy(&killed.stderr)
    );

    // Pre-recovery shape: proves the point landed exactly where the
    // kernel's doc comments say it does.
    let a_at_kill = std::fs::read_to_string(r.join("a.rs")).unwrap();
    let b_at_kill = std::fs::read_to_string(r.join("b.rs")).unwrap();
    let entry_path = r.join(".vc/journal/j-000001.json");
    let marker_path = r.join(".vc/journal/j-000001.committed");
    match point {
        "pre_journal" => {
            assert_eq!(a_at_kill, PRE_A, "pre_journal: a.rs must be untouched");
            assert_eq!(b_at_kill, PRE_B, "pre_journal: b.rs must be untouched");
            assert!(
                !entry_path.exists(),
                "pre_journal: no journal entry should exist yet"
            );
        }
        "post_journal_entry" => {
            assert_eq!(a_at_kill, PRE_A, "post_journal_entry: no file written yet");
            assert_eq!(b_at_kill, PRE_B, "post_journal_entry: no file written yet");
            assert!(
                entry_path.exists(),
                "post_journal_entry: entry must be journaled"
            );
            assert!(
                !marker_path.exists(),
                "post_journal_entry: not yet committed"
            );
        }
        "mid_files" => {
            assert_eq!(
                a_at_kill, POST_A,
                "mid_files: a.rs (sorted first) must already be written"
            );
            assert_eq!(
                b_at_kill, PRE_B,
                "mid_files: b.rs must NOT be written yet — this is the genuine \
                 partial multi-file state the two-file fixture exists to create"
            );
            assert!(entry_path.exists());
            assert!(!marker_path.exists(), "mid_files: not yet committed");
        }
        "pre_commit_marker" => {
            assert_eq!(a_at_kill, POST_A, "pre_commit_marker: all files written");
            assert_eq!(b_at_kill, POST_B, "pre_commit_marker: all files written");
            assert!(entry_path.exists());
            assert!(
                !marker_path.exists(),
                "pre_commit_marker: marker not yet written"
            );
        }
        "post_commit_marker" => {
            assert_eq!(a_at_kill, POST_A);
            assert_eq!(b_at_kill, POST_B);
            assert!(entry_path.exists());
            assert!(
                marker_path.exists(),
                "post_commit_marker: marker IS written"
            );
        }
        other => panic!("unknown kill point {other}"),
    }

    // The lock file remains with a dead pid — `Drop` never ran.
    assert!(
        r.join(".vc/journal/LOCK").is_file(),
        "point {point}: lock must remain after a hard kill"
    );

    // Recover.
    let doc = run(r, None, &["--json", "doctor", "--rollback"]);
    assert!(
        doc.status.success(),
        "doctor failed at {point}: {}",
        String::from_utf8_lossy(&doc.stderr)
    );
    let dv = json(&doc);
    assert_eq!(
        dv["lock_removed"], true,
        "point {point}: a dead-pid lock must always be cleared"
    );

    // doctor's own contract: an entry that was actually journaled before
    // the kill (post_journal_entry, mid_files, pre_commit_marker) is
    // uncommitted and gets rolled back; pre_journal never journaled
    // anything (nothing *to* roll back) and post_commit_marker's entry is
    // already committed (rolled back means restoring the *files*, which
    // is a different axis from "should the tree end up pre" — pre_journal
    // ends up pre with zero rollback work, since nothing ever moved).
    let expect_rollback_work = matches!(
        point,
        "post_journal_entry" | "mid_files" | "pre_commit_marker"
    );
    let rolled_back_count = dv["rolled_back"].as_array().unwrap().len();
    assert_eq!(
        rolled_back_count,
        if expect_rollback_work { 1 } else { 0 },
        "point {point}: unexpected rolled_back count"
    );

    // THE gate (F26 / R2 acceptance): a.rs and b.rs together, fully-pre or
    // fully-post — never mixed.
    let a_now = std::fs::read_to_string(r.join("a.rs")).unwrap();
    let b_now = std::fs::read_to_string(r.join("b.rs")).unwrap();
    let fully_pre = a_now == PRE_A && b_now == PRE_B;
    let fully_post = a_now == POST_A && b_now == POST_B;
    assert!(
        fully_pre || fully_post,
        "point {point}: tree neither fully-pre nor fully-post — a.rs={a_now:?} b.rs={b_now:?}"
    );
    // Stricter than the gate's bare minimum: pins the *direction* too —
    // every point except post_commit_marker (already durably committed
    // before the kill) must land back at pre.
    let expect_final_pre = point != "post_commit_marker";
    if expect_final_pre {
        assert!(
            fully_pre,
            "point {point}: expected fully-pre after recovery, got \
             a.rs={a_now:?} b.rs={b_now:?}"
        );
    } else {
        assert!(
            fully_post,
            "point {point}: a committed entry was rolled back — \
             a.rs={a_now:?} b.rs={b_now:?}"
        );
    }

    // c.txt was never named by plan A — untouched by the kill or the
    // recovery, regardless of which point fired.
    assert_eq!(
        std::fs::read_to_string(r.join("c.txt")).unwrap(),
        PRE_C,
        "point {point}: c.txt must be untouched by plan A's kill/recovery"
    );

    // No leftover temp file from the write-through discipline.
    for entry in std::fs::read_dir(r).unwrap() {
        let name = entry.unwrap().file_name();
        let name = name.to_string_lossy();
        assert!(
            !name.starts_with(".vc-tmp-"),
            "point {point}: leftover temp file {name}"
        );
    }

    // Kernel usable post-recovery: plan B applies cleanly.
    let ok = run(r, None, &["apply", &sha8b]);
    assert!(
        ok.status.success(),
        "post-recovery apply failed at {point}: {}",
        String::from_utf8_lossy(&ok.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(r.join("c.txt")).unwrap(),
        POST_C,
        "point {point}: plan B must actually land"
    );
}

#[test]
fn kill_at_pre_journal_recovers_to_pre() {
    assert_kill_point_recovers("pre_journal");
}

#[test]
fn kill_at_post_journal_entry_recovers_to_pre() {
    assert_kill_point_recovers("post_journal_entry");
}

#[test]
fn kill_at_mid_files_recovers_to_pre() {
    assert_kill_point_recovers("mid_files");
}

#[test]
fn kill_at_pre_commit_marker_recovers_to_pre() {
    assert_kill_point_recovers("pre_commit_marker");
}

#[test]
fn kill_at_post_commit_marker_stays_post() {
    assert_kill_point_recovers("post_commit_marker");
}
