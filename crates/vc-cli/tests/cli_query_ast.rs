//! `vc query PATTERN --ast` — structural (AST) query mode, the read-only
//! twin of `vc plan match`'s matcher (Task 15). `--ast` runs the same
//! `ast-grep` engine over an empty rewrite and renders each site as a
//! query hit at its start line, through the same render_hits/budget/
//! epoch-header/`--json` path literal and regex query modes use.

use assert_cmd::Command;

fn vc(dir: &std::path::Path) -> Command {
    let mut c = Command::cargo_bin("vc").unwrap();
    c.current_dir(dir);
    c
}

/// `vc query 'fetch_config($$$A)' --ast` finds the one call site and
/// renders it at its true start line/col, with the matched text as the
/// hit's line text — same fixture/pattern `plan_match_reports_true_site_count`
/// (cli_match.rs) uses for the matcher itself.
#[test]
fn ast_mode_finds_pattern_site_at_correct_line() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn main() {\n    fetch_config(a);\n}\n").unwrap();

    let out = vc(r)
        .args(["query", "fetch_config($$$A)", "--ast"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    let mut lines = text.lines();
    let header = lines.next().unwrap();
    assert!(
        header.ends_with(" — 1 hits"),
        "expected exactly one hit: {header}"
    );
    let rest: Vec<&str> = lines.collect();
    assert_eq!(rest, vec!["a.rs:2:5:     fetch_config(a);"]);
}

/// Two call sites in one file, both found, sorted by position.
#[test]
fn ast_mode_finds_every_call_site() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(
        r.join("a.rs"),
        "fn main() { fetch_config(a); fetch_config(b); }\n",
    )
    .unwrap();

    let out = vc(r)
        .args(["--json", "query", "fetch_config($$$A)", "--ast"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["elided"], 0);
    let hits = v["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0]["path"], "a.rs");
    assert_eq!(hits[0]["line"], 1);
    assert_eq!(
        hits[0]["text"],
        "fn main() { fetch_config(a); fetch_config(b); }"
    );
}

// `--ast` against `--regex`/`--symbol` is covered with every other mode
// pairing by `query_mode_flags_are_mutually_exclusive_in_every_pairing`
// (cli_query.rs) — one loop over the pairs, rather than a near-identical
// test per pair here.

/// Zero matches under `--ast` is still success (exit 0), same "found
/// nothing" vs "the command failed" contract every other query mode has.
#[test]
fn ast_mode_zero_hits_exits_zero() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn main() {}\n").unwrap();

    let out = vc(r)
        .args(["query", "fetch_config($$$A)", "--ast"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("0 hits"), "got: {text}");
}

/// An explicit `--lang` pins the language the same way `plan match`'s
/// does, instead of relying on auto-detect.
#[test]
fn ast_mode_accepts_explicit_lang() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::write(r.join("a.rs"), "fn main() { fetch_config(a); }\n").unwrap();

    vc(r)
        .args(["query", "fetch_config($$$A)", "--ast", "--lang", "rust"])
        .assert()
        .success();
}
