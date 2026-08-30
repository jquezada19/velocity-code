//! Certificate check at apply — scope-drift refusal (exit 4). Task 14, the
//! spec's flagship safety scenario: "a 24th site appeared in a file you
//! didn't plan." A match-form plan's `ProvenanceCert` records every file
//! its selector could see at plan time; before `vc apply` ever reaches the
//! kernel, the CLI re-derives the selector's CURRENT visible scope
//! (identical `walk_scoped(selector.paths) ∩ lang_tag == selector.lang`
//! definition) and refuses if a file OUTSIDE the plan's named set now
//! matches the selector — a change the plan never accounted for and never
//! asked the caller to review.

use assert_cmd::Command;

fn vc(dir: &std::path::Path) -> Command {
    let mut c = Command::cargo_bin("vc").unwrap();
    c.current_dir(dir);
    c
}

fn plan_match(r: &std::path::Path, pattern: &str, rewrite: &str) -> (String, serde_json::Value) {
    let out = vc(r)
        .args([
            "--json",
            "plan",
            "match",
            "--pattern",
            pattern,
            "--rewrite",
            rewrite,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let sha8 = v["sha8"].as_str().unwrap().to_string();
    (sha8, v)
}

/// The canonical scenario: a plan matches only `a.rs`. `b.rs` is later
/// mutated so the SAME pattern now matches there too — a site the plan
/// never named and never asked the caller to review. `apply` must refuse
/// (`ScopeDrift`, exit 4) rather than silently applying only what it
/// planned while a real, live match sits untouched outside the named set.
#[test]
fn a_new_match_outside_the_named_set_refuses_scope_drift_exit_4() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn main() { fetch_config(a); }\n").unwrap();
    std::fs::write(r.join("b.rs"), "fn other() {}\n").unwrap();

    let (sha8, v) = plan_match(r, "fetch_config($$$A)", "load_config($$$A)");
    assert_eq!(v["sites"], 1);
    assert_eq!(v["files"], 1);

    let a_before = std::fs::read(r.join("a.rs")).unwrap();

    // b.rs gains a match after the plan was built — the "24th site".
    std::fs::write(r.join("b.rs"), "fn other() { fetch_config(x); }\n").unwrap();

    let assert = vc(r).args(["apply", &sha8]).assert().failure().code(4);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("scope-drift: b.rs"), "stderr: {stderr}");
    assert!(stderr.contains("next: vc plan refresh"), "stderr: {stderr}");

    let a_after = std::fs::read(r.join("a.rs")).unwrap();
    assert_eq!(a_before, a_after, "kernel apply must never have run");
}

/// The false-refusal half of the gate: drift is NOT "any change outside
/// the named set" — a change to `b.rs` that does not create a new match
/// must let `apply` proceed normally.
#[test]
fn unrelated_change_outside_named_set_does_not_refuse() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn main() { fetch_config(a); }\n").unwrap();
    std::fs::write(r.join("b.rs"), "fn other() {}\n").unwrap();

    let (sha8, _v) = plan_match(r, "fetch_config($$$A)", "load_config($$$A)");

    // b.rs changes, but never gains a fetch_config(...) call.
    std::fs::write(r.join("b.rs"), "fn other() { println!(\"hi\"); }\n").unwrap();

    vc(r).args(["apply", &sha8]).assert().success();
    assert_eq!(
        std::fs::read_to_string(r.join("a.rs")).unwrap(),
        "fn main() { load_config(a); }\n"
    );
}

/// A change to a NAMED file (in the plan's own `files` set) is the
/// kernel's existing stale check (exit 3), not scope drift (exit 4) — the
/// drift check is exclusively about files OUTSIDE the plan.
#[test]
fn change_to_a_named_file_is_stale_not_drift() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn main() { fetch_config(a); }\n").unwrap();

    let (sha8, _v) = plan_match(r, "fetch_config($$$A)", "load_config($$$A)");

    // a.rs (the NAMED file) changes after planning.
    std::fs::write(
        r.join("a.rs"),
        "fn main() { fetch_config(a); }\n// drifted\n",
    )
    .unwrap();

    let assert = vc(r).args(["apply", &sha8]).assert().failure().code(3);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.starts_with("stale:"), "stderr: {stderr}");
}

/// A file that did NOT EXIST at plan time — absent from the certificate's
/// `scope_files` entirely — must be treated exactly like a changed file:
/// if it now matches the selector, that's drift.
#[test]
fn new_file_that_matches_is_drift() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn main() { fetch_config(a); }\n").unwrap();

    let (sha8, _v) = plan_match(r, "fetch_config($$$A)", "load_config($$$A)");

    // A brand new file appears, matching the selector.
    std::fs::write(r.join("c.rs"), "fn c() { fetch_config(z); }\n").unwrap();

    let assert = vc(r).args(["apply", &sha8]).assert().failure().code(4);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("scope-drift: c.rs"), "stderr: {stderr}");
}

/// A candidate file that fails to parse at drift-check time cannot be
/// cleared by the selector at all (its match-or-not status is genuinely
/// unknown) — conservatively treated as drift too, rather than silently
/// let through because the matcher merely warned instead of hard-failing.
#[test]
fn candidate_file_that_fails_to_parse_is_drift() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn main() { fetch_config(a); }\n").unwrap();
    std::fs::write(r.join("b.rs"), "fn other() {}\n").unwrap();

    let (sha8, _v) = plan_match(r, "fetch_config($$$A)", "load_config($$$A)");

    // b.rs changes into something that no longer parses as rust.
    std::fs::write(r.join("b.rs"), "fn broken( { fetch_config(a) ]]]\n").unwrap();

    let assert = vc(r).args(["apply", &sha8]).assert().failure().code(4);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("scope-drift:"), "stderr: {stderr}");
    assert!(stderr.contains("b.rs"), "stderr: {stderr}");
}

/// End-to-end recovery path: after a scope-drift refusal, `vc plan
/// refresh` re-runs the full selector pipeline against the CURRENT tree,
/// and the refreshed plan applies cleanly — the caller's actual way out.
#[test]
fn after_refresh_the_refreshed_plan_applies_cleanly() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn main() { fetch_config(a); }\n").unwrap();
    std::fs::write(r.join("b.rs"), "fn other() {}\n").unwrap();

    let (sha8, _v) = plan_match(r, "fetch_config($$$A)", "load_config($$$A)");

    std::fs::write(r.join("b.rs"), "fn other() { fetch_config(x); }\n").unwrap();

    vc(r).args(["apply", &sha8]).assert().failure().code(4);

    let out = vc(r)
        .args(["--json", "plan", "refresh", &sha8])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let sha8b = v["sha8"].as_str().unwrap().to_string();
    assert_eq!(v["sites"], 2, "refresh must see both a.rs and b.rs now");

    vc(r).args(["apply", &sha8b]).assert().success();
    assert_eq!(
        std::fs::read_to_string(r.join("a.rs")).unwrap(),
        "fn main() { load_config(a); }\n"
    );
    assert_eq!(
        std::fs::read_to_string(r.join("b.rs")).unwrap(),
        "fn other() { load_config(x); }\n"
    );
}

/// Fix round 1 (review finding, Important): a selector-visible, out-of-plan
/// file that becomes UNREADABLE (permissions flipped, not deleted) must
/// fail CLOSED — refuse `ScopeDrift` naming it — rather than being
/// silently skipped. A file outside `plan.files` is invisible to the
/// kernel's own stale check, so this drift check is the ONE place that can
/// catch it; treating "can't read it" the same as "definitely unchanged"
/// would leave exactly the gap the check exists to close.
#[cfg(unix)]
#[test]
fn unreadable_candidate_fails_closed_scope_drift_exit_4() {
    use std::os::unix::fs::PermissionsExt;

    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn main() { fetch_config(a); }\n").unwrap();
    std::fs::write(r.join("b.rs"), "fn other() {}\n").unwrap();

    let (sha8, _v) = plan_match(r, "fetch_config($$$A)", "load_config($$$A)");

    let b_path = r.join("b.rs");
    std::fs::set_permissions(&b_path, std::fs::Permissions::from_mode(0o000)).unwrap();

    let output = vc(r).args(["apply", &sha8]).output().unwrap();

    // Restore permissions before any assertion below can panic, so the
    // tempdir's own Drop cleanup is never at the mercy of a failed test.
    std::fs::set_permissions(&b_path, std::fs::Permissions::from_mode(0o644)).unwrap();

    assert_eq!(output.status.code(), Some(4));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("scope-drift:"), "stderr: {stderr}");
    assert!(stderr.contains("b.rs"), "stderr: {stderr}");
}

/// The benign sub-case, pinned alongside the fail-closed case above so the
/// two don't get confused: a candidate file DELETED since plan time (a
/// `NotFound` read, not any other I/O error) cannot contain a new match —
/// it's simply out of scope now — so `apply` must still succeed as long as
/// nothing else drifted.
#[test]
fn deleted_candidate_is_benign_apply_still_succeeds() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn main() { fetch_config(a); }\n").unwrap();
    std::fs::write(r.join("b.rs"), "fn other() {}\n").unwrap();

    let (sha8, _v) = plan_match(r, "fetch_config($$$A)", "load_config($$$A)");

    // b.rs (in scope, unnamed, no match at plan time) is removed entirely.
    std::fs::remove_file(r.join("b.rs")).unwrap();

    vc(r).args(["apply", &sha8]).assert().success();
    assert_eq!(
        std::fs::read_to_string(r.join("a.rs")).unwrap(),
        "fn main() { load_config(a); }\n"
    );
}

/// Fix round 1 (folded minor): when MORE THAN ONE out-of-plan file drifted
/// into a live match, the refusal names the first drifted path and counts
/// ONLY that file's own sites — not the total across every drifted file
/// (the bug: `b.rs`'s message previously reported `c.rs`'s sites too) —
/// and separately notes how many OTHER files also drifted.
#[test]
fn multi_file_drift_attributes_site_count_per_file() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn main() { fetch_config(a); }\n").unwrap();
    std::fs::write(r.join("b.rs"), "fn other() {}\n").unwrap();
    std::fs::write(r.join("c.rs"), "fn third() {}\n").unwrap();

    let (sha8, _v) = plan_match(r, "fetch_config($$$A)", "load_config($$$A)");

    // b.rs gains exactly ONE new site; c.rs gains TWO.
    std::fs::write(r.join("b.rs"), "fn other() { fetch_config(x); }\n").unwrap();
    std::fs::write(
        r.join("c.rs"),
        "fn third() { fetch_config(y); fetch_config(z); }\n",
    )
    .unwrap();

    let assert = vc(r).args(["apply", &sha8]).assert().failure().code(4);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    // b.rs sorts before c.rs, so it's the named file — its own count (1),
    // never c.rs's (2) folded in, plus a note that one other file drifted.
    assert!(
        stderr.contains("b.rs gained a match since plan (1 new site(s)) (+1 more file(s))"),
        "stderr: {stderr}"
    );
}

/// A stored plan that still SAYS `form: Match` but has lost its selector
/// and certificate must refuse, not apply. The old drift check read the
/// two halves with `let (Some, Some) = ... else { return Ok(()) }`, so a
/// plan in exactly this shape skipped the guard silently and went straight
/// through to the kernel — the one plan shape where skipping is least
/// defensible.
///
/// The tampered file is rewritten under the filename that matches its OWN
/// recomputed digest, so the content-addressed integrity check passes and
/// the form check is what has to catch it.
#[test]
fn match_form_plan_stripped_of_selector_and_certificate_refuses() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn main() { fetch_config(a); }\n").unwrap();

    let (sha8, _v) = plan_match(r, "fetch_config($$$A)", "load_config($$$A)");

    // Strip both halves and re-store under the resulting content's own id.
    let plans = r.join(".vc/plans");
    let original = std::fs::read_dir(&plans)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&sha8))
        })
        .expect("the stored plan file");
    let mut json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&original).unwrap()).unwrap();
    let obj = json.as_object_mut().unwrap();
    obj.remove("selector");
    obj.remove("certificate");
    assert_eq!(
        json["form"], "Match",
        "the form claim is what must be caught"
    );

    // Name the file by the tampered content's OWN digest, computed the way
    // the kernel computes it, so `Plan::load`'s content-addressed integrity
    // check passes and the form check is the only thing left to refuse.
    let id = serde_json::from_value::<velocity_code_kernel::plan::Plan>(json.clone())
        .expect("a match plan minus its two optional halves still deserializes")
        .id();
    std::fs::remove_file(&original).unwrap();
    std::fs::write(
        plans.join(format!("{id}.json")),
        serde_json::to_vec_pretty(&json).unwrap(),
    )
    .unwrap();

    let assert = vc(r).args(["apply", &id[..8]]).assert().failure().code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.starts_with("malformed:"), "stderr: {stderr}");

    assert_eq!(
        std::fs::read_to_string(r.join("a.rs")).unwrap(),
        "fn main() { fetch_config(a); }\n",
        "the refusal must leave the tree untouched"
    );
}

/// Edit-form plans carry no `certificate`/`selector` — the drift check
/// must be a no-op for them, not error out on `None.unwrap()` or similar.
#[test]
fn edit_form_plan_skips_the_drift_check_entirely() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn old_name() {}\n").unwrap();

    let out = vc(r)
        .args([
            "--json", "plan", "edit", "a.rs", "--old", "old_name", "--new", "new_name",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let sha8 = serde_json::from_slice::<serde_json::Value>(&out).unwrap()["sha8"]
        .as_str()
        .unwrap()
        .to_string();

    // Unrelated new file appears — must not trip any drift logic for a
    // plan with no selector/certificate to check against.
    std::fs::write(r.join("b.rs"), "fn other() {}\n").unwrap();

    vc(r).args(["apply", &sha8]).assert().success();
    assert_eq!(
        std::fs::read_to_string(r.join("a.rs")).unwrap(),
        "fn new_name() {}\n"
    );
}
