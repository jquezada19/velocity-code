// crates/vc-select/tests/astgrep_spike.rs
// The Task 1 spike, now driving the real API.
//
// The spike originally carried its own `vc_spike` module: a hand-rolled
// `ast_grep_core::Language` impl over tree-sitter-rust plus a
// `match_and_rewrite` helper. That module has been folded into
// `velocity_code_select::matcher` (Task 11) — including the finding the
// spike existed to make, that `$$$A` sends tree-sitter-rust into an ERROR
// node unless the `Language` impl overrides `expando_char` /
// `pre_process_pattern` to substitute a grammar-legal identifier lead (µ).
//
// What survives here is the spike's assertion, unchanged in substance and
// now checked against the production entry point: pattern
// `fetch_config($$$A)`, rewrite `load_config($$$A)`, exactly one match, the
// multi-argument metavariable carried through, and the rest of the file
// untouched. The one shape change is the spike's known limitation: it
// reported `Match::replacement` as the whole rewritten document, whereas a
// `MatchSite` carries only this match's span and this match's rewrite
// bytes — so the document-level assertion is reconstructed by splicing the
// site into the original buffer, which is what `vc apply` will do.

use std::path::PathBuf;

use velocity_code_select::match_sites;

#[test]
fn astgrep_matches_and_rewrites_with_metavariable() {
    let src = "fn main() { fetch_config(a, b); other(); }";
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("main.rs"), src).unwrap();

    let (sites, content, warnings) = match_sites(
        d.path(),
        "fetch_config($$$A)",
        "load_config($$$A)",
        "rust",
        &[PathBuf::from("main.rs")],
    )
    .expect("a valid pattern over parseable rust must not error");

    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    assert_eq!(sites.len(), 1);

    // Byte exactness — the property the spike could not check, because it
    // never produced a per-match span.
    let buf = &content[&PathBuf::from("main.rs")];
    assert_eq!(buf, src.as_bytes());
    assert_eq!(&buf[sites[0].start..sites[0].end], &sites[0].old[..]);
    assert_eq!(sites[0].old, b"fetch_config(a, b)");
    assert_eq!(sites[0].new, b"load_config(a, b)");

    // The spike's original document-level assertion, reconstructed from the
    // span.
    let mut rewritten = Vec::new();
    rewritten.extend_from_slice(&buf[..sites[0].start]);
    rewritten.extend_from_slice(&sites[0].new);
    rewritten.extend_from_slice(&buf[sites[0].end..]);
    let rewritten = String::from_utf8(rewritten).unwrap();
    assert!(rewritten.contains("load_config(a, b)"));
    assert!(rewritten.contains("other();"));
}
