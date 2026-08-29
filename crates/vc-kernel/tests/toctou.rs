// toctou.rs — F27: symlinks, root escape, concurrent lock, replace-after-plan
//
// Adjustment note: the brief's `plan_one` helper and
// `target_replaced_by_symlink_after_plan_is_refused` call
// `Plan::build`/`.store`/`apply_plan` through `&r.to_path_buf()` even where
// `r` is already a `&Path` (or already a `PathBuf` referenced as `&r`).
// `clippy::unnecessary_to_owned` (part of `-D warnings`) flags exactly this
// shape — allocating an owned `PathBuf` solely to borrow it back down to
// `&Path`, when the original reference already coerces. Dropped the
// redundant `.to_path_buf()` calls; behavior is unchanged (`Plan::build`,
// `.store`, and `apply_plan` all take `&Path`, and `&PathBuf` /
// `&std::path::Path` deref-coerce to it either way).
//
// Undo-path additions below (`undo_target_replaced_by_symlink_is_refused`,
// `undo_refuses_when_lock_already_held`) are not in the brief — added per
// controller ruling to close the gap `apply::verify_current_matches_post`
// had relative to `apply::verify_files`: undo's hash-gate read every file
// with a plain `std::fs::read`, which follows symlinks, with no
// symlink/parent-escape check ahead of it. See task-12-report.md for the
// RED (pre-fix) / GREEN (post-fix) evidence.
use velocity_code_kernel::{
    ErrorKind, apply,
    lock::Lock,
    plan::{Plan, PlanForm},
    resolve,
};

fn plan_one(r: &std::path::Path, file: &str, old: &str, new: &str) -> String {
    let reqs = vec![resolve::EditRequest {
        path: file.into(),
        old: old.as_bytes().to_vec(),
        new: new.as_bytes().to_vec(),
        line_hint: None,
    }];
    Plan::build(r, PlanForm::Edit, &reqs)
        .unwrap()
        .store(r)
        .unwrap()
}

#[test]
fn target_replaced_by_symlink_after_plan_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    std::fs::create_dir_all(r.join(".vc")).unwrap();
    let victim = d.path().join("outside.txt");
    std::fs::write(&victim, "outside").unwrap();
    std::fs::write(r.join("t.rs"), "inside").unwrap();
    let sha8 = plan_one(r, "t.rs", "inside", "PWNED");
    std::fs::remove_file(r.join("t.rs")).unwrap();
    std::os::unix::fs::symlink(&victim, r.join("t.rs")).unwrap();
    let err = apply::apply_plan(r, &sha8).unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Toctou | ErrorKind::Stale));
    assert_eq!(
        std::fs::read_to_string(&victim).unwrap(),
        "outside",
        "file outside root untouched"
    );
}

#[test]
fn concurrent_apply_is_serialized_by_lock() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path().to_path_buf();
    std::fs::create_dir_all(r.join(".vc/journal")).unwrap();
    std::fs::write(r.join("a.rs"), "x").unwrap();
    let sha8 = plan_one(&r, "a.rs", "x", "y");
    let _held = Lock::acquire(&r).unwrap();
    let err = apply::apply_plan(&r, &sha8).unwrap_err();
    assert!(matches!(err.kind, ErrorKind::JournalBlocked));
    assert_eq!(err.exit_code(), 5);
}

#[test]
fn hardlinked_target_still_hash_gated() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path().to_path_buf();
    std::fs::create_dir_all(r.join(".vc")).unwrap();
    std::fs::write(r.join("a.rs"), "same").unwrap();
    std::fs::hard_link(r.join("a.rs"), r.join("b.rs")).unwrap();
    let sha8 = plan_one(&r, "a.rs", "same", "edited");
    apply::apply_plan(&r, &sha8).unwrap();
    // rename-into-place breaks the hardlink: b.rs must retain original content
    assert_eq!(std::fs::read_to_string(r.join("b.rs")).unwrap(), "same");
}

/// Mirrors `target_replaced_by_symlink_after_plan_is_refused`, but for
/// `undo`. Design note: a symlink pointing at a file with *different*
/// content (as in the apply-path test above) is refused either way here —
/// even with zero symlink-specific guard, `undo`'s hash-gate reads through
/// the symlink, hashes what it finds, sees a mismatch against the
/// journaled `post_hash`, and refuses as plain `Stale`. That would make
/// this test pass regardless of whether the guard exists, proving nothing.
/// So the decoy this test plants holds bytes IDENTICAL to the post-apply
/// content: the one construction where a hash-only check is defeated by
/// design, and only a check that refuses to follow the symlink at all
/// (never mind what it points to, never mind whether the bytes match) can
/// catch it. That is exactly what `verify_files` checks (a)/(b) exist to
/// do independently of check (c)'s hash comparison — this test is what
/// proves `undo` needs the same two checks.
#[cfg(unix)]
#[test]
fn undo_target_replaced_by_symlink_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path().join("repo");
    std::fs::create_dir_all(r.join(".vc")).unwrap();
    // `victim` is a sibling of `r`, not a descendant — genuinely outside
    // the vc root, not just a same-directory decoy.
    let victim = d.path().join("outside.txt");
    std::fs::write(r.join("t.rs"), "inside").unwrap();

    let sha8 = plan_one(&r, "t.rs", "inside", "PWNED");
    apply::apply_plan(&r, &sha8).unwrap();
    assert_eq!(std::fs::read_to_string(r.join("t.rs")).unwrap(), "PWNED");

    // Plant a decoy outside root holding byte-identical content to the
    // post-apply state, then swap the tracked file for a symlink to it.
    std::fs::write(&victim, "PWNED").unwrap();
    std::fs::remove_file(r.join("t.rs")).unwrap();
    std::os::unix::fs::symlink(&victim, r.join("t.rs")).unwrap();

    let err = apply::undo(&r, None).unwrap_err();
    assert!(
        matches!(err.kind, ErrorKind::Toctou),
        "expected Toctou (the hash-only fallback cannot fire here): {:?}",
        err.kind
    );
    assert_eq!(
        std::fs::read_to_string(&victim).unwrap(),
        "PWNED",
        "file outside root untouched"
    );
}

/// Mirrors `concurrent_apply_is_serialized_by_lock`, but for `undo`:
/// `apply::undo` acquires the same `.vc/journal/LOCK` as `apply_plan`, as
/// its very first step, before it even resolves which journal entry to
/// target — so a held lock must block it exactly the same way.
#[test]
fn undo_refuses_when_lock_already_held() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path().to_path_buf();
    std::fs::create_dir_all(r.join(".vc/journal")).unwrap();
    std::fs::write(r.join("a.rs"), "x").unwrap();
    let sha8 = plan_one(&r, "a.rs", "x", "y");
    apply::apply_plan(&r, &sha8).unwrap(); // leaves a committed entry to undo; lock released on return

    let _held = Lock::acquire(&r).unwrap();
    let err = apply::undo(&r, None).unwrap_err();
    assert!(matches!(err.kind, ErrorKind::JournalBlocked));
    assert_eq!(err.exit_code(), 5);
}
