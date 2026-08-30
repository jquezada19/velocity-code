//! End-to-end: a `query`/`outline`/`read` invocation's metrics line
//! carries the Task 15 read-side gain fields (`bytes_out`/`naive_bytes`),
//! and `vc gain` aggregates them into the `read savings: ...` line (spec
//! §7.2's read-side counterfactual).

use assert_cmd::Command;

fn vc(dir: &std::path::Path) -> Command {
    let mut c = Command::cargo_bin("vc").unwrap();
    c.current_dir(dir);
    c
}

fn last_metrics_line(root: &std::path::Path) -> serde_json::Value {
    let dir = root.join(".vc/metrics");
    let entry = std::fs::read_dir(&dir)
        .unwrap()
        .next()
        .expect("expected at least one metrics file")
        .unwrap();
    let content = std::fs::read_to_string(entry.path()).unwrap();
    let last = content.lines().next_back().unwrap();
    serde_json::from_str(last).unwrap()
}

#[test]
fn query_invocation_writes_bytes_out_and_naive_bytes() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn alpha() {}\n").unwrap();

    vc(r).args(["query", "alpha"]).assert().success();

    let line = last_metrics_line(r);
    assert_eq!(line["verb"], "query");
    assert!(line["bytes_out"].as_u64().is_some(), "got: {line}");
    assert!(line["naive_bytes"].as_u64().is_some(), "got: {line}");
    // naive_bytes is the full on-disk size of every file that
    // contributed a hit — here just a.rs's own byte length.
    let file_len = std::fs::metadata(r.join("a.rs")).unwrap().len();
    assert_eq!(line["naive_bytes"].as_u64().unwrap(), file_len);
}

#[test]
fn outline_and_read_invocations_also_write_read_gain_fields() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn alpha() {\n    1\n}\n").unwrap();

    vc(r).args(["outline", "a.rs"]).assert().success();
    let outline_line = last_metrics_line(r);
    assert_eq!(outline_line["verb"], "outline");
    assert!(outline_line["bytes_out"].as_u64().is_some());
    assert!(outline_line["naive_bytes"].as_u64().is_some());

    vc(r).args(["read", "a.rs"]).assert().success();
    let read_line = last_metrics_line(r);
    assert_eq!(read_line["verb"], "read");
    assert!(read_line["bytes_out"].as_u64().is_some());
    assert!(read_line["naive_bytes"].as_u64().is_some());
}

/// A non-read verb (`status`) never sets either field — the metrics line
/// simply omits both keys (`skip_serializing_if`).
#[test]
fn non_read_verb_omits_read_gain_fields() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();

    vc(r).args(["status"]).assert().success();

    let line = last_metrics_line(r);
    assert_eq!(line["verb"], "status");
    assert!(line.get("bytes_out").is_none(), "got: {line}");
    assert!(line.get("naive_bytes").is_none(), "got: {line}");
}

/// `vc gain` aggregates the read-gain fields into a `read savings: ...`
/// line, both in human and `--json` mode.
#[test]
fn query_invocation_feeds_gain_read_savings_line() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn alpha() {}\n").unwrap();

    vc(r).args(["query", "alpha"]).assert().success();

    let out = vc(r)
        .args(["gain"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("read savings:") && text.contains("read-verb calls"),
        "got: {text}"
    );

    let out = vc(r)
        .args(["--json", "gain"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["read_savings"]["calls"], 1);
    assert!(v["read_savings"]["naive"].as_u64().unwrap() > 0);
}
