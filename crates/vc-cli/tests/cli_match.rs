//! `vc plan match` — structural match-and-rewrite plan CLI, and the
//! uniform `vc plan refresh` behavior across all three plan forms
//! (edit/import replay the stored edits against current content; a
//! match-form plan re-runs the FULL match pipeline from its stored
//! selector). Task 13.

use assert_cmd::Command;

fn vc(dir: &std::path::Path) -> Command {
    let mut c = Command::cargo_bin("vc").unwrap();
    c.current_dir(dir);
    c
}

fn plans_count(root: &std::path::Path) -> usize {
    let dir = root.join(".vc/plans");
    if !dir.exists() {
        return 0;
    }
    std::fs::read_dir(dir).unwrap().count()
}

/// `vc plan match` reports the TRUE site count the matcher found (not a
/// caller-guessed number) — two files, three total call sites.
#[test]
fn plan_match_reports_true_site_count() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(
        r.join("a.rs"),
        "fn main() { fetch_config(a); fetch_config(b); }\n",
    )
    .unwrap();
    std::fs::write(r.join("b.rs"), "fn other() { fetch_config(c); }\n").unwrap();

    let out = vc(r)
        .args([
            "--json",
            "plan",
            "match",
            "--pattern",
            "fetch_config($$$A)",
            "--rewrite",
            "load_config($$$A)",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["sites"], 3);
    assert_eq!(v["files"], 2);
    assert!(v["sha8"].as_str().unwrap().len() == 8);
    assert_eq!(plans_count(r), 1);
}

/// `--expect N` mismatch refuses (`Usage`, exit 2) and stores NOTHING —
/// `.vc/plans` must be untouched by the failed attempt.
#[test]
fn expect_mismatch_exits_2_and_stores_nothing() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn main() { fetch_config(a); }\n").unwrap();

    let before = plans_count(r);
    let assert = vc(r)
        .args([
            "--json",
            "plan",
            "match",
            "--pattern",
            "fetch_config($$$A)",
            "--rewrite",
            "load_config($$$A)",
            "--expect",
            "99",
        ])
        .assert()
        .failure()
        .code(2);
    let out = assert.get_output();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["error"]["kind"].as_str().unwrap(), "usage");
    let message = v["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("expected 99 sites, found 1"),
        "message: {message}"
    );
    assert!(message.contains("plan not stored"), "message: {message}");
    assert_eq!(
        plans_count(r),
        before,
        "a failed --expect check must store nothing"
    );
}

/// `vc show` on a match plan previews the actual rewrite: old text
/// prefixed `-`, new text prefixed `+`.
#[test]
fn vc_show_previews_the_rewrite() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn main() { fetch_config(a); }\n").unwrap();

    let out = vc(r)
        .args([
            "--json",
            "plan",
            "match",
            "--pattern",
            "fetch_config($$$A)",
            "--rewrite",
            "load_config($$$A)",
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

    let out = vc(r)
        .args(["show", &sha8])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("-fetch_config(a)"), "preview: {text}");
    assert!(text.contains("+load_config(a)"), "preview: {text}");
}

/// Refresh of a match plan after the file changed produces a NEW sha8
/// whose edits reflect CURRENT content — a fresh call site added after
/// planning must show up in the refreshed plan, not just the original one.
#[test]
fn refresh_of_match_plan_after_change_produces_new_sha8_with_current_content() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn main() { fetch_config(a); }\n").unwrap();

    let out = vc(r)
        .args([
            "--json",
            "plan",
            "match",
            "--pattern",
            "fetch_config($$$A)",
            "--rewrite",
            "load_config($$$A)",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let sha8 = v["sha8"].as_str().unwrap().to_string();
    assert_eq!(v["sites"], 1);

    // A second call site appears after the plan was made.
    std::fs::write(
        r.join("a.rs"),
        "fn main() { fetch_config(a); fetch_config(b); }\n",
    )
    .unwrap();

    let out = vc(r)
        .args(["--json", "plan", "refresh", &sha8])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v2: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let sha8b = v2["sha8"].as_str().unwrap().to_string();
    assert_ne!(
        sha8b, sha8,
        "refresh against changed content must mint a new plan"
    );
    assert_eq!(
        v2["sites"], 2,
        "refresh must re-run the matcher against CURRENT content"
    );

    vc(r).args(["apply", &sha8b]).assert().success();
    assert_eq!(
        std::fs::read_to_string(r.join("a.rs")).unwrap(),
        "fn main() { load_config(a); load_config(b); }\n"
    );
}

/// Regression: refresh of an ordinary edit-form plan still works — the
/// match-form refresh path must not have disturbed the pre-existing
/// edit/import refresh behavior.
#[test]
fn refresh_of_edit_plan_still_works() {
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

    // Drift the file so the original plan would be stale, then refresh.
    std::fs::write(r.join("a.rs"), "fn old_name() {}\n// drifted\n").unwrap();

    let out = vc(r)
        .args(["--json", "plan", "refresh", &sha8])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let sha8b = serde_json::from_slice::<serde_json::Value>(&out).unwrap()["sha8"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(sha8b, sha8);

    vc(r).args(["apply", &sha8b]).assert().success();
    assert_eq!(
        std::fs::read_to_string(r.join("a.rs")).unwrap(),
        "fn new_name() {}\n// drifted\n"
    );
}

/// A scope spanning two supported languages with no `--lang` given must
/// refuse (`Usage`, exit 2) naming the mix, rather than silently picking
/// one language over the other.
#[test]
fn mixed_language_scope_without_lang_is_usage() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn main() { fetch_config(a); }\n").unwrap();
    std::fs::write(r.join("b.py"), "def f():\n    fetch_config(a)\n").unwrap();

    let assert = vc(r)
        .args([
            "--json",
            "plan",
            "match",
            "--pattern",
            "fetch_config($$$A)",
            "--rewrite",
            "load_config($$$A)",
        ])
        .assert()
        .failure()
        .code(2);
    let v: serde_json::Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(v["error"]["kind"].as_str().unwrap(), "usage");
    let message = v["error"]["message"].as_str().unwrap();
    assert!(message.contains("pass --lang"), "message: {message}");
    assert_eq!(plans_count(r), 0);
}

/// A match plan whose scope includes a non-parsing file stores the
/// matcher's warning ON THE PLAN, and `vc show` prints it after the
/// preview.
#[test]
fn match_plan_with_skipped_file_stores_warning_and_show_prints_it() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("good.rs"), "fn main() { fetch_config(a); }\n").unwrap();
    std::fs::write(r.join("bad.rs"), "fn broken( { fetch_config(a) ]]]\n").unwrap();

    let assert = vc(r)
        .args([
            "--json",
            "plan",
            "match",
            "--pattern",
            "fetch_config($$$A)",
            "--rewrite",
            "load_config($$$A)",
        ])
        .assert()
        .success();
    let out = assert.get_output();
    // Plan-time CLI joins matcher warnings into the existing warning
    // convention (stderr "warning: ..." line), even on the success path.
    let stderr = String::from_utf8(out.stderr.clone()).unwrap();
    assert!(stderr.contains("warning:"), "stderr: {stderr}");
    assert!(stderr.contains("bad.rs"), "stderr: {stderr}");

    let sha8 = serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["sha8"]
        .as_str()
        .unwrap()
        .to_string();

    let out = vc(r)
        .args(["show", &sha8])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("warning: bad.rs"),
        "vc show must print the stored warning after the preview: {text}"
    );
    assert!(text.contains("did not parse"), "preview: {text}");
}

/// `vc show --json` must not widen the spec-pinned `{sha8, preview}`
/// shape for a plan that has no warnings —
/// `"warnings"` must be ABSENT, not present as an empty array. Pins the
/// exact key set, not just individual key presence.
#[test]
fn show_json_omits_warnings_key_when_plan_has_none() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn main() { fetch_config(a); }\n").unwrap();

    let out = vc(r)
        .args([
            "--json",
            "plan",
            "match",
            "--pattern",
            "fetch_config($$$A)",
            "--rewrite",
            "load_config($$$A)",
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

    let out = vc(r)
        .args(["--json", "show", &sha8])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let obj = v.as_object().unwrap();
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["preview", "sha8"],
        "warnings-less plan must have EXACTLY {{sha8, preview}}, no stray warnings key"
    );
}

/// The mirror case: a match plan WITH a stored warning must include the
/// `warnings` array in `vc show --json`, alongside the exact same
/// `{sha8, preview}` pair.
#[test]
fn show_json_includes_warnings_key_when_plan_has_some() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("good.rs"), "fn main() { fetch_config(a); }\n").unwrap();
    std::fs::write(r.join("bad.rs"), "fn broken( { fetch_config(a) ]]]\n").unwrap();

    let out = vc(r)
        .args([
            "--json",
            "plan",
            "match",
            "--pattern",
            "fetch_config($$$A)",
            "--rewrite",
            "load_config($$$A)",
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

    let out = vc(r)
        .args(["--json", "show", &sha8])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let obj = v.as_object().unwrap();
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["preview", "sha8", "warnings"]);
    let warnings = v["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].as_str().unwrap().contains("bad.rs"));
}
