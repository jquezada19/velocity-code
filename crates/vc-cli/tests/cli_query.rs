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

/// The three search modes are mutually exclusive with each other, in
/// every pairing — one loop over the pairs rather than one near-identical
/// test per pair, which is how a fourth mode would arrive with two of its
/// three pairings untested. Checked manually in `cmd_query` (not `clap`'s
/// `conflicts_with`) so each refusal routes through the same
/// `VcError`/`--json` envelope as any other, same pattern as `doctor
/// --rollback --discard`.
#[test]
fn query_mode_flags_are_mutually_exclusive_in_every_pairing() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn main() { fetch_config(a); }\n").unwrap();

    for pair in [
        ["--symbol", "--regex"],
        ["--ast", "--regex"],
        ["--ast", "--symbol"],
    ] {
        let assert = vc(r)
            .args(["query", "fetch_config($$$A)", pair[0], pair[1]])
            .assert()
            .failure()
            .code(2);
        let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
        assert!(
            stderr.starts_with("usage:"),
            "{pair:?}: expected a 'usage:' prefixed refusal, got: {stderr}"
        );
        assert!(
            stderr.contains("mutually exclusive"),
            "{pair:?}: got: {stderr}"
        );
    }
}

/// An empty pattern matches at every byte position, so `vc query ""` built
/// one hit per byte of the tree — materialized in full, because `--budget`
/// only trims at render time. Both content modes refuse it up front, and
/// the regex mode refuses every pattern that matches the empty string, not
/// just the literally-empty one.
#[test]
fn empty_and_empty_matching_patterns_are_refused() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn alpha() {}\n").unwrap();

    let assert = vc(r).args(["query", ""]).assert().failure().code(2);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.starts_with("usage:"), "got: {stderr}");
    assert!(stderr.contains("empty pattern"), "got: {stderr}");

    for pattern in ["", "a*"] {
        let assert = vc(r)
            .args(["query", pattern, "--regex"])
            .assert()
            .failure()
            .code(2);
        let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
        assert!(
            stderr.contains("empty string"),
            "{pattern:?}: got: {stderr}"
        );
    }
}

/// A `--symbol` result that came only from the fuzzy substring tier is
/// marked as such in the HUMAN header. `--json` has always carried the
/// `fuzzy` flag; the human line said a plain "N hits" for a result
/// containing no exact match at all.
#[test]
fn query_symbol_human_header_marks_a_fuzzy_only_result() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn load_configuration() {}\n").unwrap();

    let out = vc(r)
        .args(["query", "load_config", "--symbol"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    let header = text.lines().next().unwrap();
    assert!(
        header.ends_with(" — 1 hits (fuzzy: no exact match)"),
        "header must mark the fuzzy tier: {header}"
    );

    // The control: an exact hit carries no marker.
    std::fs::write(r.join("a.rs"), "fn load_config() {}\n").unwrap();
    let out = vc(r)
        .args(["query", "load_config", "--symbol"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    let header = text.lines().next().unwrap();
    assert!(
        header.ends_with(" — 1 hits"),
        "an exact result must not be marked fuzzy: {header}"
    );
}

/// A file the literal/regex search skipped is surfaced as a warning on
/// stderr, not dropped in silence — the CLI half of the contract, joined
/// into the same `CmdOutcome.warning` slot `--symbol` and `--ast` already
/// use. Before this, the two content modes returned a short answer with
/// nothing saying it was short, while the README claimed no file is ever
/// skipped silently.
///
/// The oversized-file skip is what's exercised here: an *unreadable* file
/// never reaches the search from the CLI, because `index::refresh` hashes
/// every walked file first and fails the command outright — that path is
/// covered at library level in `vc-query`'s own tests, where no index
/// refresh intervenes.
#[test]
fn query_content_modes_warn_about_a_skipped_file() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("small.rs"), "fn alpha() {}\n").unwrap();
    // Sparse, so it costs no real disk; the size gate reads `metadata`,
    // never the body.
    let big = std::fs::File::create(r.join("big.rs")).unwrap();
    big.set_len(velocity_code_query::MAX_SEARCH_FILE_BYTES + 1)
        .unwrap();
    drop(big);

    for mode in [vec!["query", "alpha"], vec!["query", "alpha", "--regex"]] {
        let assert = vc(r).args(&mode).assert().success();
        let out = assert.get_output();
        let stdout = String::from_utf8(out.stdout.clone()).unwrap();
        let stderr = String::from_utf8(out.stderr.clone()).unwrap();
        assert!(
            stdout.contains("1 hits"),
            "{mode:?}: the searchable file still answers: {stdout}"
        );
        assert!(
            stderr.contains("warning:") && stderr.contains("big.rs"),
            "{mode:?}: the skip must be surfaced: {stderr}"
        );
    }
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

/// `--lang` names the grammar the structural matcher parses with, so it
/// is meaningless off `--ast`. It used to be accepted and ignored
/// entirely — even `--lang bogus` — so a caller who believed they had
/// constrained the search got an unconstrained one with nothing saying
/// so. Refuse (`usage`, exit 2) instead.
#[test]
fn lang_without_ast_is_refused_as_ast_only() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn alpha() {}\n").unwrap();

    for extra in [vec![], vec!["--regex"], vec!["--symbol"]] {
        let mut args = vec!["query", "alpha"];
        args.extend(extra.iter().copied());
        args.extend(["--lang", "rust"]);

        let out = vc(r)
            .args(&args)
            .assert()
            .code(2)
            .get_output()
            .stderr
            .clone();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.starts_with("usage: ") && text.contains("--lang applies only to --ast"),
            "args {args:?}: {text}"
        );
    }

    // A bogus --lang is refused the same way rather than silently ignored.
    vc(r)
        .args(["query", "alpha", "--lang", "not-a-language"])
        .assert()
        .code(2);

    // The control: --ast + --lang still works.
    vc(r)
        .args(["query", "alpha($$$A)", "--ast", "--lang", "rust"])
        .assert()
        .success();
}

/// An empty `--symbol` name falls through the exact tier into the fuzzy
/// tier, where `contains("")` is true of every symbol in the tree — the
/// same materialize-everything shape an empty content pattern already
/// refuses, arriving through the symbol door. Both verbs refuse it.
#[test]
fn an_empty_symbol_name_is_refused_by_query_and_read() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();

    for name in ["", "   "] {
        let out = vc(r)
            .args(["query", "--symbol", "--", name])
            .assert()
            .code(2)
            .get_output()
            .stderr
            .clone();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("usage: "), "name {name:?}: {text}");

        vc(r).args(["read", "--symbol", name]).assert().code(2);
    }

    // The control: a real name still resolves.
    vc(r)
        .args(["query", "alpha", "--symbol"])
        .assert()
        .success();
}
