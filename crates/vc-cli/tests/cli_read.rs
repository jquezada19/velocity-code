use assert_cmd::Command;

fn vc(dir: &std::path::Path) -> Command {
    let mut c = Command::cargo_bin("vc").unwrap();
    c.current_dir(dir);
    c
}

/// Same struct+impl+method+free-fn fixture `vc-lang::outline`'s own unit
/// tests use — pins the CLI boundary: skeleton renders with `… N lines`
/// collapse markers, nested methods indent, and the header matches query's
/// `epoch {epoch8}` style.
#[test]
fn outline_renders_skeleton_with_collapse_markers_and_epoch_header() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(
        r.join("plan.rs"),
        "\n/// doc\npub struct Plan { pub version: u32 }\n\nimpl Plan {\n    pub fn id(&self) -> String { String::new() }\n}\n\nfn free() {}\n",
    )
    .unwrap();

    let out = vc(r)
        .args(["outline", "plan.rs"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    let header = text.lines().next().unwrap();
    assert!(
        header.starts_with("epoch "),
        "header must start with epoch: {header}"
    );
    assert!(text.contains("3: pub struct Plan { … 1 lines }"));
    assert!(text.contains("5: impl Plan { … 3 lines }"));
    assert!(
        text.contains("  6: pub fn id(&self) -> String { … 1 lines }"),
        "expected a two-space-indented nested method line, got: {text}"
    );
    assert!(text.contains("9: fn free() { … 1 lines }"));
}

/// `--json` shape: `{epoch8, outline, elided}`.
#[test]
fn outline_json_shape_matches_contract() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn free() {}\n").unwrap();

    let out = vc(r)
        .args(["--json", "outline", "a.rs"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["epoch8"].as_str().unwrap().len(), 8);
    assert_eq!(v["elided"], 0);
    assert!(
        v["outline"]
            .as_str()
            .unwrap()
            .contains("1: fn free() { … 1 lines }")
    );
}

/// A budget too small to hold the whole skeleton elides the nested method
/// first and reports the elided count, mirroring `query --budget`'s CLI
/// contract for `elided`.
#[test]
fn outline_budget_elides_nested_lines_and_reports_count() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(
        r.join("plan.rs"),
        "impl Plan {\n    pub fn id(&self) -> String { String::new() }\n}\n",
    )
    .unwrap();

    let out = vc(r)
        .args(["--json", "outline", "plan.rs", "--budget", "5"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert!(v["elided"].as_u64().unwrap() > 0);
}

/// Unsupported language (a `.txt` file, empty `lang_tag`) refuses with
/// `Usage`, exit 2, pointing at `vc read`.
#[test]
fn outline_unsupported_language_is_usage_error() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("notes.txt"), "hello\n").unwrap();

    let assert = vc(r)
        .args(["outline", "notes.txt"])
        .assert()
        .failure()
        .code(2);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("outline: unsupported language — vc read the file instead"),
        "got: {stderr}"
    );
}

/// `vc read file:2-3` returns exactly lines 2-3, each prefixed `{line}: `.
#[test]
fn read_range_returns_exact_lines_with_prefixes_and_epoch_header() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();

    let out = vc(r)
        .args(["read", "a.txt:2-3"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    let mut lines = text.lines();
    let header = lines.next().unwrap();
    assert!(
        header.starts_with("epoch "),
        "header must start with epoch: {header}"
    );
    let rest: Vec<&str> = lines.collect();
    assert_eq!(rest, vec!["2: two", "3: three"]);
}

/// No range at all reads the whole file.
#[test]
fn read_whole_file_returns_every_line() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.txt"), "one\ntwo\n").unwrap();

    let out = vc(r)
        .args(["read", "a.txt"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("1: one"));
    assert!(text.contains("2: two"));
}

/// `a == 0` and `a > b` are both `Usage` refusals (exit 2), not panics or
/// silent empty reads.
#[test]
fn read_invalid_ranges_are_usage_errors() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.txt"), "one\ntwo\nthree\n").unwrap();

    vc(r).args(["read", "a.txt:0-2"]).assert().failure().code(2);
    vc(r).args(["read", "a.txt:3-1"]).assert().failure().code(2);
}

/// A range whose end overshoots EOF clamps to the true end instead of
/// refusing — agents overshoot ranges constantly.
#[test]
fn read_range_beyond_eof_clamps_to_true_end() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.txt"), "one\ntwo\nthree\n").unwrap();

    let out = vc(r)
        .args(["--json", "read", "a.txt:2-999"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["start"], 2);
    assert_eq!(v["end"], 3, "end must clamp to the file's true last line");
    assert_eq!(v["text"], "2: two\n3: three");
}

/// Over-budget content refuses via the new `Budget` kind — exit 1, `budget:`
/// prefix, and a `vc outline <path>` next-hint — never a silent truncation.
#[test]
fn read_over_budget_refuses_with_budget_kind_and_outline_hint() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    let mut content = String::new();
    for i in 0..200 {
        content.push_str(&format!("line number {i} has some real content in it\n"));
    }
    std::fs::write(r.join("big.txt"), content).unwrap();

    let assert = vc(r)
        .args(["read", "big.txt", "--budget", "5"])
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.starts_with("budget:"),
        "expected a 'budget:' prefixed refusal, got: {stderr}"
    );
    assert!(
        stderr.contains("tokens (budget 5)"),
        "expected the token-estimate message shape, got: {stderr}"
    );
    assert!(
        stderr.contains("next: vc outline"),
        "expected a 'vc outline' next-hint, got: {stderr}"
    );
    assert!(stderr.contains("big.txt"), "got: {stderr}");
}

/// `--json` shape for `read`: `{epoch8, path, start, end, text}`.
#[test]
fn read_json_shape_matches_contract() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.txt"), "one\ntwo\n").unwrap();

    let out = vc(r)
        .args(["--json", "read", "a.txt"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["epoch8"].as_str().unwrap().len(), 8);
    assert_eq!(v["path"], "a.txt");
    assert_eq!(v["start"], 1);
    assert_eq!(v["end"], 2);
    assert_eq!(v["text"], "1: one\n2: two");
}

/// `vc read --symbol id` prints the unique method's full body, prefixed
/// per line, when the name resolves to exactly one symbol.
#[test]
fn read_symbol_unique_prints_full_body() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(
        r.join("plan.rs"),
        "impl Plan {\n    pub fn id(&self) -> String {\n        String::new()\n    }\n}\n",
    )
    .unwrap();

    let out = vc(r)
        .args(["read", "--symbol", "id"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("2: "), "got: {text}");
    assert!(text.contains("pub fn id(&self) -> String {"), "got: {text}");
    assert!(text.contains("String::new()"), "got: {text}");
    assert!(text.contains("4:     }"), "got: {text}");
}

/// Multiple hits for the same name refuse `Ambiguous`, listing each
/// candidate as `path:line` in the message.
#[test]
fn read_symbol_ambiguous_lists_candidates_as_path_colon_line() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn dup() {}\n").unwrap();
    std::fs::write(r.join("b.rs"), "fn dup() {}\n").unwrap();

    let assert = vc(r)
        .args(["read", "--symbol", "dup"])
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.starts_with("ambiguous:"), "got: {stderr}");
    assert!(stderr.contains("a.rs:1"), "got: {stderr}");
    assert!(stderr.contains("b.rs:1"), "got: {stderr}");
}

/// Zero hits refuses `NotFound`, exit 1.
#[test]
fn read_symbol_zero_hits_is_not_found() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn other() {}\n").unwrap();

    vc(r)
        .args(["read", "--symbol", "zzz"])
        .assert()
        .failure()
        .code(1);
}

/// Neither a path nor `--symbol` is a usage error, not a panic.
#[test]
fn read_requires_a_path_or_symbol() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    vc(r).args(["read"]).assert().failure().code(2);
}

/// A path *and* `--symbol` together is also a usage error — mutually
/// exclusive, same posture as `query --symbol --regex`.
#[test]
fn read_path_and_symbol_together_is_usage_error() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn other() {}\n").unwrap();

    vc(r)
        .args(["read", "a.rs", "--symbol", "other"])
        .assert()
        .failure()
        .code(2);
}
