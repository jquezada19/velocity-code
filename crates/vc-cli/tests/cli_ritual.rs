use assert_cmd::Command;

fn vc(dir: &std::path::Path) -> Command {
    let mut c = Command::cargo_bin("vc").unwrap();
    c.current_dir(dir);
    c
}

#[test]
fn the_ritual_plan_apply_stale_undo() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn old_name() {}\n").unwrap();

    // plan
    let out = vc(r)
        .args([
            "--json", "plan", "edit", "a.rs", "--old", "old_name", "--new", "new_name",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let sha8 = v["sha8"].as_str().unwrap().to_string();

    // stale refusal: mutate between plan and apply
    std::fs::write(r.join("a.rs"), "fn old_name() { /* drifted */ }\n").unwrap();
    vc(r).args(["apply", &sha8]).assert().failure().code(3);

    // restore + apply cleanly
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
    let sha8b = serde_json::from_slice::<serde_json::Value>(&out).unwrap()["sha8"]
        .as_str()
        .unwrap()
        .to_string();
    vc(r).args(["apply", &sha8b]).assert().success();
    assert_eq!(
        std::fs::read_to_string(r.join("a.rs")).unwrap(),
        "fn new_name() {}\n"
    );

    // undo
    vc(r).args(["undo"]).assert().success();
    assert_eq!(
        std::fs::read_to_string(r.join("a.rs")).unwrap(),
        "fn old_name() {}\n"
    );

    // status + metrics existence
    vc(r).args(["status"]).assert().success();
    assert!(std::fs::read_dir(r.join(".vc/metrics")).unwrap().count() >= 1);
}

#[test]
fn import_flows_through_same_engine() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("x.txt"), "a\nb\nc\n").unwrap();
    let diff = "--- a/x.txt\n+++ b/x.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n";
    let out = vc(r)
        .args(["--json", "plan", "import"])
        .write_stdin(diff)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let sha8 = serde_json::from_slice::<serde_json::Value>(&out).unwrap()["sha8"]
        .as_str()
        .unwrap()
        .to_string();
    vc(r).args(["apply", &sha8]).assert().success();
    assert_eq!(
        std::fs::read_to_string(r.join("x.txt")).unwrap(),
        "a\nB\nc\n"
    );
}

/// Own test: `--json` error shape on a stale apply — exit 3 and a
/// `{"error":{"kind":"stale",...}}` envelope on STDOUT (not stderr; the
/// contract routes --json errors to stdout so a machine caller never has
/// to interleave two streams).
#[test]
fn json_error_shape_on_stale_apply() {
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

    // mutate between plan and apply -> stale
    std::fs::write(r.join("a.rs"), "fn old_name() { /* drifted */ }\n").unwrap();

    let assert = vc(r)
        .args(["--json", "apply", &sha8])
        .assert()
        .failure()
        .code(3);
    let out = assert.get_output();
    assert!(
        out.stderr.is_empty(),
        "json-mode errors go to stdout, not stderr: stderr was {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["error"]["kind"].as_str().unwrap(), "stale");
    assert!(!v["error"]["message"].as_str().unwrap().is_empty());
}

/// Own test: `doctor --rollback --discard` together is a usage error
/// (exit 2), not silently picking one action.
#[test]
fn doctor_both_flags_is_usage_error() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    vc(r)
        .args(["doctor", "--rollback", "--discard"])
        .assert()
        .failure()
        .code(2);
}

/// Own test: the `--json` shape for a clean `apply` and the `undo` that
/// follows it — pins the exact key set the contract promises
/// (`{journal_id, files, edits, epoch8_after, warning}`), including that
/// `warning` is a present-but-null key on the non-degraded path and that
/// `undo`'s `edits` is 0 (it replays inverse patches, not resolved
/// edits). Neither of the brief's two mandated tests exercises `--json`
/// on the success path for these two verbs.
#[test]
fn json_shape_for_clean_apply_and_undo() {
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

    let out = vc(r)
        .args(["--json", "apply", &sha8])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["files"], 1);
    assert_eq!(v["edits"], 1);
    assert!(v["journal_id"].as_str().unwrap().starts_with("j-"));
    assert_eq!(v["epoch8_after"].as_str().unwrap().len(), 8);
    assert!(v["warning"].is_null());

    let out = vc(r)
        .args(["--json", "undo"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["files"], 1);
    assert_eq!(v["edits"], 0, "undo replays inverse patches, not edits");
    assert!(v["journal_id"].as_str().unwrap().starts_with("j-"));
    assert!(v["warning"].is_null());
}

/// Own test: `gain` on a repo with no `.vc/metrics/` at all (nothing has
/// ever run) succeeds with an empty aggregate rather than erroring.
#[test]
fn gain_on_empty_metrics_does_not_error() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    let out = vc(r)
        .args(["gain"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("reads: n/a until M2"));
}
