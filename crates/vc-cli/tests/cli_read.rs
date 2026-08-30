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
/// `Usage`, exit 2. The remedy rides in `next:` per the error grammar, and
/// names the actual file — a command the caller can copy and run, rather
/// than prose telling them to work it out ("vc read the file instead").
#[test]
fn outline_unsupported_language_is_usage_error_with_a_runnable_next_hint() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("notes.txt"), "hello\n").unwrap();

    let assert = vc(r)
        .args(["outline", "notes.txt"])
        .assert()
        .failure()
        .code(2);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert_eq!(
        stderr.trim_end(),
        "usage: outline: unsupported language — next: vc read notes.txt",
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

/// A range whose START is beyond EOF is unsatisfiable, not clampable —
/// `NotFound` (exit 1), not a silent empty "success" with an inverted
/// start>end range (fix round 1, controller ruling).
#[test]
fn read_range_start_beyond_eof_is_not_found_not_silent_empty_success() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    let content = (1..=10).map(|i| format!("line {i}\n")).collect::<String>();
    std::fs::write(r.join("file.txt"), content).unwrap();

    let assert = vc(r)
        .args(["read", "file.txt:11-12"])
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("not-found:"), "got: {stderr}");
    assert!(
        stderr.contains("start beyond EOF (10 lines)"),
        "got: {stderr}"
    );
    assert!(
        stderr.contains("next: vc read file.txt:1-10"),
        "got: {stderr}"
    );
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

/// A FUZZY-only result refuses instead of serving a different function's
/// body. `search_symbol` falls back to a case-insensitive substring tier
/// when nothing matches the name exactly, and `read` discarded that flag —
/// so `vc read --symbol load_config`, in a tree with no `load_config` at
/// all, printed `load_configuration_from_disk`'s body at exit 0, with
/// nothing in the output saying it was a different function.
///
/// The refusal still has to be useful: the near-misses are listed as
/// `path:line name`, and `next:` points at the verb that may answer
/// fuzzily.
#[test]
fn read_symbol_fuzzy_only_match_refuses_instead_of_serving_a_near_miss() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(
        r.join("config.rs"),
        "fn load_configuration_from_disk() {\n    let v = 1;\n}\n",
    )
    .unwrap();

    let assert = vc(r)
        .args(["read", "--symbol", "load_config"])
        .assert()
        .failure()
        .code(1);
    let out = assert.get_output();
    let stderr = String::from_utf8(out.stderr.clone()).unwrap();
    let stdout = String::from_utf8(out.stdout.clone()).unwrap();

    assert!(stderr.starts_with("not-found:"), "stderr: {stderr}");
    assert!(
        stderr.contains("load_config:"),
        "the queried name must be named: {stderr}"
    );
    assert!(
        stderr.contains("config.rs:1 load_configuration_from_disk"),
        "candidates must be listed as `path:line name`: {stderr}"
    );
    assert!(
        stderr.contains("next: vc query load_config --symbol"),
        "stderr: {stderr}"
    );
    assert!(
        !stdout.contains("let v = 1"),
        "the near-miss body must never be served: {stdout}"
    );
}

/// The control for the refusal above: an EXACT match still reads normally.
/// The fuzzy guard must not cost the ordinary case anything — here the
/// same tree also holds a longer name containing the query as a substring,
/// so the exact tier is what has to win.
#[test]
fn read_symbol_exact_match_still_reads_even_with_fuzzy_neighbours() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(
        r.join("config.rs"),
        "fn load_config() {\n    let exact = 1;\n}\nfn load_config_from_disk() {\n    let near = 2;\n}\n",
    )
    .unwrap();

    let out = vc(r)
        .args(["read", "--symbol", "load_config"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("let exact = 1"), "got: {text}");
    assert!(
        !text.contains("let near = 2"),
        "the fuzzy neighbour must not be included: {text}"
    );
}

/// CRLF line endings, pinned as the documented behaviour rather than left
/// to be discovered: `read` is line-oriented — it splits with `str::lines`,
/// which strips a trailing `\r` along with the `\n` — so a CRLF file comes
/// back with `\n` endings and no carriage returns. `vc read` is not a
/// byte-for-byte `cat`.
#[test]
fn read_of_a_crlf_file_returns_lines_with_carriage_returns_stripped() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("crlf.txt"), "one\r\ntwo\r\nthree\r\n").unwrap();

    let out = vc(r)
        .args(["--json", "read", "crlf.txt"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["start"], 1);
    assert_eq!(v["end"], 3);
    assert_eq!(
        v["text"], "1: one\n2: two\n3: three",
        "lines are returned \\r-stripped, joined with \\n"
    );
    assert!(
        !v["text"].as_str().unwrap().contains('\r'),
        "no carriage return survives the line-oriented read"
    );
}

/// A file whose last line has no trailing newline: that line is still a
/// line, returned intact and counted. `str::lines` yields it, so the
/// final line is neither dropped nor merged with its predecessor.
#[test]
fn read_of_a_file_without_a_trailing_newline_keeps_the_last_line() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("no_eol.txt"), "one\ntwo\nlast line, no newline").unwrap();

    let out = vc(r)
        .args(["--json", "read", "no_eol.txt"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["end"], 3, "the unterminated last line still counts");
    assert_eq!(v["text"], "1: one\n2: two\n3: last line, no newline");
}

/// The budget pre-check added for bounded reads gates a WHOLE-FILE read on
/// the file's raw size, before the file is read. It must not leak into a
/// RANGE read: a small range out of a large file is a legitimate request,
/// and the range's own rendered text is what the budget applies to.
#[test]
fn budget_gates_a_whole_file_read_but_not_a_small_range_of_a_large_one() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    let mut content = String::new();
    for i in 0..500 {
        content.push_str(&format!("line number {i} has some real content in it\n"));
    }
    std::fs::write(r.join("big.txt"), content).unwrap();

    // Whole file (~5900 tokens), budget 50 -> refuses.
    vc(r)
        .args(["read", "big.txt", "--budget", "50"])
        .assert()
        .failure()
        .code(1);

    // Two lines (~24 tokens) out of the same file, SAME budget -> succeeds.
    let out = vc(r)
        .args(["--json", "read", "big.txt:1-2", "--budget", "50"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["start"], 1);
    assert_eq!(v["end"], 2);
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
