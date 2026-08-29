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

/// I3 regression: a stale apply's `next:` hint must name a real
/// subcommand, and that subcommand must actually recover. Drift here
/// leaves the OLD TEXT (`old_name`) present but appends a line, so the
/// plan's edit would still *resolve* — it's the file's overall content
/// (and therefore its plan-time hash) that changed, which is exactly what
/// `apply` refuses on. `vc plan refresh <sha8>` re-resolves the same
/// edit against current content and mints a brand-new plan, which then
/// applies cleanly.
#[test]
fn stale_apply_hint_names_plan_refresh_which_recovers() {
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

    // Drift: append a line after planning. "old_name" is still present
    // (re-resolution will succeed), but the file's hash has changed, so
    // `apply` must refuse as stale.
    std::fs::write(r.join("a.rs"), "fn old_name() {}\n// drifted\n").unwrap();

    let assert = vc(r)
        .args(["--json", "apply", &sha8])
        .assert()
        .failure()
        .code(3);
    let stdout = assert.get_output().stdout.clone();
    let v: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(v["error"]["kind"].as_str().unwrap(), "stale");
    assert_eq!(
        v["error"]["next"].as_str().unwrap(),
        format!("vc plan refresh {sha8}"),
        "the hint must name the real `plan refresh` subcommand"
    );

    // Run the hint's own command — it must exist and must recover.
    let out = vc(r)
        .args(["--json", "plan", "refresh", &sha8])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let sha8b = v["sha8"].as_str().unwrap().to_string();
    assert_ne!(
        sha8b, sha8,
        "refresh must mint a new plan against the drifted content"
    );

    vc(r).args(["apply", &sha8b]).assert().success();
    assert_eq!(
        std::fs::read_to_string(r.join("a.rs")).unwrap(),
        "fn new_name() {}\n// drifted\n"
    );
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

/// C1 regression (a), end-to-end: a diff deleting a Lua/SQL comment line
/// `--x` must actually remove it, not silently no-op. Before the fix, the
/// hunk's body-line prefix check (`!body.starts_with("---")`) broke out of
/// the hunk the instant it hit the `---x` line (the `-` marker plus the
/// literal `--x` text), so `old` and `new` both ended up as just `keep\n`
/// — a no-op edit that resolves and applies cleanly against the real file
/// without ever touching the `--x` line.
#[test]
fn plan_import_deleting_dashdash_line_actually_removes_it() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("comment.lua"), "keep\n--x\n").unwrap();
    let diff = "--- a/comment.lua\n+++ b/comment.lua\n@@ -1,2 +1,1 @@\n keep\n---x\n";

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
        std::fs::read_to_string(r.join("comment.lua")).unwrap(),
        "keep\n",
        "the '--x' line must actually be deleted, not silently kept"
    );
}

/// C1 regression (b), end-to-end: a diff adding C's `++i;` must actually
/// add it. Symmetric to the deletion case above, on the `+` side.
#[test]
fn plan_import_adding_plusplus_line_actually_adds_it() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("loop.c"), "keep\n").unwrap();
    let diff = "--- a/loop.c\n+++ b/loop.c\n@@ -1,1 +1,2 @@\n keep\n+++i;\n";

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
        std::fs::read_to_string(r.join("loop.c")).unwrap(),
        "keep\n++i;\n",
        "the '++i;' line must actually be added, not silently dropped"
    );
}

/// C2 regression: `vc plan edit <file>` run from a subdirectory must
/// resolve `<file>` against the CWD (the shell/user's frame of reference),
/// not silently against the repo root — a repo with a same-named file at
/// both the root and in `sub/` must edit the SUBDIRECTORY's file when run
/// from `sub/`, leaving the root twin untouched.
#[test]
fn plan_edit_from_subdirectory_resolves_subdirectory_file_not_root_twin() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    // Pin `r` as the discovered root so `find_root` doesn't fall back to
    // `sub/` itself for lack of any `.vc` dir anywhere up the tree.
    std::fs::create_dir_all(r.join(".vc")).unwrap();
    std::fs::write(r.join("note.txt"), "root version\n").unwrap();
    std::fs::create_dir_all(r.join("sub")).unwrap();
    std::fs::write(r.join("sub/note.txt"), "sub version\n").unwrap();

    let sub = r.join("sub");
    let out = vc(&sub)
        .args([
            "--json",
            "plan",
            "edit",
            "note.txt",
            "--old",
            "sub version",
            "--new",
            "SUB VERSION",
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
    vc(&sub).args(["apply", &sha8]).assert().success();

    assert_eq!(
        std::fs::read_to_string(r.join("sub/note.txt")).unwrap(),
        "SUB VERSION\n",
        "the subdirectory's own file must be the one edited"
    );
    assert_eq!(
        std::fs::read_to_string(r.join("note.txt")).unwrap(),
        "root version\n",
        "the root twin must be left untouched"
    );
}

/// C2 regression: an absolute path argument that resolves outside the
/// repo root must refuse (`Usage`, exit 2) rather than being silently
/// accepted or escaping the repo.
#[test]
fn plan_edit_absolute_path_outside_root_is_usage_error() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn old_name() {}\n").unwrap();

    let outside = tempfile::tempdir().unwrap();
    let outside_file = outside.path().join("outside.txt");
    std::fs::write(&outside_file, "hello\n").unwrap();

    vc(r)
        .args([
            "plan",
            "edit",
            outside_file.to_str().unwrap(),
            "--old",
            "hello",
            "--new",
            "world",
        ])
        .assert()
        .failure()
        .code(2);

    assert_eq!(
        std::fs::read_to_string(&outside_file).unwrap(),
        "hello\n",
        "the out-of-root file must be untouched by the refusal"
    );
}

/// C2 residual regression: `rebase_user_path`'s `abs.canonicalize()` on a
/// missing file must report `not-found` with the user's ORIGINAL
/// (relative) path, matching every other kernel path that reports a
/// missing file — not `io` with the absolute, tempdir-prefixed path
/// leaked into the message.
#[test]
fn plan_edit_missing_file_is_not_found_with_users_relative_path() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();

    let out = vc(r)
        .args([
            "--json",
            "plan",
            "edit",
            "ghost.txt",
            "--old",
            "x",
            "--new",
            "y",
        ])
        .assert()
        .failure()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["error"]["kind"].as_str().unwrap(), "not-found");
    let message = v["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("ghost.txt"),
        "message must name the user's path: {message}"
    );
    assert!(
        !message.contains(&r.display().to_string()),
        "message must not leak the absolute tempdir-prefixed path: {message}"
    );
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
