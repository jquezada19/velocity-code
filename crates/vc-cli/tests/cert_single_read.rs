//! The query-provenance certificate's single-read discipline, exercised
//! against the real matcher.
//!
//! `vc plan match` reads its scope exactly once — in the match pass — and
//! the certificate is hashed from precisely those bytes. This suite
//! reproduces, deterministically, the race that a second read would open:
//! a file mutated *between* the match pass and the certificate walk.
//!
//! Why this lives in `vc-cli` and not in either crate it exercises:
//! `vc-select` (which reads) and `vc-kernel` (which certifies) do not
//! depend on each other — `vc-kernel` deliberately knows nothing about
//! `MatchSite`. `vc-cli` is the one crate that depends on both, so it is
//! the only place the two halves can be composed the way `plan match`
//! composes them.

use std::collections::BTreeMap;
use std::path::PathBuf;

use velocity_code_kernel::plan::{MatchSelector, Plan};
use velocity_code_select::match_sites;

fn selector() -> MatchSelector {
    MatchSelector {
        pattern: "fetch_config($$$A)".to_string(),
        rewrite: "load_config($$$A)".to_string(),
        lang: "rust".to_string(),
        paths: vec![],
    }
}

/// The flagship race, made deterministic.
///
/// `a.rs` matches and is edited; `b.rs` is in scope and matches nothing.
/// Between the match pass and `Plan::build_match`, `b.rs` is rewritten on
/// disk — standing in for a concurrent write landing in exactly that
/// window. The certificate must baseline `b.rs` at what the MATCH PASS
/// read, so the apply-time scope-drift check (which compares the current
/// tree against this baseline) sees the file as changed and refuses.
///
/// Under the old two-read design this fails: `b.rs` produced no site, so
/// it was absent from the matcher's content map, so `build_match` read it
/// fresh — baselining the post-mutation bytes. A file that changed after
/// the plan was made would then hash EQUAL to its own certificate entry,
/// the drift check would clear it, and the flagship refusal would silently
/// become exit 0.
#[test]
fn certificate_baselines_the_match_pass_read_not_a_later_one() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::create_dir_all(r.join(".vc")).unwrap();
    std::fs::write(r.join("a.rs"), "fn main() { fetch_config(a); }\n").unwrap();
    let b_at_match_time = "fn other() {}\n";
    std::fs::write(r.join("b.rs"), b_at_match_time).unwrap();

    // 1. The match pass — the ONE read of the scope.
    let scope = vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")];
    let (sites, content_by_path, warnings) =
        match_sites(r, "fetch_config($$$A)", "load_config($$$A)", "rust", &scope).unwrap();
    assert_eq!(sites.len(), 1, "only a.rs matches");
    assert!(warnings.is_empty());

    // 2. The concurrent write, landing in the window a second read would
    //    have observed.
    let b_after = "fn other() { changed_between_the_two_reads(); }\n";
    std::fs::write(r.join("b.rs"), b_after).unwrap();

    // 3. The certificate walk.
    let edits = sites
        .into_iter()
        .map(|s| velocity_code_kernel::plan::ResolvedEdit {
            path: s.path,
            start: s.start,
            end: s.end,
            old_b64: velocity_code_kernel::plan::b64e(&s.old),
            new_b64: velocity_code_kernel::plan::b64e(&s.new),
            line_hint: None,
        })
        .collect();
    let content: BTreeMap<PathBuf, Vec<u8>> = content_by_path.into_iter().collect();
    let plan = Plan::build_match(r, selector(), edits, &content, warnings).unwrap();

    let cert = plan
        .certificate
        .expect("a match plan carries a certificate");
    let b = PathBuf::from("b.rs");
    assert_eq!(
        cert.scope_files.get(&b),
        Some(&velocity_code_kernel::hash::bytes_hash(
            b_at_match_time.as_bytes()
        )),
        "the certificate must baseline what the match pass read"
    );
    assert_ne!(
        cert.scope_files.get(&b),
        Some(&velocity_code_kernel::hash::bytes_hash(b_after.as_bytes())),
        "baselining the post-mutation bytes is exactly the race: the drift \
         check would then find b.rs unchanged and let the apply through"
    );
}

/// The other end of the same guarantee, at the CLI: an unmatched in-scope
/// file that changes AFTER the plan is a file the drift check can still
/// see, because the certificate recorded the pre-change bytes. (`b.rs`
/// gains a match, which is the observable refusal; the certificate having
/// baselined it correctly is what makes the comparison meaningful.)
#[test]
fn unmatched_scope_file_is_covered_by_the_certificate() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::create_dir_all(r.join(".vc")).unwrap();
    std::fs::write(r.join("a.rs"), "fn main() { fetch_config(a); }\n").unwrap();
    std::fs::write(r.join("b.rs"), "fn other() {}\n").unwrap();

    let scope = vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")];
    let (sites, content_by_path, warnings) =
        match_sites(r, "fetch_config($$$A)", "load_config($$$A)", "rust", &scope).unwrap();

    let edits = sites
        .into_iter()
        .map(|s| velocity_code_kernel::plan::ResolvedEdit {
            path: s.path,
            start: s.start,
            end: s.end,
            old_b64: velocity_code_kernel::plan::b64e(&s.old),
            new_b64: velocity_code_kernel::plan::b64e(&s.new),
            line_hint: None,
        })
        .collect();
    let content: BTreeMap<PathBuf, Vec<u8>> = content_by_path.into_iter().collect();
    let plan = Plan::build_match(r, selector(), edits, &content, warnings).unwrap();

    let cert = plan.certificate.unwrap();
    assert!(
        cert.scope_files.contains_key(&PathBuf::from("b.rs")),
        "an in-scope file that matched nothing is still certified"
    );
    assert!(
        !plan.files.contains_key(&PathBuf::from("b.rs")),
        "...and is still outside the plan's NAMED set, so only the drift \
         check can catch it — never the kernel's stale check"
    );
}
