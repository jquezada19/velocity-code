use assert_cmd::Command;

fn vc(dir: &std::path::Path) -> Command {
    let mut c = Command::cargo_bin("vc").unwrap();
    c.current_dir(dir);
    c
}

/// Same three-hit fixture as vc-query's own
/// `literal_search_finds_hits_across_files_in_deterministic_order` unit
/// test: `a.rs` matches once, `b.rs` matches twice, sorted (path, line,
/// col) so the order is deterministic. Pins the human header line and
/// that the rendered hits follow it.
#[test]
fn query_literal_prints_epoch_header_and_deterministic_hits() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn alpha() {}\n").unwrap();
    std::fs::write(r.join("b.rs"), "let alpha = 1;\nlet beta = alpha;\n").unwrap();

    let out = vc(r)
        .args(["query", "alpha"])
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
    assert!(
        header.ends_with(" — 3 hits"),
        "header must report 3 hits: {header}"
    );

    let rest: Vec<&str> = lines.collect();
    assert_eq!(
        rest,
        vec![
            "a.rs:1:4: fn alpha() {}",
            "b.rs:1:5: let alpha = 1;",
            "b.rs:2:12: let beta = alpha;",
        ]
    );
}

/// `--regex` mode: an alternation pattern matches hits in both files that
/// a literal search for "alpha" (or "beta" alone) would miss.
#[test]
fn query_regex_mode_matches_alternation_across_files() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn alpha() {}\n").unwrap();
    std::fs::write(r.join("b.rs"), "fn beta() {}\n").unwrap();

    let out = vc(r)
        .args(["query", "fn (alpha|beta)", "--regex"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("a.rs:1:1: fn alpha() {}"));
    assert!(text.contains("b.rs:1:1: fn beta() {}"));
}

/// Zero hits is not an error — it must exit 0 with a `0 hits` header, so
/// an agent can tell "found nothing" apart from "the command failed."
#[test]
fn query_zero_hits_exits_zero_with_zero_hits_header() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn alpha() {}\n").unwrap();

    let out = vc(r)
        .args(["query", "zzz"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("0 hits"),
        "expected a '0 hits' header, got: {text}"
    );
}

/// An invalid regex pattern is a usage error (exit 2), not a panic or a
/// silent empty result — human-mode stderr carries the `usage:` prefix
/// from `VcError`'s `Display` grammar.
#[test]
fn query_invalid_regex_is_usage_error() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn alpha() {}\n").unwrap();

    let assert = vc(r)
        .args(["query", "(", "--regex"])
        .assert()
        .failure()
        .code(2);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.starts_with("usage:"),
        "expected a 'usage:' prefixed refusal, got: {stderr}"
    );
}

/// `--json` shape: `{epoch8, hits: [{path, line, col, text}], elided}`.
#[test]
fn query_json_shape_matches_contract() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn alpha() {}\n").unwrap();

    let out = vc(r)
        .args(["--json", "query", "alpha"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["epoch8"].as_str().unwrap().len(), 8);
    assert_eq!(v["elided"], 0);
    let hits = v["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["path"], "a.rs");
    assert_eq!(hits[0]["line"], 1);
    assert_eq!(hits[0]["col"], 4);
    assert_eq!(hits[0]["text"], "fn alpha() {}");
}

/// `--budget` elides whole hits and reports the count, mirroring
/// `render_hits`'s own contract — pinned here at the CLI boundary so the
/// human elision line and the `--json` `elided` count agree with each
/// other for the same invocation shape.
#[test]
fn query_budget_elides_hits_and_reports_count_in_both_modes() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    let mut content = String::new();
    for i in 0..50 {
        content.push_str(&format!("hit {i}\n"));
    }
    std::fs::write(r.join("many.rs"), content).unwrap();

    let out = vc(r)
        .args(["--json", "query", "hit", "--budget", "5"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let elided = v["elided"].as_u64().unwrap();
    assert!(elided > 0, "expected some hits elided under a tiny budget");
    let hits = v["hits"].as_array().unwrap();
    assert_eq!(hits.len() as u64 + elided, 50);

    let out = vc(r)
        .args(["query", "hit", "--budget", "5"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains(&format!("… elided {elided} hits (budget)")));
}

/// `vc query id --symbol` finds a method by exact name. Human hit line:
/// `{path}:{start_line}: [{kind}] {signature}`, kind lowercase.
#[test]
fn query_symbol_mode_finds_a_method_by_exact_name() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(
        r.join("plan.rs"),
        "impl Plan {\n    pub fn id(&self) -> String { String::new() }\n}\n",
    )
    .unwrap();

    let out = vc(r)
        .args(["query", "id", "--symbol"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("plan.rs:2: [method]"),
        "expected a labeled method hit line, got: {text}"
    );
    assert!(
        text.contains("pub fn id(&self) -> String"),
        "expected the signature in the hit line, got: {text}"
    );
}

/// `--json` shape for symbol mode:
/// `{epoch8, hits: [{path, line, kind, signature, fuzzy_source?}], fuzzy, elided}`.
/// An exact match must not carry `fuzzy_source`.
#[test]
fn query_symbol_json_shape_carries_kind_signature_and_fuzzy() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(
        r.join("plan.rs"),
        "impl Plan {\n    pub fn id(&self) -> String { String::new() }\n}\n",
    )
    .unwrap();

    let out = vc(r)
        .args(["--json", "query", "id", "--symbol"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["fuzzy"], false);
    assert_eq!(v["elided"], 0);
    let hits = v["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["path"], "plan.rs");
    assert_eq!(hits[0]["line"], 2);
    assert_eq!(hits[0]["kind"], "method");
    assert!(
        hits[0]["signature"]
            .as_str()
            .unwrap()
            .contains("pub fn id(&self) -> String")
    );
    assert!(
        hits[0].get("fuzzy_source").is_none(),
        "an exact match must not carry fuzzy_source: {:?}",
        hits[0]
    );
}

/// `--symbol` + `--regex` together is a usage error (exit 2), routed
/// through the same `VcError`/`--json` envelope as any other refusal —
/// checked manually in `cmd_query` (not `clap`'s `conflicts_with`), same
/// pattern as `doctor --rollback --discard`.
#[test]
fn query_symbol_and_regex_together_is_usage_error() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn alpha() {}\n").unwrap();

    let assert = vc(r)
        .args(["query", "alpha", "--symbol", "--regex"])
        .assert()
        .failure()
        .code(2);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.starts_with("usage:"),
        "expected a 'usage:' prefixed refusal, got: {stderr}"
    );
}

/// A malformed file in scope is skipped, not fatal — the command still
/// succeeds and finds the good symbol, but surfaces a warning naming the
/// bad file on stderr (the same `CmdOutcome.warning` slot `apply`/`undo`
/// use for a non-fatal, surfaced-but-not-failing condition).
#[test]
fn query_symbol_mode_surfaces_a_warning_for_a_malformed_file_without_failing() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("good.rs"), "fn target() {}\n").unwrap();
    std::fs::write(
        r.join("bad.rs"),
        "this is not rust code at all, just prose.",
    )
    .unwrap();

    let assert = vc(r)
        .args(["query", "target", "--symbol"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("target"), "got: {stdout}");
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("warning:") && stderr.contains("bad.rs"),
        "got: {stderr}"
    );
}
