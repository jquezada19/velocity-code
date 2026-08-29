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
